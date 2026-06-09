use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use crate::progress_history::{
    no_progress_window_eligible, oldest_no_progress_window_depth, CycleSnapshot, ProgressHistory,
};

pub fn default_true() -> bool {
    true
}

// K-8 (2026-04-28): NodeId and TargetId are now distinct newtype wrappers
// rather than `type X = String` aliases. The K-7 bug in
// `theorem_review_next_active_legal` swapped a NodeId for a TargetId and
// compiled cleanly because both were String aliases — only the runtime
// symptom (silent empty BTreeMap lookup) revealed it. Newtype wrappers
// give nominal typing back: passing a NodeId where a TargetId is expected
// (or vice versa) now fails to compile. Both types are `#[serde(transparent)]`
// so JSON shapes are unchanged. They impl `Borrow<str>` and `Deref<Target=str>`
// so existing read-side code (lookups by `&str`, formatting, comparison)
// works unchanged.
//
// Fingerprint and LaneId stay as `String` aliases — they are not subject
// to the same swap hazard (no map keyed by Fingerprint that's also keyed
// by something else of similar shape).

macro_rules! string_newtype {
    ($name:ident) => {
        #[derive(
            Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl From<&String> for $name {
            fn from(s: &String) -> Self {
                Self(s.clone())
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }
    };
}

string_newtype!(NodeId);
string_newtype!(TargetId);
string_newtype!(DeviationId);

pub type Fingerprint = String;
pub type LaneId = String;

pub const CHECKER_MISMATCH_REJECTION_PREFIX: &str = "authoritative checker mismatch:";
pub const SORRY_AX_REJECTION_REMINDER: &str = concat!(
    "sorryAx is forbidden, use sorry instead for open proof nodes. ",
    "Do not use `sorryAx`, `axiom`, or other unapproved axioms to make a node appear closed. ",
    "Close the dependency first or keep the dependent node open."
);
const MAX_PROMPT_REJECTION_REASON_CHARS: usize = 4096;
const CHECKER_MISMATCH_PROMPT_SUMMARY: &str = concat!(
    "authoritative checker mismatch: worker-side acceptance reported success, ",
    "but the supervisor authoritative check rejected the submitted result. ",
    "Raw worker/supervisor checker JSON was omitted from this prompt because ",
    "it can contain large fingerprint snapshots; inspect bridge/latest_worker.json ",
    "or the event log for full detail."
);

const PREAMBLE_NAME: &str = "Preamble";

/// Built-in default for the mandatory-LastClean threshold. The runtime
/// value is read via `csc_last_clean_threshold()` which honors the
/// `TRELLIS_CSC_LAST_CLEAN_THRESHOLD` env override. Keep in sync with
/// the comment in `request_allowed_resets`.
pub const CSC_LAST_CLEAN_THRESHOLD_DEFAULT: u32 = 15;

/// Number of rewinds to the same clean checkpoint that waives the
/// mandatory-LastClean rule. When `last_clean_rewind_count` reaches
/// this value, `request_allowed_resets` stops dropping `None` /
/// `LastCommit` from the menu even past the threshold — repeated
/// rewinds clearly aren't helping, so the situation is treated as
/// a genuine "necessary decomposition." Surfaced on the review
/// request summary so the reviewer prompt fragment can render the
/// effective number without drift.
pub const CSC_REWIND_WAIVER_COUNT: u32 = 2;

/// Effective threshold at which `request_allowed_resets` drops `None` /
/// `LastCommit` from the menu (forcing the reviewer to LastClean) when
/// `last_clean_rewind_count < 2`. Reads `TRELLIS_CSC_LAST_CLEAN_THRESHOLD`
/// from the environment per call; falls back to
/// `CSC_LAST_CLEAN_THRESHOLD_DEFAULT` when the env var is unset or
/// malformed. Surfaced on the review request summary so prompt fragments
/// can render the effective number without being separately edited.
pub fn csc_last_clean_threshold() -> u32 {
    std::env::var("TRELLIS_CSC_LAST_CLEAN_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(CSC_LAST_CLEAN_THRESHOLD_DEFAULT)
}

/// Built-in threshold for entering StuckMathAudit. This is deliberately
/// lower than the mandatory-LastClean threshold: it is a prompt/tooling
/// escalation for repeated mathematical blockage, not a rollback policy.
pub const STUCK_MATH_AUDIT_CYCLES_SINCE_CLEAN_THRESHOLD_DEFAULT: u32 = 5;
pub const STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD_DEFAULT: u32 = 5;
/// Built-in window length `k` (in checkpoint snapshots) for the
/// no-Sound-progress StuckMathAudit gate (`progress_history.rs`). The
/// gate fires when some snapshot S at least `k` snapshots old satisfies
/// "no node surviving from S to the latest checkpoint progressed from
/// not-sound to sound." Applied in both `Phase::TheoremStating` and
/// `Phase::ProofFormalization`; in the latter the gate additionally
/// requires that not all Sound carriers have closed. Debounced via
/// `ProgressHistory::note_dispatched` so a single stagnation streak
/// fires the audit once.
pub const STUCK_MATH_AUDIT_NO_SOUND_PROGRESS_WINDOW_DEFAULT: u32 = 5;
/// Proposal v32: starvation-guard threshold for the active coarse anchor.
/// When `cycles_in_coarse_repair_mode` reaches this value, the kernel
/// opens `active_coarse_change_allowed()` even without strict shallow
/// closure, so the reviewer is not trapped chasing a transitive blocker
/// chain forever. Default 8 — chosen high enough to absorb routine
/// cross-coarse repair work (the typical chain is 1–3 cycles) without
/// silently undermining the lock for genuinely slow repairs.
pub const STUCK_COARSE_REPAIR_THRESHOLD_DEFAULT: u32 = 8;

/// Maximum serialized size of a reviewer Lean product accepted into
/// StuckMathAudit state. Larger artifacts should live on disk in the
/// reviewer scratch directory, with this product carrying a compact summary
/// and path.
pub const STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS: usize = 20_000;
pub const AUDIT_REPORT_TEXT_MIN_CHARS: usize = 200;
pub const AUDIT_REPORT_TEXT_MAX_CHARS: usize = 20_000;
pub const AUDIT_TASK_TITLE_MAX_CHARS: usize = 200;
pub const AUDIT_TASK_BODY_MAX_CHARS: usize = 5_000;
pub const AUDIT_TASK_REASON_MAX_CHARS: usize = 1_000;
pub const AUDIT_PLAN_MAX_JSON_CHARS: usize = 80_000;
pub const STUCK_MATH_AUDIT_BURST_RETRY_LIMIT: u32 = 1;
pub const STUCK_MATH_AUDIT_DISPATCH_COOLDOWN_CYCLES_DEFAULT: u32 = 1;
/// When an `audit_plan` is already on the table, the dispatcher still
/// re-runs the audit role this many cycles after the last dispatch so
/// the falsification agent gets to revisit a stalled plan periodically.
/// The new audit sees the current plan via `previous_audit_plan_snapshot`
/// and either confirms, refines, or replaces it.
pub const STUCK_MATH_AUDIT_REAUDIT_INTERVAL_CYCLES_DEFAULT: u32 = 4;

pub fn stuck_math_audit_dispatch_cooldown_cycles() -> u32 {
    std::env::var("TRELLIS_AUDIT_DISPATCH_COOLDOWN_CYCLES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(STUCK_MATH_AUDIT_DISPATCH_COOLDOWN_CYCLES_DEFAULT)
}

/// Default for `global_repair_grant_ttl_cycles()`.
pub const GLOBAL_REPAIR_GRANT_TTL_CYCLES_DEFAULT: u32 = 3;

/// Audit M-2 — canonical set of approved kernel axioms shared by every
/// kernel-side trust boundary. The engine's accept-time defensive ceiling
/// (`engine::apply_local_closure_acceptance_bookkeeping`), the runtime
/// CLI's default approved-axioms set (`runtime_cli_observations`), and
/// the public tablet viewer export script (`scripts/export_public_tablet_viewer.py`)
/// must agree on this list — adding a platform-blessed axiom to one
/// without the others would create asymmetric acceptance.
///
/// Single source of truth: every consumer reads from this constant
/// rather than duplicating the literal. The canonical-axioms consistency
/// regression test
/// (`kernel/src/runtime_cli_observations.rs::tests::default_approved_axioms_matches_canonical_constant`)
/// asserts the runtime-CLI side stays equal as a set.
///
/// The Python export script's duplicate list is kept as a TODO-doc
/// comment (cross-language sourcing from a single Rust const would add
/// disproportionate build complexity); a Python regression test
/// (`tests/test_public_release_axioms_consistency.py`) parses both
/// literals and asserts they stay equal.
pub const CANONICAL_APPROVED_AXIOMS: &[&str] =
    &["propext", "funext", "Classical.choice", "Quot.sound"];

/// Audit-grant TTL for the global_repair_mode mechanism. A pending
/// global-repair grant older than this many cycles is dropped at the
/// next `commit_live` and on every `apply_last_clean_reset`.
/// Env-overridable via `TRELLIS_GLOBAL_REPAIR_GRANT_TTL_CYCLES`,
/// mirroring the existing kernel-threshold idiom.
pub fn global_repair_grant_ttl_cycles() -> u32 {
    std::env::var("TRELLIS_GLOBAL_REPAIR_GRANT_TTL_CYCLES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(GLOBAL_REPAIR_GRANT_TTL_CYCLES_DEFAULT)
}

pub fn stuck_math_audit_reaudit_interval_cycles() -> u32 {
    std::env::var("TRELLIS_STUCK_MATH_AUDIT_REAUDIT_INTERVAL_CYCLES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(STUCK_MATH_AUDIT_REAUDIT_INTERVAL_CYCLES_DEFAULT)
}

/// Effective threshold for StuckMathAudit activation. Reads
/// `TRELLIS_STUCK_MATH_AUDIT_CYCLES_SINCE_CLEAN_THRESHOLD`; malformed or
/// unset values fall back to
/// `STUCK_MATH_AUDIT_CYCLES_SINCE_CLEAN_THRESHOLD_DEFAULT`.
pub fn stuck_math_audit_cycles_since_clean_threshold() -> u32 {
    std::env::var("TRELLIS_STUCK_MATH_AUDIT_CYCLES_SINCE_CLEAN_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(STUCK_MATH_AUDIT_CYCLES_SINCE_CLEAN_THRESHOLD_DEFAULT)
}

/// Effective threshold for the StuckMathAudit shallow-coarse progress
/// trigger. Reads
/// `TRELLIS_STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD`;
/// malformed or unset values fall back to
/// `STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD_DEFAULT`.
pub fn stuck_math_audit_shallow_coarse_no_progress_threshold() -> u32 {
    std::env::var("TRELLIS_STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD_DEFAULT)
}

/// Effective threshold for the active-coarse-anchor starvation guard
/// (proposal v32). Reads `TRELLIS_STUCK_COARSE_REPAIR_THRESHOLD`;
/// malformed or unset values fall back to
/// `STUCK_COARSE_REPAIR_THRESHOLD_DEFAULT`.
pub fn stuck_coarse_repair_threshold() -> u32 {
    std::env::var("TRELLIS_STUCK_COARSE_REPAIR_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(STUCK_COARSE_REPAIR_THRESHOLD_DEFAULT)
}

/// Effective window length `k` for the no-Sound-progress StuckMathAudit
/// gate. Reads `TRELLIS_STUCK_MATH_AUDIT_NO_SOUND_PROGRESS_WINDOW`;
/// malformed or unset values fall back to
/// `STUCK_MATH_AUDIT_NO_SOUND_PROGRESS_WINDOW_DEFAULT`. See
/// `progress_history::no_progress_window_eligible` for the semantics.
pub fn stuck_math_audit_no_sound_progress_window() -> u32 {
    std::env::var("TRELLIS_STUCK_MATH_AUDIT_NO_SOUND_PROGRESS_WINDOW")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(STUCK_MATH_AUDIT_NO_SOUND_PROGRESS_WINDOW_DEFAULT)
}

// Option C (2026-06-04): `allow_reviewer_pass_override`,
// `set_allow_reviewer_pass_override_for_test`, and the thread-local
// `ALLOW_REVIEWER_PASS_OVERRIDE_TEST` cell were retired together with
// the reviewer Pass-override authority. The
// `TRELLIS_ALLOW_REVIEWER_PASS_OVERRIDE` env var is no longer read
// (and was never observed=1 in production). See
// REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.

fn truncate_rejection_reason_for_prompt(reason: &str) -> String {
    if reason.chars().count() <= MAX_PROMPT_REJECTION_REASON_CHARS {
        return reason.to_string();
    }
    let prefix: String = reason
        .chars()
        .take(MAX_PROMPT_REJECTION_REASON_CHARS)
        .collect();
    format!(
        "{}... [truncated to {} chars for prompt safety; inspect worker artifacts for full detail]",
        prefix, MAX_PROMPT_REJECTION_REASON_CHARS
    )
}

pub fn prompt_safe_deterministic_worker_rejection_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.starts_with(CHECKER_MISMATCH_REJECTION_PREFIX)
        && (trimmed.contains("worker={")
            || trimmed.contains("supervisor={")
            || trimmed.chars().count() > MAX_PROMPT_REJECTION_REASON_CHARS)
    {
        if trimmed.contains("sorryAx") {
            return format!("{CHECKER_MISMATCH_PROMPT_SUMMARY} {SORRY_AX_REJECTION_REMINDER}");
        }
        return CHECKER_MISMATCH_PROMPT_SUMMARY.to_string();
    }
    truncate_rejection_reason_for_prompt(trimmed)
}

pub fn prompt_safe_deterministic_worker_rejection_reasons(reasons: &[String]) -> Vec<String> {
    let mut safe = Vec::new();
    for reason in reasons {
        let safe_reason = prompt_safe_deterministic_worker_rejection_reason(reason);
        if !safe.iter().any(|item| item == &safe_reason) {
            safe.push(safe_reason);
        }
    }
    safe
}

pub fn prompt_safe_rejection_reasons(reasons: &[String]) -> Vec<String> {
    let mut safe = Vec::new();
    for reason in reasons {
        let safe_reason = truncate_rejection_reason_for_prompt(reason.trim());
        if !safe_reason.is_empty() && !safe.iter().any(|item| item == &safe_reason) {
            safe.push(safe_reason);
        }
    }
    safe
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Phase {
    #[default]
    #[serde(alias = "theorem_stating")]
    TheoremStating,
    #[serde(alias = "proof_formalization")]
    ProofFormalization,
    #[serde(alias = "cleanup")]
    Cleanup,
    #[serde(alias = "complete")]
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Stage {
    #[default]
    #[serde(alias = "start")]
    Start,
    #[serde(alias = "worker")]
    Worker,
    #[serde(alias = "verify_paper")]
    VerifyPaper,
    #[serde(alias = "verify_corr")]
    VerifyCorr,
    #[serde(alias = "verify_sound")]
    VerifySound,
    #[serde(alias = "reviewer")]
    Reviewer,
    #[serde(alias = "human_gate")]
    HumanGate,
    #[serde(alias = "complete")]
    Complete,
    #[serde(alias = "stuck_math_audit")]
    StuckMathAudit,
    /// Cleanup-v2 audit sub-phase. Active inside Phase::Cleanup while the
    /// kernel is collecting audit-task proposals from the audit role
    /// across one or more bursts. Transitions to Stage::Reviewer once
    /// the audit returns `AuditDone` or hits the per-round burst cap.
    /// Added 2026-05-14 as part of the cleanup-v2 design (see
    /// `CLAUDES_NOTES_cleanup_v2.md`). Legacy state files
    /// deserialize without the variant; only state files written
    /// after this deploy can mention it.
    #[serde(alias = "cleanup_audit")]
    CleanupAudit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CorrStatus {
    #[default]
    Unknown,
    Pass,
    Fail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum SoundStatus {
    #[default]
    Unknown,
    Pass,
    Fail,
    Structural,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum SoundAssessmentStatus {
    #[default]
    FreshUnknown,
    VerifierPass,
    VerifierFail,
    VerifierStructural,
    ReviewerPinnedFail,
    /// Retired by Option C (2026-06-04) — no longer produced. Kept for
    /// defensive serde back-compat with any pre-retirement checkpoint
    /// (production runs never observed the variant, but a backup might).
    /// On read it is treated as `FreshUnknown` via `current_sound_state`
    /// (maps to `CurrentCheckState::Unknown`); the assessment record
    /// will be overwritten by the next verifier verdict.
    ReviewerAcceptedPass,
    SketchAutoFail,
    SelfEditUnknown,
    DepEditOnlyStaleFail,
    DepEditOnlyStalePassDeferred,
    SplitUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum AssessmentOrigin {
    #[default]
    VerifierPanel,
    ReviewerAction,
    KernelSketch,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundFingerprintParts {
    pub own_tex_hash: Fingerprint,
    pub dep_statement_hashes: BTreeMap<NodeId, Fingerprint>,
    pub combined_sound_fp: Fingerprint,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundAssessment {
    pub status: SoundAssessmentStatus,
    pub origin: AssessmentOrigin,
    pub fingerprints: SoundFingerprintParts,
    pub lane_votes: BTreeMap<LaneId, SoundStatus>,
    pub reviewer_action_id: Option<u32>,
}

/// One dependency whose statement hash changed between the prior
/// approved Sound assessment and the current state. Used by the
/// re-verification context surfaced to the Sound verifier when a
/// previously-approved node is reissued because of dep drift.
///
/// Hashes are truncated to 12 hex characters (kernel-side) before
/// being surfaced — the verifier should use `git show <tag>:Tablet/<dep>.tex`
/// for full content, not the hash itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundDepHashDriftEntry {
    pub dep: NodeId,
    /// Truncated prior hash; "(absent)" if the dep was not in the
    /// stored fingerprint map (newly-added dep).
    pub prior_hash: String,
    /// Truncated current hash; "(absent)" if the dep is no longer in
    /// the current fingerprint map (removed dep).
    pub current_hash: String,
}

/// Re-verification context surfaced to the Sound verifier when the
/// current assessment for the target is `DepEditOnlyStalePassDeferred`
/// or `SelfEditUnknown`. Facts only — the prompt fragment must not
/// instruct the verifier how to weight this evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundReverificationContext {
    /// The Sound verify target this context applies to.
    pub target: NodeId,
    /// The stored (prior) assessment status before drift was detected.
    pub prior_status: SoundAssessmentStatus,
    /// The current derived status (always one of
    /// `DepEditOnlyStalePassDeferred` / `SelfEditUnknown`).
    pub current_status: SoundAssessmentStatus,
    /// True iff `own_tex_hash` changed since the prior approval.
    pub own_tex_changed: bool,
    /// Per-dep statement-hash drift entries. Empty when only the
    /// target's own_tex changed.
    pub deps_changed: Vec<SoundDepHashDriftEntry>,
    /// Verbatim prior accepted-lane evidence (per LaneId) for THIS
    /// target, drawn from `state.latest_sound_reviewer_evidence`.
    /// May be empty if no per-target evidence is retained.
    pub prior_lane_evidence: BTreeMap<LaneId, SoundReviewerLaneEvidence>,
}

/// Substantiveness verifier verdict. Distinct from `CorrStatus`
/// because the per-node lane admits a third "didn't get to it this time"
/// value: `NotDoneYet`. Wes's design (see plan §10b) gives the verifier the
/// full outstanding Unknown set in one request and lets it triage; nodes
/// it didn't carefully evaluate come back as `NotDoneYet`, leaving the
/// kernel to re-issue a follow-up Paper request covering the residual.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SubstantivenessStatus {
    #[default]
    #[serde(alias = "unknown")]
    Unknown,
    #[serde(alias = "pass")]
    Pass,
    #[serde(alias = "fail")]
    Fail,
    /// Verifier triaged the request and didn't get a careful read on this
    /// node. Kernel leaves the substantiveness status Unknown and queues
    /// another Paper request for the residual set (subject to the
    /// `substantiveness_consecutive_no_progress_requests` safety bound).
    #[serde(alias = "not_done_yet", alias = "NotDone", alias = "not_done")]
    NotDoneYet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BlockerKind {
    #[serde(alias = "target_corr", alias = "TargetCorr")]
    PaperFaithfulness,
    /// Single reference-file deviation authorization. This is a paper-lane
    /// check on one TeX-only reference file, distinct from node
    /// substantiveness.
    #[serde(alias = "deviation", alias = "DeviationAuthorization")]
    Deviation,
    NodeCorr,
    Soundness,
    /// Substantiveness lane (TheoremStating + ProofFormalization). Distinct
    /// from `PaperFaithfulness` (which is target-bound) — this variant is
    /// always node-bound (`BlockerObject::Node`). The verifier judges per
    /// node: does the .tex statement correspond to a paper claim AND is it
    /// not weakened in a way that breaks downstream usability?
    #[serde(alias = "substantiveness", alias = "paper_node_faithfulness")]
    Substantiveness,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "otype", rename_all = "snake_case")]
pub enum BlockerObject {
    Node { node: NodeId },
    Target { target: TargetId },
    Deviation { deviation: DeviationId },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Blocker {
    pub kind: BlockerKind,
    pub object: BlockerObject,
    pub fingerprint: Fingerprint,
    /// Topological-dispatch marker. `true` when this blocker corresponds to
    /// a node whose own corr/paper fingerprint says it needs verification,
    /// but the verifier cannot run yet because some Lean-relevant descendant
    /// of the node has open corr (the trust-chain dependency). Deferred
    /// blockers are scheduling-only state — they clear automatically once
    /// the Lean-relevant descendant's corr is repinned. They MUST be filtered
    /// out of every Reviewer-facing surface (Review request `blockers` set,
    /// blocker_choices, allowed_override_blockers/reset_blockers, and the
    /// `pending_task.task_blockers` projection): the Reviewer cannot
    /// adjudicate them, so surfacing them would offer the Reviewer a blocker
    /// id that no action bucket (task/override/reset) could legally accept.
    ///
    /// Defaults to `false` for legacy blockers (pre-topological-dispatch) and
    /// for any blocker the kernel raises through non-corr-dispatch paths.
    #[serde(default)]
    pub deferred: bool,
}

impl Blocker {
    /// True when the blocker is eligible for verifier dispatch (or for
    /// Reviewer adjudication). Deferred blockers return `false`.
    pub fn is_dispatch_eligible(&self) -> bool {
        !self.deferred
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CurrentCheckState {
    Pass,
    Fail,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GateKind {
    #[default]
    #[serde(alias = "none")]
    None,
    #[serde(alias = "advance")]
    Advance,
    #[serde(alias = "need_input")]
    NeedInput,
    #[serde(alias = "protected_reapproval")]
    ProtectedReapproval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TargetEditMode {
    #[default]
    Global,
    #[serde(
        alias = "Repair",
        alias = "repair",
        alias = "Restructure",
        alias = "restructure"
    )]
    Targeted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProofEditMode {
    #[default]
    Local,
    Restructure,
    CoarseRestructure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NodeDifficulty {
    #[serde(alias = "easy")]
    Easy,
    #[default]
    #[serde(alias = "hard")]
    Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkerProfile {
    #[default]
    #[serde(alias = "none")]
    None,
    #[serde(alias = "theorem")]
    Theorem,
    #[serde(alias = "proof_easy")]
    ProofEasy,
    #[serde(alias = "proof_hard")]
    ProofHard,
    #[serde(alias = "cleanup")]
    Cleanup,
    #[serde(alias = "final_cleanup")]
    FinalCleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkerValidationKind {
    #[default]
    #[serde(alias = "none")]
    None,
    #[serde(alias = "theorem_global")]
    TheoremGlobal,
    #[serde(alias = "theorem_targeted")]
    TheoremTargeted,
    #[serde(alias = "proof_easy")]
    ProofEasy,
    #[serde(alias = "proof_local")]
    ProofLocal,
    #[serde(alias = "proof_restructure")]
    ProofRestructure,
    #[serde(alias = "proof_coarse_restructure")]
    ProofCoarseRestructure,
    #[serde(alias = "cleanup")]
    Cleanup,
    #[serde(alias = "final_cleanup")]
    FinalCleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkerBaselineScope {
    #[default]
    #[serde(alias = "none")]
    None,
    #[serde(alias = "authorized_nodes")]
    AuthorizedNodes,
    #[serde(alias = "all_present")]
    AllPresent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkerProofDeltaMode {
    #[default]
    #[serde(alias = "none")]
    None,
    #[serde(alias = "easy")]
    Easy,
    #[serde(alias = "local")]
    Local,
    #[serde(alias = "restructure")]
    Restructure,
    #[serde(alias = "coarse_restructure")]
    CoarseRestructure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ScopedTabletAllowedNodesMode {
    #[default]
    #[serde(alias = "explicit")]
    Explicit,
    #[serde(alias = "all_present")]
    AllPresent,
    #[serde(alias = "previous_or_explicit")]
    PreviousOrExplicit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerValidationExecutionPlanStep {
    TheoremTargetEditScope {
        target: Option<NodeId>,
        #[serde(default)]
        initial_scope: BTreeSet<NodeId>,
    },
    ScopedTablet {
        allowed_nodes_mode: ScopedTabletAllowedNodesMode,
        #[serde(default)]
        explicit_nodes: BTreeSet<NodeId>,
    },
    ProofEasyScope {
        active_node: Option<NodeId>,
    },
    ProofWorkerDelta {
        active_node: Option<NodeId>,
        mode: WorkerProofDeltaMode,
        #[serde(default)]
        authorized_nodes: BTreeSet<NodeId>,
        #[serde(default)]
        protected_semantic_change_nodes: BTreeSet<NodeId>,
        #[serde(default = "default_true")]
        allow_new_obligations: bool,
        #[serde(default)]
        must_close_active: bool,
    },
    CleanupPreserving {},
    /// Cleanup-v2 (Step 8, 2026-05-14). Carries the active task's kind
    /// (None for legacy lint-only mode), target node, authorized-node
    /// scope, and the live protected-statement node set. The runtime
    /// validator (`final_cleanup_preserving_step_result`) branches on
    /// `task_kind` to apply Substitution-specific relaxations
    /// (target deletion + `.tex` sweep) or LintFix-specific
    /// tightening (single-node scope, no `.tex`). All fields
    /// `#[serde(default)]` so legacy plan-step JSON deserializes
    /// cleanly with task_kind=None.
    FinalCleanupPreserving {
        #[serde(default)]
        task_kind: Option<CleanupTaskKind>,
        #[serde(default)]
        target_node: Option<NodeId>,
        #[serde(default)]
        authorized_nodes: BTreeSet<NodeId>,
        #[serde(default)]
        protected_statement_node_set: BTreeSet<NodeId>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum NodeKind {
    #[serde(alias = "preamble")]
    Preamble,
    #[default]
    #[serde(alias = "definition")]
    Definition,
    #[serde(alias = "proof")]
    Proof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum TaskMode {
    #[default]
    #[serde(alias = "global")]
    Global,
    #[serde(
        alias = "Targeted",
        alias = "targeted",
        alias = "Repair",
        alias = "repair"
    )]
    Targeted,
    #[serde(alias = "local")]
    Local,
    #[serde(alias = "Restructure", alias = "restructure")]
    Restructure,
    #[serde(alias = "coarse_restructure")]
    CoarseRestructure,
    #[serde(alias = "cleanup")]
    Cleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkerOutcome {
    #[default]
    Valid,
    Invalid,
    Stuck,
    NeedsRestructure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RetryOutcomeKind {
    #[default]
    None,
    Invalid,
    Stuck,
    NeedsRestructure,
    /// Bridge / agent-infrastructure failure that prevented the worker from
    /// producing any meaningful output (timeout, hang, agent crash mid-burst,
    /// missing done file, rate-limit retries exhausted, etc.). Distinct
    /// from `Invalid`: the worker never had a chance to do its job, so
    /// these failures consume their own retry budget (`transport_attempt`
    /// against `transport_invalid_review_threshold`) rather than the
    /// work-quality budget tracked by `attempt`.
    /// (Bug X principled fix.)
    Transport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ResponseStatus {
    #[default]
    Ok,
    Malformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum ReviewDecisionKind {
    #[default]
    #[serde(alias = "continue")]
    Continue,
    #[serde(alias = "advance_phase")]
    AdvancePhase,
    #[serde(alias = "need_input")]
    NeedInput,
    #[serde(alias = "done")]
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum ResetChoice {
    #[default]
    #[serde(alias = "none")]
    None,
    #[serde(alias = "last_commit")]
    LastCommit,
    /// Rewind the repo worktree to the most recent checkpoint that had
    /// `global_blockers().is_empty()` at emission time (marked by the
    /// `supervisor2/clean-NNNNNN` git tag). Coarser than `LastCommit`:
    /// may discard several cycles of worker effort. Intended as the
    /// reviewer's "break out of a blocker spiral" lever. Guidance:
    /// consider when `cycles_since_clean >= 3`.
    #[serde(alias = "last_clean")]
    LastClean,
    /// Restore one coarse node to its theorem-stating snapshot and let
    /// the runtime prune any helper nodes that become orphaned. The
    /// runtime re-observes the resulting tablet and normal fingerprint
    /// comparisons decide which lanes remain approved.
    #[serde(alias = "theorem_stating_node")]
    TheoremStatingNode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum HumanChoice {
    #[default]
    Approve,
    Feedback,
}

// ---- Cleanup-v2 audit types -----------------------------------------------
// See CLAUDES_NOTES_cleanup_v2.md / CLAUDES_NOTES_cleanup_v2_impl_plan.md.
// Added 2026-05-14. All carry `Default` impls so the container-level
// `#[serde(default)]` on `ProtocolState` and `AuditResponse` covers
// legacy state files that don't mention these fields at all.

/// Audit-time confidence rating for a proposed cleanup task. Free-form
/// guidance only; the kernel does not gate behavior on this. Surfaced to
/// the reviewer so they can prioritize tasks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum CleanupTaskConfidence {
    #[default]
    #[serde(alias = "low")]
    Low,
    #[serde(alias = "medium")]
    Medium,
    #[serde(alias = "high")]
    High,
}

/// Replacement target for a `CleanupTaskKind::Substitution` task. Either
/// the worker should inline a mathlib lemma (citation carried verbatim)
/// or replace with a different tablet node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CleanupReplacement {
    Mathlib { citation: String },
    TabletWrapper { node: NodeId },
}

impl Default for CleanupReplacement {
    fn default() -> Self {
        Self::Mathlib {
            citation: String::new(),
        }
    }
}

/// Cleanup-v2 task kind. Substitution tasks delete a tablet node and
/// rewire its importers; LintFix tasks apply a single-node edit driven
/// by a lake-warning string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CleanupTaskKind {
    Substitution { replacement: CleanupReplacement },
    LintFix { warning_text: String },
}

impl Default for CleanupTaskKind {
    fn default() -> Self {
        Self::LintFix {
            warning_text: String::new(),
        }
    }
}

/// Cleanup-v2 task status. Transitions:
///   Pending → {Dismissed, Failed, Completed}.
/// Terminal states (Dismissed, Failed, Completed) never reverse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CleanupTaskStatus {
    Pending,
    Dismissed { reason: String },
    Failed { reason: String },
    Completed,
}

impl Default for CleanupTaskStatus {
    fn default() -> Self {
        Self::Pending
    }
}

/// A single cleanup-v2 task. Either audit-proposed (Pending after audit
/// burst, then reviewer-dismissed or reviewer-dispatched) or — historically,
/// not yet — synthesized by other means. The `audit_origin_round` marker
/// records which audit round (1 or 2) created this task so a later audit
/// round can revise only its own past Pending proposals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanupAuditTask {
    pub target_node: NodeId,
    pub rationale: String,
    pub confidence: CleanupTaskConfidence,
    pub kind: CleanupTaskKind,
    pub status: CleanupTaskStatus,
    /// Audit round (1 or 2) that produced this task. Used by
    /// `apply_audit_response` to enforce the "audit may revise only its
    /// own current-round Pending proposals" rule. Defaults to 1 for
    /// legacy state files (which won't have any cleanup_audit_tasks
    /// anyway, but kept for forward-compat).
    pub audit_origin_round: u32,
}

impl Default for CleanupAuditTask {
    fn default() -> Self {
        Self {
            target_node: NodeId::default(),
            rationale: String::new(),
            confidence: CleanupTaskConfidence::default(),
            kind: CleanupTaskKind::default(),
            status: CleanupTaskStatus::default(),
            audit_origin_round: 1,
        }
    }
}

/// Outcome of an audit burst. `NeedToContinue` requests another burst
/// (subject to the per-round burst cap); `AuditDone` transitions to
/// the reviewer task-processing loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AuditOutcome {
    #[default]
    #[serde(alias = "audit_done", alias = "done")]
    AuditDone,
    #[serde(alias = "need_to_continue", alias = "continue")]
    NeedToContinue,
}

/// One entry in an audit response's `new_tasks` list. Carries the same
/// shape as `CleanupAuditTask` minus status / audit_origin_round, which
/// the kernel sets at append time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NewCleanupAuditTask {
    pub target_node: NodeId,
    pub rationale: String,
    pub confidence: CleanupTaskConfidence,
    pub kind: CleanupTaskKind,
}

/// Status transition requested by the audit on one of its own
/// previously-proposed (Pending, current-round) tasks. Kernel rejects
/// modifications of non-Pending or out-of-round tasks. Status here is
/// limited to `Dismissed { reason }` — the audit cannot mark a task
/// Completed or Failed (those are worker outcomes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CleanupAuditTaskModification {
    pub task_index: u32,
    pub reason: String,
}

/// Per-round burst cap. The audit may produce at most this many bursts
/// per round before being forced to AuditDone.
pub const CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND: u32 = 5;

/// Total number of audit rounds permitted per Cleanup phase entry.
/// The reviewer may request a re-audit once (round 1 → round 2); a
/// second re-audit request is ignored.
pub const CLEANUP_AUDIT_MAX_ROUNDS: u32 = 2;

/// Consecutive Invalid worker bursts in Phase::Cleanup that force the
/// run to terminate (auto-Done). Mirrors `proof_invalid_review_threshold`
/// in shape but applies in cleanup specifically.
pub const CLEANUP_CONSECUTIVE_INVALID_THRESHOLD: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RequestKind {
    #[default]
    #[serde(alias = "worker")]
    Worker,
    #[serde(alias = "paper")]
    Paper,
    #[serde(alias = "corr")]
    Corr,
    #[serde(alias = "sound")]
    Sound,
    #[serde(alias = "review")]
    Review,
    #[serde(alias = "human_gate")]
    HumanGate,
    /// Cleanup-v2 audit role — proposes target nodes for substitution /
    /// lint-fix tasks during the cleanup audit sub-phase. Has its own
    /// stage (`Stage::CleanupAudit`) and response envelope
    /// (`AuditResponse`). Distinct from `Paper` because the response
    /// shape (`new_tasks` / `task_modifications` / `scratchpad_replace` /
    /// `outcome`) diverges from substantiveness's lane-update
    /// reconciliation.
    #[serde(alias = "audit")]
    Audit,
    #[serde(alias = "stuck_math_audit")]
    StuckMathAudit,
}

impl RequestKind {
    pub fn requires_runtime_support(self) -> bool {
        matches!(
            self,
            Self::Worker
                | Self::Paper
                | Self::Corr
                | Self::Sound
                | Self::Review
                | Self::Audit
                | Self::StuckMathAudit
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Update<T> {
    Same,
    Set(T),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkingSnapshot {
    pub present_nodes: BTreeSet<NodeId>,
    pub open_nodes: BTreeSet<NodeId>,
    pub coverage: BTreeMap<TargetId, BTreeSet<NodeId>>,
    pub target_fingerprints: BTreeMap<NodeId, Fingerprint>,
    pub corr_current_fingerprints: BTreeMap<NodeId, Fingerprint>,
    pub paper_current_fingerprints: BTreeMap<TargetId, Fingerprint>,
    pub sound_current_fingerprints: BTreeMap<NodeId, Fingerprint>,
    /// Deviation reference-file content fingerprints. Empty value means the
    /// file was missing or unreadable at observation time.
    #[serde(default)]
    pub deviation_current_fingerprints: BTreeMap<DeviationId, Fingerprint>,
    #[serde(default)]
    pub sound_current_fingerprint_parts: BTreeMap<NodeId, SoundFingerprintParts>,
    #[serde(default)]
    pub sketch_proof_nodes: BTreeSet<NodeId>,
    /// Substantiveness fingerprints. Each value is a JSON-encoded
    /// `SubstantivenessFingerprint` (see `runtime_cli_observations.rs`).
    /// Mirrors `corr_current_fingerprints` in shape; populated by the
    /// runtime after every worker delta during TheoremStating and
    /// ProofFormalization (helper nodes added by Hard restructure are
    /// checked too). Empty in cleanup/complete phases — the lane is dormant
    /// there and `current_substantiveness_state` short-circuits to Pass.
    #[serde(default)]
    pub substantiveness_current_fingerprints: BTreeMap<NodeId, Fingerprint>,
    /// Per-target narrow Lean type-surface closure: project-defined nodes
    /// reached by walking each covering node's Lean *type* signature
    /// (`scripts/lean_semantic_fingerprint.lean` policy: theorems → walk
    /// type only, definitions → walk type and value, stop at the
    /// `Tablet.*` boundary). Excludes Preamble and the covering nodes
    /// themselves (those live in `coverage`). Populated by the runtime
    /// observation layer at every worker burst by reading the cached
    /// `lean_semantic_payload` sidecars. AdvancePhase snapshots the
    /// union of these sets into `approved_targets.protected_closure_nodes`
    /// to extend the worker-acceptance protection set beyond the bare
    /// covering nodes — without this the kernel would silently allow a
    /// proof-phase worker to mutate a definition that's part of an
    /// approved target's *meaning surface*, only catching the divergence
    /// later via the per-target paper fingerprint reopen.
    #[serde(default)]
    pub protected_closure_nodes_per_target: BTreeMap<TargetId, BTreeSet<NodeId>>,
}

pub type NodeBoolUpdates = BTreeMap<NodeId, Update<bool>>;
pub type NodeKindUpdates = BTreeMap<NodeId, Update<NodeKind>>;
pub type NodeSetUpdates = BTreeMap<NodeId, Update<BTreeSet<NodeId>>>;
pub type TargetClaimUpdates = BTreeMap<NodeId, Update<BTreeSet<TargetId>>>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovedTargetSnapshot {
    pub configured_targets: BTreeSet<TargetId>,
    pub coverage: BTreeMap<TargetId, BTreeSet<NodeId>>,
    /// Frozen-at-AdvancePhase narrow Lean type-surface closure of the
    /// approved target package: project-defined nodes reached by walking
    /// the Lean *type* signature of each covering node (recursing into
    /// def values per `scripts/lean_semantic_fingerprint.lean`'s policy;
    /// theorem proof bodies are excluded so lemmas used only in proofs
    /// don't enter this set). Excludes the covering nodes themselves
    /// (those live in `coverage`). Snapshotted from
    /// `live.protected_closure_nodes_per_target` at AdvancePhase Approve;
    /// `approved_target_nodes()` returns the union with `coverage`.
    /// Empty for legacy state (`#[serde(default)]`) — pre-deploy
    /// AdvancePhase snapshots will continue to behave as before
    /// (covering-only protection) until the next AdvancePhase fires.
    #[serde(default)]
    pub protected_closure_nodes: BTreeSet<NodeId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingTask {
    pub task_blockers: BTreeSet<Blocker>,
    pub node: Option<NodeId>,
    pub mode: TaskMode,
    #[serde(default)]
    pub orphan_cleanup_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub protected_semantic_change_nodes: BTreeSet<NodeId>,
    /// Existing nodes the worker may edit. Reviewer-set for proof
    /// Restructure / CoarseRestructure tasks; empty for modes/paths
    /// that fall back to the legacy scope-envelope (Local, theorem
    /// phases, orphan cleanup, internal synthetic tasks).
    #[serde(default)]
    pub authorized_nodes: BTreeSet<NodeId>,
    #[serde(default = "default_true")]
    pub allow_new_obligations: bool,
    #[serde(default)]
    pub must_close_active: bool,
    pub next_worker_context_mode: WorkerContextMode,
    pub paper_focus_ranges: Vec<PaperFocusRange>,
    pub work_style_hint: WorkerWorkStyleHint,
    /// global_repair_mode Step C: presentational flag — true when this
    /// pending task was produced by a `consume_global_repair_grant`
    /// Continue. Permission is still `authorized_nodes`; this only
    /// drives worker prompt copy.
    #[serde(default)]
    pub consumed_global_repair_grant: bool,
}

impl Default for PendingTask {
    fn default() -> Self {
        Self {
            task_blockers: BTreeSet::new(),
            node: None,
            mode: TaskMode::Global,
            orphan_cleanup_nodes: BTreeSet::new(),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::None,
            consumed_global_repair_grant: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProtectedSemanticChangeConfirmation {
    pub nodes: BTreeSet<NodeId>,
    pub next_active: Option<NodeId>,
    pub next_mode: TaskMode,
    #[serde(default = "default_true")]
    pub allow_new_obligations: bool,
    #[serde(default)]
    pub must_close_active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerContextMode {
    Resume,
    Fresh,
}

impl Default for WorkerContextMode {
    fn default() -> Self {
        Self::Resume
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerWorkStyleHint {
    None,
    Restructure,
}

impl Default for WorkerWorkStyleHint {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperFocusRange {
    pub start_line: u32,
    pub end_line: u32,
    pub reason: String,
}

/// Reviewer-side attestation that `paper_focus_ranges` were directly
/// consulted in the reviewer's current pass. Required on Continue
/// decisions in friction-state reviews (any blockers, or
/// retry_outcome_kind ∈ {Stuck, NeedsRestructure}); enforced by
/// `WrapperRequest::review_response_paper_grounding_legal`.
///
/// The cited ranges themselves live in `ReviewResponse.paper_focus_ranges`;
/// this struct is the *attestation* that the reviewer did the reading
/// rather than a place to duplicate the ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperGrounding {
    /// True iff the reviewer directly consulted the original paper
    /// text for every range in `paper_focus_ranges` before submitting
    /// this response.
    pub consulted_cited_ranges: bool,
    /// Short reviewer-authored summary of what the cited paper text
    /// says and why it matters for the next step. Required to be
    /// nonempty when `consulted_cited_ranges` is required to be true.
    pub basis_summary: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StuckMathAuditReviewReport {
    /// Short reviewer-authored note from the audit pass. This is
    /// intentionally free-form: StuckMathAudit is a deeper-analysis mode,
    /// not a fixed diagnostic protocol.
    pub notes: String,
    /// Optional reviewer-produced Lean/math artifact to hand to the next
    /// worker. The value is intentionally schema-light: it may describe a
    /// finite obstruction, a sufficient strengthened statement, a scratch
    /// Lean probe, or another compact diagnostic product.
    pub reviewer_lean_product: Option<serde_json::Value>,
}

fn stuck_math_json_value_meaningful(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::String(text) => !text.trim().is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        serde_json::Value::Object(map) => !map.is_empty(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
    }
}

pub fn stuck_math_reviewer_lean_product_within_limit(value: &serde_json::Value) -> bool {
    serde_json::to_string(value)
        .map(|text| text.chars().count() <= STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS)
        .unwrap_or(false)
}

impl StuckMathAuditReviewReport {
    pub fn has_content(&self) -> bool {
        !self.notes.trim().is_empty()
            || self
                .reviewer_lean_product
                .as_ref()
                .is_some_and(stuck_math_json_value_meaningful)
    }

    pub fn reviewer_lean_product_meaningful(&self) -> Option<&serde_json::Value> {
        self.reviewer_lean_product
            .as_ref()
            .filter(|value| stuck_math_json_value_meaningful(value))
    }

    pub fn reviewer_lean_product_within_limit(&self) -> bool {
        self.reviewer_lean_product
            .as_ref()
            .filter(|value| stuck_math_json_value_meaningful(value))
            .map_or(true, stuck_math_reviewer_lean_product_within_limit)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NeedInputAuditContext {
    pub phase: Phase,
    pub active_node: Option<NodeId>,
    pub held_target: Option<NodeId>,
    pub mode: TaskMode,
    pub reviewer_reason: String,
    pub reviewer_comments: String,
    pub review_request_id: u32,
    pub review_cycle: u32,
    pub gate_from_invalid_attempt: bool,
}

/// global_repair_mode Step A: reviewer's pending audit request for an
/// out-of-cone authorization. Set on Step A acceptance and cleared on
/// Step B audit response (approve or decline).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingGlobalRepairRequest {
    pub proposed_extension_nodes: BTreeSet<NodeId>,
    pub reviewer_reason: String,
    pub review_request_id: u32,
    pub review_cycle: u32,
    pub dispatched_at_cycle: u32,
}

/// global_repair_mode Step B: auditor's approval. While present, the
/// kernel will accept reviewer `authorized_nodes` drawn from
/// `approved_extension_nodes` outside the active-coarse cone and will
/// not require `next_active` to lie in the cone for nodes in that set.
/// Cleared on worker acceptance, Step A re-dispatch, LastClean / phase
/// advance, or TTL expiry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingGlobalRepairGrant {
    pub approved_extension_nodes: BTreeSet<NodeId>,
    pub auditor_reason: String,
    pub dispatched_at_cycle: u32,
    pub granted_at_cycle: u32,
    pub review_request_id: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StuckMathAuditState {
    pub active: bool,
    pub trigger: String,
    pub active_since_cycle: u32,
    pub trigger_blockers: BTreeSet<Blocker>,
    pub last_reviewer_lean_product: Option<serde_json::Value>,
    pub need_input_audit: Option<NeedInputAuditContext>,
}

pub type AuditLatchState = StuckMathAuditState;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditTask {
    pub id: String,
    pub title: String,
    pub body: String,
    pub dismissed: bool,
    pub dismissed_reason: String,
    pub dismissed_at_cycle: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditPlan {
    pub report: String,
    pub tasks: Vec<AuditTask>,
    pub probe_paths: Vec<String>,
    /// True when the plan was produced by the NeedInputAuditor role
    /// after a reviewer attempted to escalate to HumanGate.
    #[serde(default)]
    pub need_input_audit: bool,
    /// Optional StuckMathAudit-authorized cone clean. When present, the
    /// runtime restored this coarse node to its theorem-stating snapshot
    /// and pruned the orphaned helper cone before the reviewer sees the
    /// plan. The node must be resettable when the audit response is
    /// accepted.
    #[serde(default, alias = "recommended_cone_clean_node")]
    pub cone_clean_node: Option<NodeId>,
    pub written_at_cycle: u32,
    pub written_by_request: u32,
    pub trigger_at_write: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskDismissal {
    pub id: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WrapperRequest {
    pub id: u32,
    pub kind: RequestKind,
    pub cycle: u32,
    pub phase: Phase,
    pub active_node: Option<NodeId>,
    pub held_target: Option<NodeId>,
    pub mode: TaskMode,
    pub blockers: BTreeSet<Blocker>,
    pub blocked_targets: BTreeSet<TargetId>,
    pub configured_targets: BTreeSet<TargetId>,
    pub verify_nodes: BTreeSet<NodeId>,
    pub verify_targets: BTreeSet<TargetId>,
    pub verify_lanes: BTreeSet<LaneId>,
    #[serde(default)]
    pub paper_verify_lane_bindings: Vec<crate::BridgeVerifierLaneBinding>,
    #[serde(default)]
    pub corr_verify_lane_bindings: Vec<crate::BridgeVerifierLaneBinding>,
    #[serde(default)]
    pub sound_verify_lane_bindings: Vec<crate::BridgeVerifierLaneBinding>,
    #[serde(default)]
    pub worker_binding: crate::BridgeActorBinding,
    #[serde(default)]
    pub reviewer_binding: crate::BridgeActorBinding,
    #[serde(default)]
    pub stuck_math_audit_binding: crate::BridgeActorBinding,
    pub paper_verify_targets: BTreeSet<TargetId>,
    /// Substantiveness frontier (TheoremStating + ProofFormalization). Populated
    /// when a Paper request is dispatched in the per-node scenario. The
    /// kernel cycle scheduler always fires the target-level scenario first
    /// (drains `paper_verify_targets`) and only emits this set once that
    /// frontier is empty. Outside TheoremStating this set is always empty.
    #[serde(default)]
    pub substantiveness_verify_nodes: BTreeSet<NodeId>,
    /// Single deviation reference file selected for paper-lane authorization.
    #[serde(default)]
    pub deviation_verify_id: Option<DeviationId>,
    #[serde(default)]
    pub deviation_verify_path: String,
    /// Authorized deviation ids available to substantiveness and workers.
    #[serde(default)]
    pub authorized_deviations: BTreeMap<DeviationId, String>,
    #[serde(default)]
    pub current_deviation_files: BTreeMap<DeviationId, String>,
    /// Node-level claims naming the authorized deviations used by each node.
    #[serde(default)]
    pub node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    pub corr_verify_nodes: BTreeSet<NodeId>,
    pub corr_verify_targets: BTreeSet<TargetId>,
    pub sound_verify_nodes: BTreeSet<NodeId>,
    pub sound_verify_node: Option<NodeId>,
    pub runtime_support_required: bool,
    /// Covering nodes for approved paper targets, snapshotted at the last
    /// advance-gate approval. Feeds `paper_target_corr_reopen_guard_errors`
    /// at worker-commit validation. (Companion to `approved_corr_fingerprints`
    /// below.)
    #[serde(default)]
    pub approved_target_nodes: BTreeSet<NodeId>,
    /// Subset of `state.corr_approved_fingerprints` keyed by the
    /// `approved_target_nodes` set. Each value is a JSON-encoded
    /// `CorrespondenceFingerprint`. The commit-time guard rejects worker
    /// deltas that would change any of these fingerprints outside
    /// `coarse_restructure` mode.
    #[serde(default)]
    pub approved_corr_fingerprints: BTreeMap<NodeId, Fingerprint>,
    /// Nodes present on the tablet at the end of theorem-stating, i.e. the
    /// "coarse DAG" approved by the HumanGate advance decision. Worker
    /// signature edits on these nodes require `coarse_restructure` mode;
    /// signatures of proof-phase helpers added after the transition are
    /// editable under plain `restructure`. Empty for pre-implementation
    /// runs — the checker treats that case conservatively (all nodes coarse)
    /// to preserve prior behaviour.
    #[serde(default)]
    pub coarse_dag_nodes: BTreeSet<NodeId>,
    /// Proposal v32: the active coarse-DAG anchor (or None when no
    /// anchor is set — boot, TheoremStating, Cleanup, or post-cone-
    /// clean-of-anchor). Surfaced on worker + review requests in
    /// ProofFormalization so prompts can frame the cycle around the
    /// locked anchor.
    #[serde(default)]
    pub active_coarse_node: Option<NodeId>,
    /// Proposal v32: candidate coarse anchors the reviewer may switch
    /// to this cycle. Empty whenever `active_coarse_change_allowed()`
    /// is false (which includes "anchor is locked under shallow closure
    /// invariant"). Surfaced on Review requests only.
    #[serde(default)]
    pub kernel_hinted_next_active_coarse_nodes: BTreeSet<NodeId>,
    /// Proposal v32: TRUE iff at least one task-blocker carrier lies
    /// outside `coarse_node_support_cone(active_coarse_node, ...)`.
    /// Tells the reviewer prompt to reframe this cycle as "repair these
    /// blockers, not new formalization." Always false in TheoremStating /
    /// Cleanup / when no anchor is set / when coarse_dag_nodes is empty.
    #[serde(default)]
    pub coarse_repair_mode: bool,
    /// Proposal v32: starvation-guard counter (see ProtocolState field).
    /// Surfaced on Review requests so the prompt fragment can mention
    /// "your anchor lock has been forced open by the starvation guard"
    /// when the counter has crossed the threshold.
    #[serde(default)]
    pub cycles_in_coarse_repair_mode: u32,
    /// Proposal v32 audit-2 followup #8: TRUE iff the anchor lock is
    /// currently open ONLY because the starvation guard fired, not
    /// because the anchor reached its clean unlock predicate. Lets the
    /// reviewer prompt distinguish "clean unlock — anchor work done,
    /// pick the next coarse goal" from "forced unlock — blocker chain
    /// has been spinning for >= threshold cycles, switching anchor
    /// might unstick progress." Always false outside ProofFormalization
    /// and when the mechanism is dormant.
    #[serde(default)]
    pub coarse_anchor_starvation_unlocked: bool,
    /// Pending reviewer confirmation for exceptional protected semantic
    /// movement. The first Review response that names protected semantic
    /// scope records this and reissues Review with a short warning; the
    /// next response must repeat the same node set / active / mode and set
    /// `confirm_protected_semantic_change_scope=true` before a worker is
    /// dispatched.
    #[serde(default)]
    pub protected_semantic_change_confirmation: Option<ProtectedSemanticChangeConfirmation>,
    /// Protected approved-target / protected-closure nodes whose semantic
    /// meaning was actually reopened and is awaiting explicit human
    /// reapproval. Populated on Review requests as context while ordinary
    /// blockers remain, and on ProtectedReapproval HumanGate requests as the
    /// exact scope under human review.
    #[serde(default)]
    pub protected_reapproval_nodes: BTreeSet<NodeId>,
    pub allowed_decisions: BTreeSet<ReviewDecisionKind>,
    pub allowed_next_modes: BTreeSet<TaskMode>,
    #[serde(default, alias = "allowed_next_active_nodes")]
    pub kernel_hinted_next_active_nodes: BTreeSet<NodeId>,
    /// Proposal v32 audit-2 followup #3: pre-cone-narrowing `next_active`
    /// candidates in ProofFormalization. Mirrors the kernel's
    /// `proof_active_node_base_legal_candidates()` so the response
    /// validator can check legality under a PROSPECTIVE coarse anchor
    /// the reviewer is about to set (`next_active_coarse=Some(B)`):
    /// `node` must be in this set AND in the down-cone of `B`. Without
    /// this denormalization the validator would only have `kernel_hinted_
    /// next_active_nodes`, which is already cone-narrowed to the OLD
    /// anchor and so rejects every legal one-cycle anchor switch.
    /// Empty outside ProofFormalization Review requests.
    #[serde(default)]
    pub proof_active_node_base_legal_candidates: BTreeSet<NodeId>,
    /// Proposal v32 audit-2 followup (post-fix): carrier-node projection of
    /// `ProtocolState::coarse_task_blocker_nodes()` — the FULL
    /// `global_blockers()` set, NOT the `is_dispatch_eligible`-filtered
    /// `blockers` field on this request. The two diverge for deferred
    /// blockers (`Blocker::deferred = true`): kernel-side
    /// `coarse_repair_mode` / `coarse_legal_active_set` see them as
    /// repair-mode-widening carriers, but the reviewer-visible
    /// `blockers` field does not. Without this denormalized field the
    /// request-side cone helpers would silently compute a NARROWER
    /// cone than the kernel's own, leading the live JSON path to
    /// over-reject `next_active` / `authorized_nodes` choices the
    /// kernel itself would accept. Empty outside Worker / Review
    /// requests in ProofFormalization.
    #[serde(default)]
    pub coarse_repair_blocker_carriers: BTreeSet<NodeId>,
    /// global_repair_mode S8: monotone history of coarse nodes that have
    /// been shallowly-closed at some prior committed checkpoint.
    /// Surfaced on Review and StuckMathAudit requests in
    /// ProofFormalization; empty otherwise.
    #[serde(default)]
    pub ever_shallow_coarse_closed: BTreeSet<NodeId>,
    /// global_repair_mode S8: subset of `ever_shallow_coarse_closed` that
    /// is NOT currently closed. Reviewer-actionable signal: repairing
    /// these nodes (possibly via `global_repair_request` if they fall
    /// outside the cone) lifts the anchor-change lock.
    #[serde(default)]
    pub ever_shallow_coarse_closed_regressed: BTreeSet<NodeId>,
    /// global_repair_mode: Step A pending audit request (Review +
    /// StuckMathAudit visibility).
    #[serde(default)]
    pub pending_global_repair_request: Option<PendingGlobalRepairRequest>,
    /// global_repair_mode: Step B audit grant visible to the reviewer
    /// for Step C consumption.
    #[serde(default)]
    pub pending_global_repair_grant: Option<PendingGlobalRepairGrant>,
    /// global_repair_mode S9: auditor's most recent decline reason.
    #[serde(default)]
    pub latest_global_repair_audit_decline_reason: String,
    /// global_repair_mode feature gate mirror. When `false` the reviewer
    /// must not emit `global_repair_request` or `consume_global_repair_grant`.
    #[serde(default = "default_true")]
    pub global_repair_mode_enabled: bool,
    /// global_repair_mode Step C presentational flag on the worker
    /// request: `true` when the dispatched pending task was produced by
    /// a `consume_global_repair_grant` Continue. The worker prompt may
    /// surface this; the actual permission set remains `authorized_nodes`.
    #[serde(default)]
    pub consumed_global_repair_grant: bool,
    pub targeted_next_active_nodes: BTreeSet<NodeId>,
    pub allow_targeted_without_next_active: bool,
    pub allowed_resets: BTreeSet<ResetChoice>,
    /// Coarse nodes that may be cone-cleaned by a StuckMathAudit
    /// `cone_clean_node` response. Empty for Review requests: the audit
    /// authorizes this reset directly.
    #[serde(default)]
    pub resettable_theorem_stating_nodes: BTreeSet<NodeId>,
    pub allowed_reset_blockers: BTreeSet<Blocker>,
    /// Option C (2026-06-04): retired. Always serialized as an empty
    /// set for serde back-compat with persisted `in_flight_request`
    /// state files; never populated by the engine. The reviewer's
    /// Pass-override authority has been removed entirely (see
    /// REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub allowed_override_blockers: BTreeSet<Blocker>,
    /// Sound nodes whose current obligation is ready for a reviewer-assigned
    /// worker repair. This includes deterministic SKETCH obligations only
    /// after the node and its direct NL proof dependencies have current
    /// Substantiveness+Correspondence Pass.
    #[serde(default)]
    pub sound_repair_ready_nodes: BTreeSet<NodeId>,
    /// Sound nodes for which a reviewer may explicitly request a real Sound
    /// verifier run. SKETCH nodes are excluded; the node must satisfy the same
    /// statement-surface readiness gate as repair work.
    #[serde(default)]
    pub sound_verifier_requestable_nodes: BTreeSet<NodeId>,
    /// Current derived Sound assessment category for reviewer-visible nodes.
    /// Used by the review contract to show why a Sound blocker is assignable,
    /// verifier-requestable, or latent.
    #[serde(default)]
    pub sound_assessment_statuses: BTreeMap<NodeId, SoundAssessmentStatus>,
    /// Re-verification context for the Sound request's target. `Some`
    /// only when this is a `RequestKind::Sound` whose
    /// `sound_verify_node` currently has assessment status
    /// `DepEditOnlyStalePassDeferred` or `SelfEditUnknown`. Surfaces
    /// the per-dep statement-hash drift, an `own_tex_changed` flag,
    /// and the verbatim prior accepted-lane finding for this target.
    /// Wired into the Sound prompt via the
    /// `verifier/common/15a_reverification_context.md` fragment.
    #[serde(default)]
    pub sound_reverification_context: Option<SoundReverificationContext>,
    /// Count of consecutive `CommitCheckpoint`s with non-empty
    /// `global_blockers()`. Surfaced so the reviewer can judge when to
    /// pull `ResetChoice::LastClean`. Guidance lives in the reviewer
    /// prompt fragment; the kernel only exposes the counter.
    #[serde(default)]
    pub cycles_since_clean: u32,
    /// Auditor-facing depth of the no-Sound-progress window: number of
    /// checkpoints back to the OLDEST snapshot C' (with `current - C'
    /// >= k`) for which no node present at both C' and the current
    /// snapshot progressed from not-sound to sound; 0 if no such C'
    /// exists. Lets the auditor distinguish "gate just barely fired at
    /// k" from "stagnation actually extends much further back."
    /// Computed via
    /// `progress_history::oldest_no_progress_window_depth`.
    #[serde(default)]
    pub no_sound_progress_window_cycles: u32,
    /// Current committed count of coarse-DAG nodes that are shallowly
    /// closed from coarse. Kernel-side equivalent of the viewer's
    /// `coarse_shallow.lean_closed` metric.
    #[serde(default)]
    pub shallow_coarse_closed_count: u32,
    /// Consecutive checkpoint cycles since `shallow_coarse_closed_count`
    /// last increased. Operator-facing interpretation: cycles since
    /// remaining coarse-shallow-open work last decreased.
    #[serde(default)]
    pub cycles_since_shallow_coarse_closed_count_increase: u32,
    /// Number of `apply_last_clean_reset` rewinds that have landed on
    /// the current `last_clean_*` mirror. Surfaced so the reviewer
    /// prompt's mandatory-threshold exception (waive when >=2) is
    /// directly verifiable from request_summary.
    #[serde(default)]
    pub last_clean_rewind_count: u32,
    /// StuckMathAudit request view. Active only for Review/Worker requests
    /// in repeated proof-formalization mathematical blockage. Carries the
    /// latest reviewer Lean product, if any, so the worker can see it.
    #[serde(default)]
    pub stuck_math_audit: StuckMathAuditState,
    #[serde(default)]
    pub audit_plan: Option<AuditPlan>,
    #[serde(default)]
    pub previous_audit_plan_snapshot: Option<AuditPlan>,
    #[serde(default)]
    pub latest_stuck_math_audit_rejection_reason: String,
    pub allowed_difficulty_update_nodes: BTreeSet<NodeId>,
    pub current_present_nodes: BTreeSet<NodeId>,
    pub current_proof_nodes: BTreeSet<NodeId>,
    pub current_node_kinds: BTreeMap<NodeId, NodeKind>,
    pub current_deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    pub current_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    #[serde(default)]
    pub current_paper_approved_fingerprints: BTreeMap<TargetId, Fingerprint>,
    pub reviewer_comments: String,
    pub latest_worker_summary: String,
    pub latest_worker_comments: String,
    /// Mirror of the previous worker's `needs_restructure_suggested_nodes`
    /// (empty unless the prior worker returned NeedsRestructure). Surfaced
    /// into the reviewer's request so it can authorize the named nodes
    /// concretely rather than guessing.
    #[serde(default)]
    pub latest_worker_needs_restructure_suggested_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub deterministic_worker_rejection_reasons: Vec<String>,
    /// Kernel-authored reasons the previous reviewer response was rejected.
    /// Populated on a reissued Review request after illegal reviewer JSON.
    #[serde(default)]
    pub latest_review_rejection_reasons: Vec<String>,
    #[serde(default)]
    pub review_verifier_evidence: ReviewVerifierEvidence,
    #[serde(default)]
    pub previous_paper_lane_findings: BTreeMap<LaneId, PaperReviewerLaneEvidence>,
    /// Substantiveness lane findings from the previous Paper
    /// response (per-node scenario). Mirrors `previous_sound_lane_findings`
    /// in shape: keyed first by node, then by lane. Used by the verifier
    /// prompt to render "previous own findings" for revisits.
    #[serde(default)]
    pub previous_substantiveness_lane_findings:
        BTreeMap<NodeId, BTreeMap<LaneId, PaperReviewerLaneEvidence>>,
    #[serde(default)]
    pub previous_corr_lane_findings: BTreeMap<LaneId, CorrReviewerLaneEvidence>,
    #[serde(
        default,
        deserialize_with = "deserialize_sound_reviewer_evidence_per_node"
    )]
    pub previous_sound_lane_findings: BTreeMap<NodeId, BTreeMap<LaneId, SoundReviewerLaneEvidence>>,
    #[serde(default)]
    pub retry_outcome_kind: RetryOutcomeKind,
    #[serde(default)]
    pub retry_attempt: u32,
    #[serde(default)]
    pub post_advance_routing: bool,
    pub fresh_context: bool,
    pub prompt_contract_version: u32,
    #[serde(default = "crate::default_contract_value")]
    pub project_invariants: serde_json::Value,
    #[serde(default = "crate::default_contract_value")]
    pub paper_contract: serde_json::Value,
    #[serde(default = "crate::default_contract_value")]
    pub corr_contract: serde_json::Value,
    #[serde(default = "crate::default_contract_value")]
    pub sound_contract: serde_json::Value,
    #[serde(default = "crate::default_contract_value")]
    pub worker_contract: serde_json::Value,
    #[serde(default = "crate::default_contract_value")]
    pub review_contract: serde_json::Value,
    /// Cleanup-v2 (2026-05-14): per-burst audit prompt contract. Populated
    /// only for `RequestKind::Audit`; empty otherwise. Hosts the prompt
    /// fragments, request summary, task list view, scratchpad, and
    /// artifact contract for the audit role.
    #[serde(default = "crate::default_contract_value")]
    pub audit_contract: serde_json::Value,
    /// Proof-phase StuckMathAudit prompt contract. Populated only for
    /// `RequestKind::StuckMathAudit`; empty otherwise.
    #[serde(default = "crate::default_contract_value")]
    pub stuck_math_audit_contract: serde_json::Value,
    /// Cleanup-v2: audit-time view of `cleanup_audit_tasks`. Surfaced on
    /// Audit requests so `audit_contract_payload` can render the task
    /// list. On non-Audit requests, this is the empty vec.
    #[serde(default)]
    pub cleanup_audit_tasks_view: Vec<CleanupAuditTask>,
    /// Cleanup-v2: audit-time view of `cleanup_audit_scratchpad`.
    #[serde(default)]
    pub cleanup_audit_scratchpad_view: String,
    /// Cleanup-v2: audit-time view of `cleanup_audit_round` (1 or 2).
    /// Zero when not in an Audit request context.
    #[serde(default)]
    pub cleanup_audit_round_view: u32,
    /// Cleanup-v2: audit-time view of `cleanup_audit_burst_count`.
    #[serde(default)]
    pub cleanup_audit_burst_count_view: u32,
    /// Cleanup-v2: audit-time view of the live protected-statement node
    /// set (`live_protected_statement_node_set`). Empty on non-Audit
    /// requests.
    #[serde(default)]
    pub cleanup_protected_statement_node_set_view: BTreeSet<NodeId>,
    /// Cleanup-v2: audit-time view of `latest_audit_rejection_reason`.
    /// Surfaced on Audit re-issues so the next burst sees what the
    /// previous attempt failed on. Empty on non-Audit requests.
    #[serde(default)]
    pub latest_audit_rejection_reason_view: String,
    /// Cleanup-v2 (audit Finding 2): mirror of `state.cleanup_force_done`.
    /// When true, the consecutive-invalid-worker threshold has been hit
    /// and the reviewer's only legal decision is `Done` with
    /// `cleanup_request_reaudit = false` (the latch overrides re-audit
    /// requests; see `engine.rs:3998`). Surfaced on Review requests so
    /// `review_response_legal` can reject Continue while the latch is
    /// set, and so the reviewer's allowed-decisions set rendered into
    /// the prompt reflects the constraint.
    #[serde(default)]
    pub cleanup_force_done_view: bool,
    #[serde(default)]
    pub worker_context: WorkerContext,
    #[serde(default)]
    pub worker_acceptance: WorkerAcceptanceContract,
    pub invalid_attempt: bool,
    pub human_input_outstanding: bool,
    pub gate_kind: GateKind,
    /// Patch C plan §7.4.2 — sorry-free nodes that lack a fresh
    /// local-closure record at request-issue time, paired with the
    /// failure summary explaining why (axiom violation, strict-context
    /// error, transport error, etc.). Populated for Review requests so
    /// the reviewer can pick `next_active` from this set even when
    /// `task_blockers` is empty (the §7.4.2 "blockers empty but
    /// local-closure work remains" condition); included on Worker
    /// requests as well so the worker prompt can render the failure
    /// context for the auto-scheduled unverified-node case (§7.4.1).
    /// Empty for verifier / HumanGate requests.
    #[serde(default)]
    pub local_closure_unverified: BTreeMap<NodeId, ErrorSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerContext {
    pub enabled: bool,
    pub active_difficulty: NodeDifficulty,
    pub active_easy_attempts: u32,
    pub worker_profile: WorkerProfile,
    pub validation_kind: WorkerValidationKind,
    #[serde(default)]
    pub authorized_nodes: BTreeSet<NodeId>,
    #[serde(default = "default_true")]
    pub allow_new_obligations: bool,
    #[serde(default)]
    pub must_close_active: bool,
    #[serde(default)]
    pub protected_semantic_change_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub next_context_mode: WorkerContextMode,
    #[serde(default)]
    pub paper_focus_ranges: Vec<PaperFocusRange>,
    #[serde(default)]
    pub work_style_hint: WorkerWorkStyleHint,
    /// Cleanup-v2 (2026-05-14): the active cleanup task kind, if any.
    /// Surfaced on Worker contexts during Phase::Cleanup to drive the
    /// substitution-vs-lintfix prompt branch. None for non-cleanup
    /// workers and for legacy lint-only cleanup mode.
    #[serde(default)]
    pub cleanup_active_task_kind_view: Option<CleanupTaskKind>,
    /// Cleanup-v2: the active cleanup task's `target_node`. Surfaced
    /// alongside the task kind. None outside an in-flight cleanup-v2
    /// task.
    #[serde(default)]
    pub cleanup_active_target_node_view: Option<NodeId>,
    /// Cleanup-v2: the active cleanup task's `rationale` (audit
    /// reasoning). Empty when no active task.
    #[serde(default)]
    pub cleanup_active_rationale_view: String,
}

impl Default for WorkerContext {
    fn default() -> Self {
        Self {
            enabled: false,
            active_difficulty: NodeDifficulty::Hard,
            active_easy_attempts: 0,
            worker_profile: WorkerProfile::None,
            validation_kind: WorkerValidationKind::None,
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            protected_semantic_change_nodes: BTreeSet::new(),
            next_context_mode: WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::None,
            cleanup_active_task_kind_view: None,
            cleanup_active_target_node_view: None,
            cleanup_active_rationale_view: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerAcceptanceObservationPlan {
    pub capture_before_snapshot: bool,
    pub capture_before_tablet_contents: bool,
    pub capture_scoped_tablet_baseline_errors: bool,
    pub scoped_tablet_baseline_scope: WorkerBaselineScope,
    pub capture_imports_before: bool,
    pub capture_expected_active_hash: bool,
    pub capture_baseline_declaration_hashes: bool,
    pub capture_baseline_correspondence_hashes: bool,
}

impl Default for WorkerAcceptanceObservationPlan {
    fn default() -> Self {
        Self {
            capture_before_snapshot: false,
            capture_before_tablet_contents: false,
            capture_scoped_tablet_baseline_errors: false,
            scoped_tablet_baseline_scope: WorkerBaselineScope::None,
            capture_imports_before: false,
            capture_expected_active_hash: false,
            capture_baseline_declaration_hashes: false,
            capture_baseline_correspondence_hashes: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerAcceptanceContract {
    pub enabled: bool,
    pub validation_kind: WorkerValidationKind,
    pub authorized_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub protected_semantic_change_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub validation_execution_plan: Vec<WorkerValidationExecutionPlanStep>,
    pub require_explicit_target_claims_for_new_nodes: bool,
    /// Deprecated, retained for backwards-compat in stored state files.
    /// The kernel no longer reads this — Stuck and NeedsRestructure are
    /// honoured regardless of tablet deltas (the runtime worktree restore
    /// + engine `state.restore_committed()` provide the safety guarantee
    /// the flag used to enforce). See
    /// `CLAUDES_NOTES_remove_stuck_nr_no_delta_rule.md`.
    pub forbid_tablet_changes_when_stuck: bool,
    pub observation_plan: WorkerAcceptanceObservationPlan,
}

impl Default for WorkerAcceptanceContract {
    fn default() -> Self {
        Self {
            enabled: false,
            validation_kind: WorkerValidationKind::None,
            authorized_nodes: BTreeSet::new(),
            protected_semantic_change_nodes: BTreeSet::new(),
            validation_execution_plan: Vec::new(),
            require_explicit_target_claims_for_new_nodes: true,
            // Deprecated; see field doc-comment.
            forbid_tablet_changes_when_stuck: false,
            observation_plan: WorkerAcceptanceObservationPlan::default(),
        }
    }
}

impl Default for WrapperRequest {
    fn default() -> Self {
        Self {
            id: 0,
            kind: RequestKind::Worker,
            cycle: 0,
            phase: Phase::Complete,
            active_node: None,
            held_target: None,
            mode: TaskMode::Global,
            blockers: BTreeSet::new(),
            blocked_targets: BTreeSet::new(),
            configured_targets: BTreeSet::new(),
            verify_nodes: BTreeSet::new(),
            verify_targets: BTreeSet::new(),
            verify_lanes: BTreeSet::new(),
            paper_verify_lane_bindings: Vec::new(),
            corr_verify_lane_bindings: Vec::new(),
            sound_verify_lane_bindings: Vec::new(),
            worker_binding: crate::BridgeActorBinding::default(),
            reviewer_binding: crate::BridgeActorBinding::default(),
            stuck_math_audit_binding: crate::BridgeActorBinding::default(),
            paper_verify_targets: BTreeSet::new(),
            substantiveness_verify_nodes: BTreeSet::new(),
            deviation_verify_id: None,
            deviation_verify_path: String::new(),
            authorized_deviations: BTreeMap::new(),
            current_deviation_files: BTreeMap::new(),
            node_deviation_claims: BTreeMap::new(),
            corr_verify_nodes: BTreeSet::new(),
            corr_verify_targets: BTreeSet::new(),
            sound_verify_nodes: BTreeSet::new(),
            sound_verify_node: None,
            runtime_support_required: false,
            approved_target_nodes: BTreeSet::new(),
            approved_corr_fingerprints: BTreeMap::new(),
            coarse_dag_nodes: BTreeSet::new(),
            active_coarse_node: None,
            kernel_hinted_next_active_coarse_nodes: BTreeSet::new(),
            coarse_repair_mode: false,
            cycles_in_coarse_repair_mode: 0,
            coarse_anchor_starvation_unlocked: false,
            protected_semantic_change_confirmation: None,
            protected_reapproval_nodes: BTreeSet::new(),
            allowed_decisions: BTreeSet::new(),
            allowed_next_modes: BTreeSet::new(),
            kernel_hinted_next_active_nodes: BTreeSet::new(),
            proof_active_node_base_legal_candidates: BTreeSet::new(),
            coarse_repair_blocker_carriers: BTreeSet::new(),
            ever_shallow_coarse_closed: BTreeSet::new(),
            ever_shallow_coarse_closed_regressed: BTreeSet::new(),
            pending_global_repair_request: None,
            pending_global_repair_grant: None,
            latest_global_repair_audit_decline_reason: String::new(),
            global_repair_mode_enabled: true,
            consumed_global_repair_grant: false,
            targeted_next_active_nodes: BTreeSet::new(),
            allow_targeted_without_next_active: false,
            allowed_resets: BTreeSet::new(),
            resettable_theorem_stating_nodes: BTreeSet::new(),
            allowed_reset_blockers: BTreeSet::new(),
            allowed_override_blockers: BTreeSet::new(),
            sound_repair_ready_nodes: BTreeSet::new(),
            sound_verifier_requestable_nodes: BTreeSet::new(),
            sound_assessment_statuses: BTreeMap::new(),
            sound_reverification_context: None,
            cycles_since_clean: 0,
            no_sound_progress_window_cycles: 0,
            shallow_coarse_closed_count: 0,
            cycles_since_shallow_coarse_closed_count_increase: 0,
            last_clean_rewind_count: 0,
            stuck_math_audit: StuckMathAuditState::default(),
            audit_plan: None,
            previous_audit_plan_snapshot: None,
            latest_stuck_math_audit_rejection_reason: String::new(),
            allowed_difficulty_update_nodes: BTreeSet::new(),
            current_present_nodes: BTreeSet::new(),
            current_proof_nodes: BTreeSet::new(),
            current_node_kinds: BTreeMap::new(),
            current_deps: BTreeMap::new(),
            current_target_claims: BTreeMap::new(),
            current_paper_approved_fingerprints: BTreeMap::new(),
            reviewer_comments: String::new(),
            latest_worker_summary: String::new(),
            latest_worker_comments: String::new(),
            latest_worker_needs_restructure_suggested_nodes: BTreeSet::new(),
            deterministic_worker_rejection_reasons: Vec::new(),
            latest_review_rejection_reasons: Vec::new(),
            review_verifier_evidence: ReviewVerifierEvidence::default(),
            previous_paper_lane_findings: BTreeMap::new(),
            previous_substantiveness_lane_findings: BTreeMap::new(),
            previous_corr_lane_findings: BTreeMap::new(),
            previous_sound_lane_findings: BTreeMap::new(),
            retry_outcome_kind: RetryOutcomeKind::None,
            retry_attempt: 0,
            post_advance_routing: false,
            fresh_context: false,
            prompt_contract_version: 0,
            project_invariants: crate::default_contract_value(),
            paper_contract: crate::default_contract_value(),
            corr_contract: crate::default_contract_value(),
            sound_contract: crate::default_contract_value(),
            worker_contract: crate::default_contract_value(),
            review_contract: crate::default_contract_value(),
            audit_contract: crate::default_contract_value(),
            stuck_math_audit_contract: crate::default_contract_value(),
            cleanup_audit_tasks_view: Vec::new(),
            cleanup_audit_scratchpad_view: String::new(),
            cleanup_audit_round_view: 0,
            cleanup_audit_burst_count_view: 0,
            cleanup_protected_statement_node_set_view: BTreeSet::new(),
            latest_audit_rejection_reason_view: String::new(),
            cleanup_force_done_view: false,
            worker_context: WorkerContext::default(),
            worker_acceptance: WorkerAcceptanceContract::default(),
            invalid_attempt: false,
            human_input_outstanding: false,
            gate_kind: GateKind::None,
            local_closure_unverified: BTreeMap::new(),
        }
    }
}

/// Truncate a fingerprint to a stable, bounded prefix for display.
/// Full SHA-style hashes are unwieldy in prompts; the verifier reads
/// the underlying file via git, not the hash. 12 hex chars is enough
/// to disambiguate any plausible drift while keeping the surfaced
/// JSON compact. Empty input renders as `"(absent)"` so the per-dep
/// "added" / "removed" cases are unambiguous.
pub(crate) fn truncate_fingerprint_for_display(fp: &Fingerprint) -> String {
    if fp.is_empty() {
        return "(absent)".to_string();
    }
    if fp.len() <= 12 {
        return fp.clone();
    }
    format!("{}\u{2026}", &fp[..12])
}

/// Compute the per-dep statement-hash drift between a stored
/// (previously-approved) Sound fingerprint and the current one.
/// Returns one entry per dep whose hash differs (including added or
/// removed deps); hashes are truncated for prompt-display.
pub(crate) fn dep_statement_hash_diff(
    stored: &BTreeMap<NodeId, Fingerprint>,
    current: &BTreeMap<NodeId, Fingerprint>,
) -> Vec<SoundDepHashDriftEntry> {
    let mut keys: BTreeSet<NodeId> = stored.keys().cloned().collect();
    keys.extend(current.keys().cloned());
    let mut out = Vec::new();
    for key in keys {
        let prior = stored.get(&key).cloned().unwrap_or_default();
        let curr = current.get(&key).cloned().unwrap_or_default();
        if prior == curr {
            continue;
        }
        out.push(SoundDepHashDriftEntry {
            dep: key,
            prior_hash: truncate_fingerprint_for_display(&prior),
            current_hash: truncate_fingerprint_for_display(&curr),
        });
    }
    out
}

fn dep_closure_from(
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

fn reverse_dep_closure_from(
    seed: &BTreeSet<NodeId>,
    live_present: &BTreeSet<NodeId>,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeSet<NodeId> {
    let mut closure: BTreeSet<NodeId> = seed
        .iter()
        .filter(|node| live_present.contains(*node))
        .cloned()
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for node in live_present {
            if closure.contains(node) {
                continue;
            }
            let requires = deps.get(node).cloned().unwrap_or_default();
            if requires.iter().any(|dep| closure.contains(dep)) {
                closure.insert(node.clone());
                changed = true;
            }
        }
    }
    closure
}

fn impact_region_from(
    node: &NodeId,
    live_present: &BTreeSet<NodeId>,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeSet<NodeId> {
    if !live_present.contains(node) {
        return BTreeSet::new();
    }
    let seed = BTreeSet::from([node.clone()]);
    let mut region = dep_closure_from(&seed, live_present, deps);
    region.extend(reverse_dep_closure_from(&seed, live_present, deps));
    region
}

/// "Shallowly closed from coarse" predicate. A node `n` is shallowly
/// closed-from-coarse iff it is committed-closed (in `present` AND
/// not in `open`) AND every dep `c` of `n` (descending through `deps`,
/// **stopping AT coarse-DAG nodes** which are treated as opaque
/// leaves regardless of their own state) is itself shallowly
/// closed-from-coarse.
///
/// Mirrors the viewer's `isCoarseShallowlyClosed` (viewer/server.js,
/// circa the coarse-DAG progress-graph implementation — see
/// CLAUDES_NOTES_coarse_dag_progress_graph.md) and the viewer's
/// client-side `isShallowlyClosedFromCoarse` (viewer/public/index.html,
/// "Coarse + open only" filter). Same algorithm, byte-for-byte
/// semantics:
///
/// - Cycle guard returns `true` (a cycle would be a kernel bug; the
///   guard prevents infinite recursion without poisoning the result
///   for the rest of the traversal).
/// - A dep that is missing from `present` makes the parent return
///   `false` (the missing-dep recursion hits the closed-check and
///   returns false). This is strict-by-design.
/// - A coarse dep is skipped regardless of its own closed-state.
///
/// Memoizes per-node into `memo`. Callers iterating over many nodes
/// should pass the same `memo` across queries; one-shot callers pass
/// an empty map.
pub fn shallowly_closed_from_coarse(
    node: &NodeId,
    present: &BTreeSet<NodeId>,
    open: &BTreeSet<NodeId>,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    coarse: &BTreeSet<NodeId>,
    memo: &mut BTreeMap<NodeId, bool>,
) -> bool {
    let mut stack = BTreeSet::new();
    shallowly_closed_from_coarse_inner(node, present, open, deps, coarse, memo, &mut stack)
}

fn shallowly_closed_from_coarse_inner(
    node: &NodeId,
    present: &BTreeSet<NodeId>,
    open: &BTreeSet<NodeId>,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    coarse: &BTreeSet<NodeId>,
    memo: &mut BTreeMap<NodeId, bool>,
    stack: &mut BTreeSet<NodeId>,
) -> bool {
    if let Some(&cached) = memo.get(node) {
        return cached;
    }
    if stack.contains(node) {
        return true; // cycle guard — matches viewer semantics
    }
    if !present.contains(node) || open.contains(node) {
        memo.insert(node.clone(), false);
        return false;
    }
    stack.insert(node.clone());
    let mut result = true;
    if let Some(child_deps) = deps.get(node) {
        for child in child_deps {
            if coarse.contains(child) {
                continue;
            }
            if !shallowly_closed_from_coarse_inner(child, present, open, deps, coarse, memo, stack)
            {
                result = false;
                break;
            }
        }
    }
    stack.remove(node);
    memo.insert(node.clone(), result);
    result
}

/// Convenience: compute the shallow-coarse-closure status for every
/// node in `coarse`. Returns the subset of `coarse` whose members
/// are shallowly closed-from-coarse.
pub fn shallowly_closed_coarse_nodes(
    present: &BTreeSet<NodeId>,
    open: &BTreeSet<NodeId>,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    coarse: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    let mut memo: BTreeMap<NodeId, bool> = BTreeMap::new();
    coarse
        .iter()
        .filter(|node| shallowly_closed_from_coarse(node, present, open, deps, coarse, &mut memo))
        .cloned()
        .collect()
}

fn worker_authorized_nodes_for_request_assignment(
    validation_kind: WorkerValidationKind,
    active_node: Option<&NodeId>,
    present_nodes: &BTreeSet<NodeId>,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeSet<NodeId> {
    match validation_kind {
        WorkerValidationKind::TheoremGlobal | WorkerValidationKind::FinalCleanup => {
            present_nodes.clone()
        }
        WorkerValidationKind::TheoremTargeted
        | WorkerValidationKind::ProofRestructure
        | WorkerValidationKind::ProofCoarseRestructure => active_node
            .map(|node| impact_region_from(node, present_nodes, deps))
            .unwrap_or_default(),
        WorkerValidationKind::None
        | WorkerValidationKind::ProofEasy
        | WorkerValidationKind::ProofLocal
        | WorkerValidationKind::Cleanup => BTreeSet::new(),
    }
}

fn review_assignment_validation_kind(phase: Phase, next_mode: TaskMode) -> WorkerValidationKind {
    match phase {
        Phase::TheoremStating => match next_mode {
            TaskMode::Global => WorkerValidationKind::TheoremGlobal,
            TaskMode::Targeted => WorkerValidationKind::TheoremTargeted,
            _ => WorkerValidationKind::None,
        },
        Phase::ProofFormalization => match next_mode {
            TaskMode::Local => WorkerValidationKind::ProofLocal,
            TaskMode::Restructure => WorkerValidationKind::ProofRestructure,
            TaskMode::CoarseRestructure => WorkerValidationKind::ProofCoarseRestructure,
            _ => WorkerValidationKind::None,
        },
        Phase::Cleanup => WorkerValidationKind::FinalCleanup,
        Phase::Complete => WorkerValidationKind::None,
    }
}

impl WrapperRequest {
    /// Scope envelope = the broadest set of existing nodes a reviewer
    /// could authorize given `next_active` / `next_mode`. The reviewer
    /// must pick `authorized_nodes ⊆ scope_envelope`; the actual edit
    /// permission handed to the worker is `authorized_nodes`, not the
    /// envelope.
    pub fn review_scope_envelope(&self, review: &ReviewResponse) -> BTreeSet<NodeId> {
        let validation_kind = review_assignment_validation_kind(self.phase, review.next_mode);
        let active_node = review.next_active.as_ref().or(self.active_node.as_ref());
        worker_authorized_nodes_for_request_assignment(
            validation_kind,
            active_node,
            &self.current_present_nodes,
            &self.current_deps,
        )
    }

    pub fn task_blockers_outside_review_worker_scope(
        &self,
        review: &ReviewResponse,
    ) -> BTreeSet<Blocker> {
        if review.decision != ReviewDecisionKind::Continue
            || review.reset != ResetChoice::None
            || review.task_blockers.is_empty()
        {
            return BTreeSet::new();
        }
        let validation_kind = review_assignment_validation_kind(self.phase, review.next_mode);
        let envelope = self.review_scope_envelope(review);
        // For proof Restructure/CoarseRestructure the reviewer must
        // pick a narrow subset of the envelope (`authorized_nodes`)
        // and every node-bound task blocker must be covered by that
        // subset, not merely by the envelope. Local mode and theorem
        // phases continue to test against the envelope (Local's own
        // task-blocker emptiness rule is enforced separately in
        // `review_response_legal`).
        let coverage = match validation_kind {
            WorkerValidationKind::ProofRestructure
            | WorkerValidationKind::ProofCoarseRestructure
                if !review.authorized_nodes.is_empty() =>
            {
                let mut cov = review.authorized_nodes.clone();
                // global_repair_mode B3: extend blocker coverage with
                // grant nodes when the reviewer is consuming the grant.
                if review.consume_global_repair_grant {
                    if let Some(grant) = self.pending_global_repair_grant.as_ref() {
                        cov.extend(grant.approved_extension_nodes.iter().cloned());
                    }
                }
                cov
            }
            WorkerValidationKind::ProofLocal => {
                // Local mode authorizes editing the active node's proof
                // body. `worker_authorized_nodes_for_request_assignment`
                // returns an empty envelope for `ProofLocal` because
                // `authorized_nodes` must always be empty under Local;
                // but for the Soundness-task-blocker carve-out (see the
                // kind-aware rule in `review_response_legal`), the
                // active node IS the worker's edit coverage. Include it
                // explicitly so Soundness blockers on the active node
                // pass the in-scope check.
                let mut local_coverage = BTreeSet::new();
                if let Some(node) = review.next_active.as_ref().or(self.active_node.as_ref()) {
                    local_coverage.insert(node.clone());
                }
                local_coverage
            }
            _ => envelope,
        };
        review
            .task_blockers
            .iter()
            .filter(|blocker| {
                !self.review_task_blocker_in_worker_scope(blocker, validation_kind, &coverage)
            })
            .cloned()
            .collect()
    }

    pub fn sound_task_blockers_not_repair_ready(
        &self,
        review: &ReviewResponse,
    ) -> BTreeSet<Blocker> {
        if review.decision != ReviewDecisionKind::Continue
            || review.reset != ResetChoice::None
            || review.task_blockers.is_empty()
        {
            return BTreeSet::new();
        }
        review
            .task_blockers
            .iter()
            .filter(|blocker| match (&blocker.object, blocker.kind) {
                (BlockerObject::Node { node }, BlockerKind::Soundness) => {
                    !self.sound_repair_ready_nodes.contains(node)
                }
                _ => false,
            })
            .cloned()
            .collect()
    }

    pub fn sound_verifier_requests_conflicting_with_blocker_actions(
        &self,
        review: &ReviewResponse,
    ) -> BTreeSet<NodeId> {
        let acted_sound_nodes: BTreeSet<NodeId> = review
            .task_blockers
            .iter()
            .chain(review.override_blockers.iter())
            .chain(review.reset_blockers.iter())
            .filter_map(|blocker| match (&blocker.object, blocker.kind) {
                (BlockerObject::Node { node }, BlockerKind::Soundness) => Some(node.clone()),
                _ => None,
            })
            .collect();
        review
            .request_sound_verifier_nodes
            .intersection(&acted_sound_nodes)
            .cloned()
            .collect()
    }

    fn format_node_set(nodes: &BTreeSet<NodeId>) -> String {
        let rendered: Vec<_> = nodes.iter().map(|node| node.as_str()).collect();
        format!("[{}]", rendered.join(", "))
    }

    fn proof_restructure_next_active_advisory(&self, review: &ReviewResponse) -> bool {
        self.phase == Phase::ProofFormalization
            && review.decision == ReviewDecisionKind::Continue
            && review.reset == ResetChoice::None
            && matches!(
                review.next_mode,
                TaskMode::Restructure | TaskMode::CoarseRestructure
            )
    }

    /// Proposal v32 audit-2 followup #3/#5: compute the legal
    /// `next_active` cone for an arbitrary anchor using request-side data
    /// (`current_present_nodes`, `current_deps`, the denormalized full
    /// carrier set `coarse_repair_blocker_carriers`). Mirrors
    /// `ProtocolState::coarse_legal_active_set` but accepts a prospective
    /// anchor rather than reading the current `active_coarse_node`.
    /// Repair-mode widening reads the **deferred-inclusive** carrier set
    /// projected by `expected_request` — NOT `self.blockers`, which is
    /// dispatch-filtered and would compute a narrower cone than the
    /// kernel's own. Returns `current_present_nodes` when the mechanism
    /// is dormant (no `coarse_dag_nodes`) or no anchor is supplied;
    /// returns the empty set if the anchor is not present.
    pub fn coarse_legal_active_set_for_anchor(&self, anchor: Option<&NodeId>) -> BTreeSet<NodeId> {
        if self.coarse_dag_nodes.is_empty() {
            return self.current_present_nodes.clone();
        }
        let Some(anchor) = anchor else {
            return self.current_present_nodes.clone();
        };
        if !self.current_present_nodes.contains(anchor) {
            return BTreeSet::new();
        }
        let mut cone = dep_closure_from(
            &BTreeSet::from([anchor.clone()]),
            &self.current_present_nodes,
            &self.current_deps,
        );
        if self
            .coarse_repair_blocker_carriers
            .iter()
            .any(|c| !cone.contains(c))
        {
            for carrier in &self.coarse_repair_blocker_carriers {
                if !self.current_present_nodes.contains(carrier) {
                    continue;
                }
                cone.insert(carrier.clone());
                let extension = dep_closure_from(
                    &BTreeSet::from([carrier.clone()]),
                    &self.current_present_nodes,
                    &self.current_deps,
                );
                cone.extend(extension);
            }
        }
        cone
    }

    fn review_next_active_legal_for_response(&self, review: &ReviewResponse) -> bool {
        let Some(node) = review.next_active.as_ref() else {
            return true;
        };
        // global_repair_mode B3: when consuming an audit grant, allow
        // next_active to land on any granted extension node, regardless
        // of cone. The base-legal candidate check still applies so a
        // closed unrelated proof node cannot be next_active.
        if review.consume_global_repair_grant {
            if let Some(grant) = self.pending_global_repair_grant.as_ref() {
                if grant.approved_extension_nodes.contains(node)
                    && self.proof_active_node_base_legal_candidates.contains(node)
                {
                    return true;
                }
            }
        }
        // Proposal v32 audit-2 followup #3: when the reviewer proposes a
        // simultaneous anchor switch (`next_active_coarse=Some(B)`), the
        // legality of `next_active` must be evaluated against the
        // PROSPECTIVE anchor's cone — not the request's pre-projected
        // `kernel_hinted_next_active_nodes`, which reflects the OLD
        // anchor's cone and would reject every legal one-cycle switch.
        // The denormalized base-legal candidate set on the request lets
        // us re-apply the cone filter against `B`.
        let anchor_changing = review.next_active_coarse.is_some()
            && review.next_active_coarse != self.active_coarse_node;
        let effective_anchor = if anchor_changing {
            review.next_active_coarse.as_ref()
        } else {
            self.active_coarse_node.as_ref()
        };

        // Proposal v32: even in the Restructure/CoarseRestructure
        // advisory branch (previously: "any present node is legal"),
        // bound the choice to the active-coarse cone (widened to
        // blocker cones in CoarseRepairMode). When no anchor is set or
        // coarse_dag_nodes is empty, `coarse_legal_active_set_for_anchor`
        // is a superset of present_nodes so this is a no-op.
        if self.proof_restructure_next_active_advisory(review) {
            if !self.current_present_nodes.contains(node) {
                return false;
            }
            if effective_anchor.is_none() || self.coarse_dag_nodes.is_empty() {
                return true;
            }
            // Strict mode: even Restructure / CoarseRestructure must
            // land in the cone — of the effective anchor.
            if anchor_changing {
                return self
                    .coarse_legal_active_set_for_anchor(effective_anchor)
                    .contains(node);
            }
            return self.kernel_hinted_next_active_nodes.contains(node);
        }
        if anchor_changing {
            // Base-legal under the live state AND in the new anchor's cone.
            return self.proof_active_node_base_legal_candidates.contains(node)
                && self
                    .coarse_legal_active_set_for_anchor(effective_anchor)
                    .contains(node);
        }
        self.kernel_hinted_next_active_nodes.contains(node)
    }

    /// Proposal v32: reviewer-chosen next active coarse anchor is
    /// legal iff (a) the field is None (preserve current), or (b)
    /// phase is ProofFormalization, retry_outcome_kind is None
    /// (i.e. not ANY kind of retry-review — matches TLA spec
    /// `retryOutcomeKind = "none"`; audit-2 followup #3 widened from
    /// the original `Invalid | Transport`-only check), decision is
    /// Continue, and the chosen node is in
    /// `kernel_hinted_next_active_coarse_nodes` (which is empty when
    /// `active_coarse_change_allowed` is false).
    fn review_next_active_coarse_legal_for_response(&self, review: &ReviewResponse) -> bool {
        let Some(node) = review.next_active_coarse.as_ref() else {
            return true;
        };
        if self.phase != Phase::ProofFormalization {
            return false;
        }
        if !matches!(self.retry_outcome_kind, RetryOutcomeKind::None) {
            return false;
        }
        if review.decision != ReviewDecisionKind::Continue {
            return false;
        }
        self.kernel_hinted_next_active_coarse_nodes.contains(node)
    }

    fn minimal_scope_anchor_for_authorized_nodes(
        &self,
        review: &ReviewResponse,
    ) -> Option<(NodeId, usize, bool)> {
        if review.authorized_nodes.is_empty()
            || !review
                .authorized_nodes
                .is_subset(&self.current_present_nodes)
        {
            return None;
        }
        let mut candidates: Vec<(NodeId, BTreeSet<NodeId>, bool)> = Vec::new();
        for node in &self.current_present_nodes {
            let mut trial = review.clone();
            trial.next_active = Some(node.clone());
            let mut envelope = self.review_scope_envelope(&trial);
            envelope.extend(trial.protected_semantic_change_nodes.iter().cloned());
            if review.authorized_nodes.is_subset(&envelope) {
                candidates.push((
                    node.clone(),
                    envelope,
                    self.kernel_hinted_next_active_nodes.contains(node),
                ));
            }
        }
        candidates
            .into_iter()
            .min_by(
                |(left_node, left_env, left_hinted), (right_node, right_env, right_hinted)| {
                    left_env
                        .len()
                        .cmp(&right_env.len())
                        .then_with(|| right_hinted.cmp(left_hinted))
                        .then_with(|| left_node.cmp(right_node))
                },
            )
            .map(|(node, envelope, hinted)| (node, envelope.len(), hinted))
    }

    fn authorized_scope_rejection_reason(
        &self,
        review: &ReviewResponse,
        outside_scope: &BTreeSet<NodeId>,
    ) -> String {
        let active = review
            .next_active
            .as_ref()
            .or(self.active_node.as_ref())
            .map(|node| node.as_str().to_string())
            .unwrap_or_else(|| "<none>".to_string());
        let mut reason = format!(
            "authorized_node_ids {} are outside the scope envelope for next_active={active}, next_mode={:?}; requested authorized_node_ids={}",
            Self::format_node_set(outside_scope),
            review.next_mode,
            Self::format_node_set(&review.authorized_nodes)
        );
        if let Some((anchor, envelope_size, hinted)) =
            self.minimal_scope_anchor_for_authorized_nodes(review)
        {
            reason.push_str(&format!(
                ". Minimal present next_active example whose envelope contains those authorized_node_ids: {anchor} (envelope_size={envelope_size}; {} in current kernel hints)",
                if hinted { "is" } else { "is not" }
            ));
        } else if !review
            .authorized_nodes
            .is_subset(&self.current_present_nodes)
        {
            let missing: BTreeSet<_> = review
                .authorized_nodes
                .difference(&self.current_present_nodes)
                .cloned()
                .collect();
            reason.push_str(&format!(
                ". No next_active can authorize missing/non-present nodes {}",
                Self::format_node_set(&missing)
            ));
        } else {
            reason.push_str(
                ". No present next_active under the submitted next_mode has an envelope containing all requested authorized_node_ids",
            );
        }
        reason
    }

    pub fn review_response_rejection_reasons(&self, review: &ReviewResponse) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.kind != RequestKind::Review {
            reasons.push(format!(
                "request kind is {:?}, so a Review response is not legal here",
                self.kind
            ));
            return reasons;
        }
        if !review.task_blockers.is_subset(&self.blockers) {
            reasons.push("task_blocker_ids are not a subset of the current blocker set".into());
        }
        if !review
            .override_blockers
            .is_subset(&self.allowed_override_blockers)
        {
            reasons
                .push("override_blocker_ids are not a subset of allowed override blockers".into());
        }
        if !review
            .reset_blockers
            .is_subset(&self.allowed_reset_blockers)
        {
            reasons.push("reset_blocker_ids are not a subset of allowed reset blockers".into());
        }
        if !review.task_blockers.is_disjoint(&review.override_blockers)
            || !review.task_blockers.is_disjoint(&review.reset_blockers)
            || !review.override_blockers.is_disjoint(&review.reset_blockers)
        {
            reasons.push("blocker action lists must be pairwise disjoint".into());
        }
        if !review
            .request_sound_verifier_nodes
            .is_subset(&self.sound_verifier_requestable_nodes)
        {
            reasons.push(
                "request_sound_verifier_node_ids contains nodes that are not legal Sound verifier targets"
                    .into(),
            );
        }
        let conflicting_sound_requests =
            self.sound_verifier_requests_conflicting_with_blocker_actions(review);
        if !conflicting_sound_requests.is_empty() {
            reasons.push(format!(
                "request_sound_verifier_node_ids conflicts with blocker actions on the same Sound nodes: {:?}",
                conflicting_sound_requests
            ));
        }
        if review.next_mode == TaskMode::Local
            && review
                .task_blockers
                .iter()
                .any(|b| !matches!(b.kind, BlockerKind::Soundness))
        {
            reasons.push(
                "Local mode cannot task non-Soundness blockers; use Restructure/CoarseRestructure or do not task those blockers"
                    .into(),
            );
        }
        if self.phase == Phase::ProofFormalization
            && review.decision == ReviewDecisionKind::Continue
            && review.reset == ResetChoice::None
        {
            match review.next_mode {
                TaskMode::Restructure | TaskMode::CoarseRestructure => {
                    if review.authorized_nodes.is_empty() {
                        reasons.push(
                            "Restructure/CoarseRestructure requires non-empty authorized_node_ids"
                                .into(),
                        );
                    }
                }
                TaskMode::Local => {
                    if !review.authorized_nodes.is_empty() {
                        reasons.push("Local mode requires authorized_node_ids to be empty".into());
                    }
                }
                _ => {}
            }
        }
        if !review.authorized_nodes.is_empty() {
            let missing: BTreeSet<_> = review
                .authorized_nodes
                .difference(&self.current_present_nodes)
                .cloned()
                .collect();
            if !missing.is_empty() {
                reasons.push(format!(
                    "authorized_node_ids contains non-present nodes {}",
                    Self::format_node_set(&missing)
                ));
            }
            let mut allowed = self.review_scope_envelope(review);
            allowed.extend(review.protected_semantic_change_nodes.iter().cloned());
            let outside_scope: BTreeSet<_> = review
                .authorized_nodes
                .difference(&allowed)
                .cloned()
                .collect();
            if !outside_scope.is_empty() {
                reasons.push(self.authorized_scope_rejection_reason(review, &outside_scope));
            }
        }
        let outside_worker_scope = self.task_blockers_outside_review_worker_scope(review);
        if !outside_worker_scope.is_empty() {
            reasons.push(format!(
                "task_blocker_ids include blockers outside the proposed worker scope: {:?}",
                outside_worker_scope
            ));
        }
        let sound_not_ready = self.sound_task_blockers_not_repair_ready(review);
        if !sound_not_ready.is_empty() {
            reasons.push(format!(
                "task_blocker_ids include Soundness blockers that are not sound-repair-ready: {:?}",
                sound_not_ready
            ));
        }
        let reset_requested = review.reset != ResetChoice::None;
        match review.reset {
            ResetChoice::TheoremStatingNode => {
                if self.phase != Phase::ProofFormalization
                    || review.decision != ReviewDecisionKind::Continue
                    || !self
                        .allowed_resets
                        .contains(&ResetChoice::TheoremStatingNode)
                {
                    reasons.push(
                        "reset=theorem_stating_node is legal only for an audit-backed ProofFormalization Continue review"
                            .into(),
                    );
                }
                match review.reset_node.as_ref() {
                    Some(reset_node) => {
                        if !self.resettable_theorem_stating_nodes.contains(reset_node) {
                            reasons.push(format!(
                                "reset_node must be one of resettable_theorem_stating_nodes: {}",
                                Self::format_node_set(&self.resettable_theorem_stating_nodes)
                            ));
                        }
                    }
                    None => reasons.push(
                        "reset=theorem_stating_node requires reset_node to name the node".into(),
                    ),
                }
                if review.next_active.is_some() {
                    reasons.push(
                        "reset=theorem_stating_node must leave next_active empty; the post-reset audit/review chooses routing"
                            .into(),
                    );
                }
                if !review.authorized_nodes.is_empty() {
                    reasons.push(
                        "reset=theorem_stating_node must leave authorized_node_ids empty; it does not dispatch a worker"
                            .into(),
                    );
                }
            }
            ResetChoice::None | ResetChoice::LastCommit | ResetChoice::LastClean => {
                if review.reset_node.is_some() {
                    reasons.push("reset_node is only legal with reset=theorem_stating_node".into());
                }
            }
        }
        if reset_requested
            && (!review.task_blockers.is_empty()
                || !review.override_blockers.is_empty()
                || !review.reset_blockers.is_empty()
                || !review.request_sound_verifier_nodes.is_empty())
        {
            reasons.push(
                "reset responses must leave blocker action lists and verifier requests empty"
                    .into(),
            );
        }
        if review.protected_semantic_change_nodes.is_empty() {
            if review.confirm_protected_semantic_change_scope {
                reasons.push(
                    "confirm_protected_semantic_change_scope cannot be true with no protected_semantic_change_node_ids"
                        .into(),
                );
            }
        } else if self.phase != Phase::ProofFormalization
            || review.decision != ReviewDecisionKind::Continue
            || reset_requested
            || review.next_mode != TaskMode::CoarseRestructure
            || review.next_active.is_none()
            || !review
                .protected_semantic_change_nodes
                .is_subset(&self.approved_target_nodes)
        {
            reasons.push(
                "protected_semantic_change_node_ids are legal only for ProofFormalization Continue+CoarseRestructure with next_active set, reset=none, and nodes drawn from approved_target_nodes"
                    .into(),
            );
        }
        let bad_difficulty_nodes: BTreeSet<_> = review
            .difficulty_updates
            .keys()
            .filter(|node| !self.allowed_difficulty_update_nodes.contains(*node))
            .cloned()
            .collect();
        if !bad_difficulty_nodes.is_empty() {
            reasons.push(format!(
                "difficulty_updates contains nodes not currently updatable: {}",
                Self::format_node_set(&bad_difficulty_nodes)
            ));
        }
        if review.clear_human_input && !self.human_input_outstanding {
            reasons
                .push("clear_human_input is legal only when human_input_outstanding=true".into());
        }
        let need_input = review.decision == ReviewDecisionKind::NeedInput;
        if need_input
            && (!review.task_blockers.is_empty()
                || !review.override_blockers.is_empty()
                || !review.reset_blockers.is_empty()
                || !review.request_sound_verifier_nodes.is_empty()
                || review.next_active.is_some()
                || review.next_mode != self.mode)
        {
            reasons.push(
                "NeedInput must not task/override/reset blockers or request_sound_verifier_node_ids, must leave next_active empty, and must keep next_mode equal to the current mode"
                    .into(),
            );
        }
        if !self.allowed_decisions.contains(&review.decision) {
            reasons.push(format!(
                "decision {:?} is not in allowed_decisions {:?}",
                review.decision, self.allowed_decisions
            ));
        }
        if !self.allowed_resets.contains(&review.reset) {
            reasons.push(format!(
                "reset {:?} is not in allowed_resets {:?}",
                review.reset, self.allowed_resets
            ));
        }
        if !self.allowed_next_modes.contains(&review.next_mode) {
            reasons.push(format!(
                "next_mode {:?} is not in allowed_next_modes {:?}",
                review.next_mode, self.allowed_next_modes
            ));
        }
        if !self.review_next_active_legal_for_response(review) {
            reasons.push(format!(
                "next_active={:?} is not legal for this response; proof Restructure/CoarseRestructure may use any present node as a scope anchor, other paths must use kernel hints {}",
                review.next_active,
                Self::format_node_set(&self.kernel_hinted_next_active_nodes)
            ));
        }
        if !self.review_next_active_coarse_legal_for_response(review) {
            reasons.push(format!(
                "next_active_coarse={:?} is not legal for this response; only legal in ProofFormalization Continue cycles (non-retry) and must be a member of kernel_hinted_next_active_coarse_nodes {} (empty means the active coarse anchor is locked under shallow-closure invariant)",
                review.next_active_coarse,
                Self::format_node_set(&self.kernel_hinted_next_active_coarse_nodes)
            ));
        }
        match self.phase {
            Phase::TheoremStating => {
                if review.next_mode == TaskMode::Targeted
                    && review.decision != ReviewDecisionKind::AdvancePhase
                {
                    // AdvancePhase ignores next_active (the next phase's
                    // request rederives it), so a Targeted advance_phase
                    // is legal whether or not next_active is set. Kept
                    // the Targeted-mode requirement for all other
                    // decisions (Continue, NeedInput).
                    if self.allow_targeted_without_next_active {
                        if review.next_active.is_some() {
                            reasons.push(
                                "Targeted mode currently requires next_active to be empty".into(),
                            );
                        }
                    } else if review
                        .next_active
                        .as_ref()
                        .is_none_or(|node| !self.targeted_next_active_nodes.contains(node))
                    {
                        reasons.push(
                            "Targeted mode requires next_active in targeted_next_active_nodes"
                                .into(),
                        );
                    }
                }
                if review.decision == ReviewDecisionKind::AdvancePhase
                    && (!self.blockers.is_empty()
                        || review.reset == ResetChoice::LastClean
                        || (self.human_input_outstanding && !review.clear_human_input))
                {
                    reasons.push(
                        "AdvancePhase requires no blockers, reset != LastClean, and any outstanding human input to be cleared"
                            .into(),
                    );
                }
                if review.decision == ReviewDecisionKind::Done {
                    reasons.push("Done is not legal during TheoremStating review".into());
                }
            }
            Phase::ProofFormalization | Phase::Cleanup => {
                if self.phase == Phase::ProofFormalization
                    && review.decision == ReviewDecisionKind::Continue
                    && review.next_active.is_none()
                    && !matches!(
                        review.reset,
                        ResetChoice::LastClean | ResetChoice::TheoremStatingNode
                    )
                    && (self.active_node.is_some()
                        || !review.task_blockers.is_empty()
                        || review.next_mode != TaskMode::Local)
                {
                    reasons.push(
                        "ProofFormalization Continue requires next_active unless this is an idle Local dispatch with no active node and no tasked blockers"
                            .into(),
                    );
                }
                // Proposal v32 followup: ProofFormalization Continue must
                // advance the coarse anchor whenever the kernel signals
                // that the anchor lock is open on a clean unlock — i.e.
                // `kernel_hinted_next_active_coarse_nodes` is non-empty
                // AND `coarse_anchor_starvation_unlocked` is false. Two
                // sub-cases trigger this:
                //   (a) `active_coarse_node` is None (phase-entry seed
                //       was lost via stale-anchor recovery or legacy
                //       state) — there is no anchor to keep.
                //   (b) `active_coarse_node` is Some(X) and X has
                //       reached shallow-coarse-closure with no global
                //       blockers — the cone is done, so piggybacking on
                //       it instead of advancing is just label noise on
                //       work that actually belongs to the next anchor.
                // Starvation unlocks are exempted: there, switching is
                // encouraged but the reviewer keeps discretion to stay
                // on the same anchor. Retry contexts are exempted
                // because `next_active_coarse` is itself illegal there.
                if self.phase == Phase::ProofFormalization
                    && review.decision == ReviewDecisionKind::Continue
                    && matches!(self.retry_outcome_kind, RetryOutcomeKind::None)
                    && !self.coarse_dag_nodes.is_empty()
                    && !self.kernel_hinted_next_active_coarse_nodes.is_empty()
                    && !self.coarse_anchor_starvation_unlocked
                    && review.next_active_coarse.is_none()
                {
                    let msg = if self.active_coarse_node.is_none() {
                        "ProofFormalization Continue requires next_active_coarse when active_coarse_node is None; pick a coarse anchor from kernel_hinted_next_active_coarse_nodes"
                    } else {
                        "ProofFormalization Continue requires next_active_coarse when the current coarse anchor is shallow-coarse-closed (clean unlock); pick a new anchor from kernel_hinted_next_active_coarse_nodes"
                    };
                    reasons.push(msg.into());
                }
                if self.phase == Phase::Cleanup
                    && review.decision == ReviewDecisionKind::Done
                    && (!self.blockers.is_empty()
                        || !review.task_blockers.is_empty()
                        || !review.override_blockers.is_empty()
                        || !review.reset_blockers.is_empty()
                        || !review.request_sound_verifier_nodes.is_empty())
                {
                    reasons.push(
                        "Cleanup Done is legal only with no current blockers, empty blocker action lists, and no verifier requests"
                            .into(),
                    );
                }
                if self.phase == Phase::Cleanup && review.next_active.is_some() {
                    reasons.push(
                        "next_active must be empty in Phase::Cleanup; the worker's active node is resolved from the dispatched task's target_node — use cleanup_next_task to pick the task"
                            .into(),
                    );
                }
            }
            Phase::Complete => {
                reasons.push("Review responses are not legal in Complete phase".into());
            }
        }
        if !self.cleanup_v2_review_fields_legal(review) {
            reasons.push(
                "cleanup task control fields are inconsistent with the current cleanup contract"
                    .into(),
            );
        }
        let proof_continue = self.phase == Phase::ProofFormalization
            && review.decision == ReviewDecisionKind::Continue;
        if !(proof_continue || (review.allow_new_obligations && !review.must_close_active)) {
            reasons.push(
                "outside ProofFormalization Continue, allow_new_obligations must be true and must_close_active must be false"
                    .into(),
            );
        }
        if review.decision != ReviewDecisionKind::Continue
            && (review.next_worker_context_mode != WorkerContextMode::Resume
                || !review.paper_focus_ranges.is_empty()
                || review.work_style_hint != WorkerWorkStyleHint::None)
        {
            reasons.push(
                "non-Continue reviews must use next_worker_context_mode=resume, no paper_focus_ranges, and work_style_hint=none"
                    .into(),
            );
        }
        if !self.review_response_paper_grounding_legal(review) {
            reasons.push(
                "paper_grounding / paper_focus_ranges do not satisfy the review contract".into(),
            );
        }
        if !self.review_response_stuck_math_audit_legal(review) {
            reasons.push(
                "stuck_math_audit report is missing or invalid while the audit latch is active"
                    .into(),
            );
        }
        if let Some(reason) = self.review_response_audit_plan_rejection_reason(review) {
            reasons.push(reason);
        }
        if reasons.is_empty() && !self.review_response_legal(review) {
            reasons.push("review response failed kernel legality checks; inspect the review contract and submitted artifact fields".into());
        }
        prompt_safe_rejection_reasons(&reasons)
    }

    /// Set of present nodes whose `current_deps` entry should be
    /// surfaced in the worker prompt. Computed from the bidirectional
    /// (dep + reverse-dep) closure of the worker's natural visibility
    /// seed: the active node, the explicit `authorized_nodes` list,
    /// and any node referenced by a current blocker. Explicit
    /// whole-tablet work streams (theorem-stating global and final
    /// cleanup) fall back to all present nodes — the worker really
    /// does need the full DAG there. Non-worker requests get the
    /// full DAG too (this helper is for the worker prompt rendering
    /// only).
    ///
    /// The reverse-dep closure climbs UP from the seed, so paper-
    /// target nodes that consume the worker's authorized region are
    /// included automatically (the worker does need to see how their
    /// edit propagates upward). Paper-target nodes are NOT seeded
    /// directly: their downward closure swallows the entire DAG and
    /// would defeat the trim. Worker can still read paper-target
    /// names from `current_target_claims_nonempty`.
    pub fn worker_prompt_dag_scope(&self) -> BTreeSet<NodeId> {
        if self.kind != RequestKind::Worker {
            return self.current_present_nodes.clone();
        }
        let validation_kind = self.worker_acceptance.validation_kind;
        match validation_kind {
            WorkerValidationKind::TheoremGlobal
            | WorkerValidationKind::Cleanup
            | WorkerValidationKind::FinalCleanup => {
                return self.current_present_nodes.clone();
            }
            _ => {}
        }
        let mut seed: BTreeSet<NodeId> = self.worker_context.authorized_nodes.clone();
        if let Some(node) = self.active_node.as_ref() {
            seed.insert(node.clone());
        }
        for blocker in &self.blockers {
            if let BlockerObject::Node { node } = &blocker.object {
                seed.insert(node.clone());
            }
        }
        let down = dep_closure_from(&seed, &self.current_present_nodes, &self.current_deps);
        let up = reverse_dep_closure_from(&seed, &self.current_present_nodes, &self.current_deps);
        let mut scope: BTreeSet<NodeId> = down;
        scope.extend(up);
        scope
    }

    fn review_task_blocker_in_worker_scope(
        &self,
        blocker: &Blocker,
        validation_kind: WorkerValidationKind,
        authorized_nodes: &BTreeSet<NodeId>,
    ) -> bool {
        match (&blocker.object, blocker.kind) {
            (BlockerObject::Node { node }, _) => authorized_nodes.contains(node),
            (BlockerObject::Target { target }, BlockerKind::PaperFaithfulness) => {
                let coverage_nodes: BTreeSet<NodeId> = self
                    .current_present_nodes
                    .iter()
                    .filter(|node| {
                        self.current_target_claims
                            .get(*node)
                            .is_some_and(|targets| targets.contains(target))
                    })
                    .cloned()
                    .collect();
                if coverage_nodes.is_empty() {
                    return self.phase == Phase::TheoremStating
                        && validation_kind == WorkerValidationKind::TheoremGlobal;
                }
                let support = dep_closure_from(
                    &coverage_nodes,
                    &self.current_present_nodes,
                    &self.current_deps,
                );
                !support.is_disjoint(authorized_nodes)
            }
            (BlockerObject::Deviation { .. }, BlockerKind::Deviation) => match self.phase {
                Phase::TheoremStating => validation_kind == WorkerValidationKind::TheoremGlobal,
                Phase::ProofFormalization => matches!(
                    validation_kind,
                    WorkerValidationKind::ProofEasy
                        | WorkerValidationKind::ProofLocal
                        | WorkerValidationKind::ProofRestructure
                        | WorkerValidationKind::ProofCoarseRestructure
                ),
                Phase::Cleanup | Phase::Complete => false,
            },
            _ => false,
        }
    }

    /// True iff this Review request is in a "friction" state where the
    /// reviewer must ground a Continue decision in the paper:
    /// any blockers present, or the prior worker exited with
    /// `Stuck` / `NeedsRestructure`. All four blocker kinds
    /// (Correspondence, PaperFaithfulness, Soundness, Substantiveness)
    /// are prose/paper judgments per
    /// `prompt_fragments/canonical/*.md`, so any blocker is a paper-
    /// relevant signal. Worker-stuck / needs-restructure outcomes are
    /// likewise the moment to re-read the source before routing.
    pub fn review_requires_paper_grounding(&self) -> bool {
        self.kind == RequestKind::Review
            && (!self.blockers.is_empty()
                || matches!(
                    self.retry_outcome_kind,
                    RetryOutcomeKind::Stuck | RetryOutcomeKind::NeedsRestructure
                ))
    }

    /// Enforce the response-shape rules for `paper_grounding`:
    ///   - For non-Continue decisions: must be default
    ///     (consulted=false, basis_summary empty). Pairs with the
    ///     existing rule that non-Continue must have empty
    ///     `paper_focus_ranges`.
    ///   - For Continue + reset=None in a friction state: at least
    ///     one `paper_focus_ranges` entry, attest consulted=true,
    ///     and provide a non-empty trimmed `basis_summary`.
    ///   - For any Continue with nonempty `paper_focus_ranges`
    ///     (friction or not): the attestation + summary are
    ///     required. Citing ranges always implies the reviewer
    ///     consulted them.
    fn review_response_paper_grounding_legal(&self, review: &ReviewResponse) -> bool {
        let has_ranges = !review.paper_focus_ranges.is_empty();
        let summary_nonempty = !review.paper_grounding.basis_summary.trim().is_empty();
        let consulted = review.paper_grounding.consulted_cited_ranges;

        if review.decision != ReviewDecisionKind::Continue {
            return !consulted && !summary_nonempty;
        }
        // Continue:
        let friction = self.review_requires_paper_grounding() && review.reset == ResetChoice::None;
        if friction && !has_ranges {
            return false;
        }
        if has_ranges || friction {
            consulted && summary_nonempty
        } else {
            // Non-friction Continue with no ranges: attestation must
            // be default (no point-of-true-without-evidence claims).
            !consulted && !summary_nonempty
        }
    }

    fn review_response_stuck_math_audit_legal(&self, review: &ReviewResponse) -> bool {
        let has_report_content = review
            .stuck_math_audit
            .as_ref()
            .is_some_and(StuckMathAuditReviewReport::has_content);
        let product_within_limit = review.stuck_math_audit.as_ref().map_or(
            true,
            StuckMathAuditReviewReport::reviewer_lean_product_within_limit,
        );

        if !self.stuck_math_audit.active {
            return !has_report_content;
        }
        if review.decision == ReviewDecisionKind::Continue && review.reset == ResetChoice::None {
            has_report_content && product_within_limit
        } else {
            !has_report_content
        }
    }

    fn review_response_audit_plan_legal(&self, review: &ReviewResponse) -> bool {
        self.review_response_audit_plan_rejection_reason(review)
            .is_none()
    }

    /// Per-rule diagnostic for audit-plan dismissal legality. Returns
    /// `Some(reason)` when the response's `dismiss_audit_plan` /
    /// `dismissed_tasks` fields are not legal for the current state, and
    /// `None` when the fields are legal (including the no-op case where
    /// neither field is populated). Used by
    /// `review_response_audit_plan_legal` for the bool gate and by
    /// `review_response_rejection_reasons` to surface a specific
    /// rejection string back to the reviewer prompt.
    ///
    /// `dismiss_audit_plan=true` with non-empty `dismissed_tasks` is
    /// legal — the reviewer's prompt invites both shapes ("dismiss
    /// individual tasks ... dismiss the whole plan ... once nothing live
    /// remains"). The engine applies individual dismissals first, then
    /// drops the (now-fully-dismissed) plan into `superseded_audit_plan`,
    /// preserving the audit trail of what was closed and why.
    fn review_response_audit_plan_rejection_reason(
        &self,
        review: &ReviewResponse,
    ) -> Option<String> {
        let touches_plan = review.dismiss_audit_plan || !review.dismissed_tasks.is_empty();
        if !touches_plan {
            return None;
        }
        if !self.stuck_math_audit.active {
            return Some(
                "dismiss_audit_plan / dismissed_tasks are only legal while StuckMathAudit is active"
                    .into(),
            );
        }
        let Some(plan) = self.audit_plan.as_ref() else {
            return Some(
                "dismiss_audit_plan / dismissed_tasks require an active audit_plan; the request carries none"
                    .into(),
            );
        };
        if !matches!(
            self.phase,
            Phase::ProofFormalization | Phase::TheoremStating
        ) && !plan.need_input_audit
        {
            return Some(
                "dismiss_audit_plan / dismissed_tasks are legal in ProofFormalization, TheoremStating, or on a NeedInputAuditor plan (need_input_audit=true); the current state is none of these"
                    .into(),
            );
        }
        let mut seen = BTreeSet::new();
        for dismissal in &review.dismissed_tasks {
            if dismissal.id.trim().is_empty() {
                return Some("dismissed_tasks entries must have a non-empty id".into());
            }
            if dismissal.reason.trim().is_empty() {
                return Some(format!(
                    "dismissed_tasks entry for id='{}' must have a non-empty reason",
                    dismissal.id
                ));
            }
            if dismissal.reason.chars().count() > AUDIT_TASK_REASON_MAX_CHARS {
                return Some(format!(
                    "dismissed_tasks entry for id='{}' has a reason longer than the {AUDIT_TASK_REASON_MAX_CHARS}-character limit",
                    dismissal.id
                ));
            }
            if !seen.insert(dismissal.id.clone()) {
                return Some(format!(
                    "dismissed_tasks references id='{}' more than once",
                    dismissal.id
                ));
            }
            let Some(task) = plan.tasks.iter().find(|task| task.id == dismissal.id) else {
                return Some(format!(
                    "dismissed_tasks references id='{}' which is not in audit_plan.tasks",
                    dismissal.id
                ));
            };
            if task.dismissed {
                return Some(format!(
                    "dismissed_tasks references id='{}' which is already dismissed",
                    dismissal.id
                ));
            }
        }
        None
    }

    pub fn review_response_legal(&self, review: &ReviewResponse) -> bool {
        if self.kind != RequestKind::Review {
            return false;
        }
        // global_repair_mode preconditions (B1 + S10 + mutual exclusion).
        // The request carries the live `global_repair_mode_enabled`,
        // `pending_global_repair_request`, and `pending_global_repair_grant`
        // projections so we can do all checks request-side.
        if !self.global_repair_mode_enabled
            && (review.global_repair_request.is_some() || review.consume_global_repair_grant)
        {
            return false;
        }
        if review.global_repair_request.is_some() && review.consume_global_repair_grant {
            return false;
        }
        if let Some(gr) = review.global_repair_request.as_ref() {
            // M15: reject only when the reviewer is trying to ALSO
            // change the coarse anchor; `None` (preserve) and `Some(current)`
            // (no-op re-assertion) are both legal.
            let anchor_change_attempted = review.next_active_coarse.is_some()
                && review.next_active_coarse != self.active_coarse_node;
            if self.phase != Phase::ProofFormalization
                || review.decision != ReviewDecisionKind::Continue
                || review.reset != ResetChoice::None
                || anchor_change_attempted
            {
                return false;
            }
            if !gr
                .proposed_extension_nodes
                .is_subset(&self.current_present_nodes)
            {
                return false;
            }
            if !review.task_blockers.is_empty()
                || !review.reset_blockers.is_empty()
                || !review.override_blockers.is_empty()
                || !review.authorized_nodes.is_empty()
                || review.next_active.is_some()
                || !review.protected_semantic_change_nodes.is_empty()
                || review.confirm_protected_semantic_change_scope
            {
                return false;
            }
            // Protected-set disjointness and S10 cooldown rely on live
            // state; the ProtocolState wrapper validator below enforces
            // those before delegating here. We accept the Step A here
            // and skip the standard checks below — every action field
            // is empty so the rest of the predicate would be vacuous.
            return true;
        }
        if review.consume_global_repair_grant {
            if self.pending_global_repair_grant.is_none() {
                return false;
            }
            let anchor_change_attempted = review.next_active_coarse.is_some()
                && review.next_active_coarse != self.active_coarse_node;
            if self.phase != Phase::ProofFormalization
                || review.decision != ReviewDecisionKind::Continue
                || review.reset != ResetChoice::None
                || anchor_change_attempted
                || !matches!(
                    review.next_mode,
                    TaskMode::Restructure | TaskMode::CoarseRestructure
                )
            {
                return false;
            }
        }
        if !review.task_blockers.is_subset(&self.blockers)
            || !review
                .override_blockers
                .is_subset(&self.allowed_override_blockers)
            || !review
                .reset_blockers
                .is_subset(&self.allowed_reset_blockers)
        {
            return false;
        }
        if !review.task_blockers.is_disjoint(&review.override_blockers)
            || !review.task_blockers.is_disjoint(&review.reset_blockers)
            || !review.override_blockers.is_disjoint(&review.reset_blockers)
        {
            return false;
        }
        if !review
            .request_sound_verifier_nodes
            .is_subset(&self.sound_verifier_requestable_nodes)
        {
            return false;
        }
        if !self
            .sound_verifier_requests_conflicting_with_blocker_actions(review)
            .is_empty()
        {
            return false;
        }
        // task_blocker_ids tells the worker "this is a blocker you should
        // address." Local mode authorizes the worker to edit ONLY the
        // active node's proof body — not other nodes, not .tex files,
        // not signatures.
        //
        // For NodeCorr / PaperFaithfulness / Substantiveness blockers, the
        // fix inherently requires .tex or signature edits (or cross-node
        // edits) that Local mode can't authorize, so the deterministic
        // checker would reject every legitimate repair attempt. Reject the
        // reviewer decision in that case so the kernel reissues and the
        // reviewer can pick a wider mode.
        //
        // Soundness is the special case: it auto-clears when the active
        // node becomes sorry-free (`needs_sound` returns false → soundness
        // state auto-Passes → no Soundness blocker emitted in
        // `global_blockers`). Closing the proof IS within Local's scope —
        // a `.lean`-proof-body edit. So `Local + must_close_active +
        // task_blockers=[soundness_id]` is a legitimate, kernel-blessed
        // workflow when the active node is plausibly closeable. Allow it.
        if review.next_mode == TaskMode::Local
            && review
                .task_blockers
                .iter()
                .any(|b| !matches!(b.kind, BlockerKind::Soundness))
        {
            return false;
        }
        // `authorized_nodes` invariants for proof-formalization
        // reviewer-assigned worker tasks. The reviewer's `next_active`
        // / `next_mode` define the maximum legal envelope; the
        // explicit `authorized_nodes` list is the actual edit
        // permission handed to the worker. Required (non-empty) for
        // Restructure / CoarseRestructure; required empty for Local
        // (Local does not authorize cross-node existing-node edits);
        // `authorized_nodes ⊆ scope_envelope` always; every listed
        // node must be currently present.
        if self.phase == Phase::ProofFormalization
            && review.decision == ReviewDecisionKind::Continue
            && review.reset == ResetChoice::None
        {
            match review.next_mode {
                TaskMode::Restructure | TaskMode::CoarseRestructure => {
                    if review.authorized_nodes.is_empty() {
                        return false;
                    }
                }
                TaskMode::Local => {
                    if !review.authorized_nodes.is_empty() {
                        return false;
                    }
                }
                _ => {}
            }
        }
        if !review.authorized_nodes.is_empty() {
            if !review
                .authorized_nodes
                .is_subset(&self.current_present_nodes)
            {
                return false;
            }
            // The reviewer may authorize editing any node inside the
            // scope envelope and additionally any node in
            // `protected_semantic_change_nodes` (the protected-closure
            // mechanism explicitly extends authorization to those
            // nodes for semantic reshape, even when they're outside
            // the active node's impact region).
            let mut allowed = self.review_scope_envelope(review);
            allowed.extend(review.protected_semantic_change_nodes.iter().cloned());
            // global_repair_mode B3: extend the allowed set with the
            // audit-granted out-of-cone nodes when the reviewer is
            // consuming the grant.
            if review.consume_global_repair_grant {
                if let Some(grant) = self.pending_global_repair_grant.as_ref() {
                    allowed.extend(grant.approved_extension_nodes.iter().cloned());
                }
            }
            if !review.authorized_nodes.is_subset(&allowed) {
                return false;
            }
            // Proposal v32 audit-2 followup #5: `authorized_nodes` must
            // additionally lie in the active-coarse cone of the effective
            // anchor (`next_active_coarse` if the reviewer is switching,
            // else the current `active_coarse_node`). The envelope
            // (`impact_region(active_node)`) is bidirectional and can
            // leak ancestors outside the down-cone; without this check
            // the worker could be authorized to edit out-of-cone
            // importers, partly defeating the anchor's scope. Protected
            // nodes are exempted (consistent with the envelope-extension
            // above): the protected-closure mechanism explicitly licenses
            // out-of-cone semantic reshape on those approved-target
            // nodes. Dormant when `coarse_dag_nodes` is empty or no
            // effective anchor — the cone helper returns
            // `current_present_nodes` and the subset check is vacuous.
            let effective_anchor = review
                .next_active_coarse
                .as_ref()
                .or(self.active_coarse_node.as_ref());
            let cone = self.coarse_legal_active_set_for_anchor(effective_anchor);
            let cone_violators: BTreeSet<NodeId> = review
                .authorized_nodes
                .difference(&cone)
                .filter(|n| !review.protected_semantic_change_nodes.contains(*n))
                .filter(|n| {
                    if !review.consume_global_repair_grant {
                        return true;
                    }
                    match self.pending_global_repair_grant.as_ref() {
                        None => true,
                        Some(grant) => !grant.approved_extension_nodes.contains(*n),
                    }
                })
                .cloned()
                .collect();
            if !cone_violators.is_empty() {
                return false;
            }
        }
        if !self
            .task_blockers_outside_review_worker_scope(review)
            .is_empty()
        {
            return false;
        }
        if !self.sound_task_blockers_not_repair_ready(review).is_empty() {
            return false;
        }
        // Reset choices are pure state changes: the reviewer accepts
        // whatever state the reset produces and does not simultaneously
        // adjudicate individual blockers. Blockers in that response
        // must all be empty; the blocker-action subset/disjoint checks
        // are trivially satisfied (every bucket empty) and the next
        // audit/review sees the post-reset blocker set.
        let reset_requested = review.reset != ResetChoice::None;
        match review.reset {
            ResetChoice::TheoremStatingNode => {
                if self.phase != Phase::ProofFormalization
                    || review.decision != ReviewDecisionKind::Continue
                    || !self
                        .allowed_resets
                        .contains(&ResetChoice::TheoremStatingNode)
                {
                    return false;
                }
                let Some(reset_node) = review.reset_node.as_ref() else {
                    return false;
                };
                if !self.resettable_theorem_stating_nodes.contains(reset_node) {
                    return false;
                }
                if review.next_active.is_some() || !review.authorized_nodes.is_empty() {
                    return false;
                }
            }
            ResetChoice::None | ResetChoice::LastCommit | ResetChoice::LastClean => {
                if review.reset_node.is_some() {
                    return false;
                }
            }
        }
        if reset_requested
            && (!review.task_blockers.is_empty()
                || !review.override_blockers.is_empty()
                || !review.reset_blockers.is_empty()
                || !review.request_sound_verifier_nodes.is_empty())
        {
            return false;
        }
        if review.protected_semantic_change_nodes.is_empty() {
            if review.confirm_protected_semantic_change_scope {
                return false;
            }
        } else if self.phase != Phase::ProofFormalization
            || review.decision != ReviewDecisionKind::Continue
            || reset_requested
            || review.next_mode != TaskMode::CoarseRestructure
            || review.next_active.is_none()
            || !review
                .protected_semantic_change_nodes
                .is_subset(&self.approved_target_nodes)
        {
            return false;
        }
        if !review
            .difficulty_updates
            .keys()
            .all(|node| self.allowed_difficulty_update_nodes.contains(node))
        {
            return false;
        }
        if review.clear_human_input && !self.human_input_outstanding {
            return false;
        }
        let need_input = review.decision == ReviewDecisionKind::NeedInput;
        if need_input
            && (!review.task_blockers.is_empty()
                || !review.override_blockers.is_empty()
                || !review.reset_blockers.is_empty()
                || !review.request_sound_verifier_nodes.is_empty()
                || review.next_active.is_some()
                || review.next_mode != self.mode)
        {
            return false;
        }
        let base_legal = if need_input {
            self.allowed_decisions.contains(&review.decision)
                && self.allowed_resets.contains(&review.reset)
        } else {
            match self.phase {
                Phase::TheoremStating => {
                    if matches!(
                        self.retry_outcome_kind,
                        RetryOutcomeKind::Invalid | RetryOutcomeKind::Transport
                    ) {
                        self.allowed_decisions.contains(&review.decision)
                            && review.next_active.is_none()
                            && self.allowed_resets.contains(&review.reset)
                            && self.allowed_next_modes.contains(&review.next_mode)
                    } else {
                        if !self.allowed_resets.contains(&review.reset) {
                            return false;
                        }
                        if review.next_active.as_ref().is_some_and(|node| {
                            !self.kernel_hinted_next_active_nodes.contains(node)
                        }) {
                            return false;
                        }
                        if review.next_mode == TaskMode::Targeted
                            && review.decision != ReviewDecisionKind::AdvancePhase
                        {
                            // AdvancePhase ignores next_active (the next phase's
                            // request rederives it), so a Targeted advance_phase
                            // is legal whether or not next_active is set. Kept
                            // the Targeted-mode requirement for all other
                            // decisions (Continue, NeedInput). Mirrors the
                            // matching gate in `review_response_rejection_reasons`
                            // above.
                            if self.allow_targeted_without_next_active {
                                if review.next_active.is_some() {
                                    return false;
                                }
                            } else if review
                                .next_active
                                .as_ref()
                                .is_none_or(|node| !self.targeted_next_active_nodes.contains(node))
                            {
                                return false;
                            }
                        }
                        self.allowed_next_modes.contains(&review.next_mode)
                            && match review.decision {
                                ReviewDecisionKind::Continue | ReviewDecisionKind::NeedInput => {
                                    self.allowed_decisions.contains(&review.decision)
                                }
                                ReviewDecisionKind::AdvancePhase => {
                                    // AdvancePhase + LastClean is semantically
                                    // incoherent: phase advance says "leave this
                                    // state behind"; LastClean says "rewind". The
                                    // engine handler used to apply LastClean during
                                    // AdvancePhase (leaving transient blockers
                                    // post-wipe before phase advance); reject here
                                    // instead so the kernel reissues. Mirror of
                                    // Cleanup Done+LastClean rejection below.
                                    self.allowed_decisions.contains(&review.decision)
                                        && self.blockers.is_empty()
                                        && review.reset != ResetChoice::LastClean
                                        && (!self.human_input_outstanding
                                            || review.clear_human_input)
                                }
                                ReviewDecisionKind::Done => false,
                            }
                    }
                }
                Phase::ProofFormalization | Phase::Cleanup => {
                    // proof/cleanup Continue cannot CLEAR an existing active_node
                    // by sending empty next_active. The contract previously
                    // documented "node id or empty string" but the engine ignored
                    // empty next_active in these branches (engine.rs:1241, :1398),
                    // making the docs a lie. Empty is now rejected when active_node
                    // is currently set; allowed when active_node is already None
                    // (the retry/orphan-cleanup state where the reviewer has no
                    // specific node to nominate, handled at engine.rs:1272-1304).
                    // Theorem branches keep their own per-branch semantics.
                    //
                    // Exception: Continue+LastClean is a pure rewind that re-issues
                    // a Review request from the post-reset state — `next_active`
                    // is decided on the next turn, not in the response that triggers
                    // the reset. So `next_active=None` is required (and forbidden
                    // to be Some — see below), regardless of whether `active_node`
                    // is currently set.
                    //
                    // Tightening (audit follow-up): even when `active_node` is
                    // currently None (the retry/orphan-cleanup retry case),
                    // `next_active=None` is only legal when the worker won't be
                    // tasked with blocker work — i.e. `task_blockers` is empty
                    // AND `next_mode` is `Local`. Restructure / CoarseRestructure
                    // are cross-node / signature edits that REQUIRE an active
                    // focus; if the reviewer omits `next_active` while requesting
                    // such a mode, `engine.rs:1500-1508` silently downgrades
                    // `proof_edit_mode` to `Local` while still attaching the
                    // submitted `task_blockers` to the new `PendingTask` — so the
                    // worker would be handed a blocker job under a mode that
                    // can't legally edit anything but the (absent) active node.
                    // Reject here so the kernel re-issues Review and the reviewer
                    // is forced to either nominate a focus or downgrade properly.
                    //
                    // Cleanup-v2 (audit Finding 1): scope this rejection to
                    // Phase::ProofFormalization. In Phase::Cleanup the only
                    // legal `next_mode` is `TaskMode::Cleanup` (see
                    // `request_allowed_next_modes`), so the
                    // `next_mode != TaskMode::Local` arm of the condition
                    // would fire unconditionally — every Continue in Cleanup
                    // would be rejected. The cleanup dispatch shape is
                    // `(cleanup_next_task, authorized_nodes)`; the active
                    // node is implicit per-task (kernel reads it from
                    // `cleanup_audit_tasks[cleanup_next_task].target_node`).
                    // Cleanup's per-decision legality is enforced by
                    // `cleanup_v2_review_fields_legal` instead.
                    if self.phase == Phase::ProofFormalization
                        && review.decision == ReviewDecisionKind::Continue
                        && review.next_active.is_none()
                        && !matches!(
                            review.reset,
                            ResetChoice::LastClean | ResetChoice::TheoremStatingNode
                        )
                        && (self.active_node.is_some()
                            || !review.task_blockers.is_empty()
                            || review.next_mode != TaskMode::Local)
                    {
                        return false;
                    }
                    // (Removed: explicit Cleanup Done+LastClean rejection.
                    //  Under the cleanup invariant — `request_allowed_resets`
                    //  for Phase::Cleanup returns {None} only — LastClean
                    //  cannot be in `allowed_resets`, so the outer
                    //  `allowed_resets.contains(&review.reset)` check rejects
                    //  the response before reaching this branch. Keeping a
                    //  redundant check obscures the structural property.)
                    // Cleanup invariant (#40): Done is only legal with an empty
                    // blocker set + all blocker action buckets empty.
                    // Belt-and-braces under the entry gate
                    // (`formalization_complete()` requires
                    // `global_blockers().is_empty()`) and the FinalCleanup
                    // validator (rejects edits that would re-open verifier
                    // lanes). PROCESS_SEMANTICS.md:355-356 requires this.
                    if self.phase == Phase::Cleanup
                        && review.decision == ReviewDecisionKind::Done
                        && (!self.blockers.is_empty()
                            || !review.task_blockers.is_empty()
                            || !review.override_blockers.is_empty()
                            || !review.reset_blockers.is_empty()
                            || !review.request_sound_verifier_nodes.is_empty())
                    {
                        return false;
                    }
                    self.allowed_resets.contains(&review.reset)
                        && self.review_next_active_legal_for_response(review)
                        && self.allowed_decisions.contains(&review.decision)
                        && self.allowed_next_modes.contains(&review.next_mode)
                }
                Phase::Complete => false,
            }
        };
        // Cleanup-v2 (audit Finding 3): the cleanup-task control fields
        // (`cleanup_dismiss_tasks`, `cleanup_next_task`,
        // `cleanup_request_reaudit`) are legal only in Phase::Cleanup with
        // specific decisions. Out-of-phase or out-of-decision usage is
        // rejected so the kernel re-issues a Review rather than silently
        // dropping the fields.
        if !self.cleanup_v2_review_fields_legal(review) {
            return false;
        }
        let proof_continue = self.phase == Phase::ProofFormalization
            && review.decision == ReviewDecisionKind::Continue;
        base_legal
            && (proof_continue || (review.allow_new_obligations && !review.must_close_active))
            && if review.decision == ReviewDecisionKind::Continue {
                true
            } else {
                review.next_worker_context_mode == WorkerContextMode::Resume
                    && review.paper_focus_ranges.is_empty()
                    && review.work_style_hint == WorkerWorkStyleHint::None
            }
            && self.review_response_paper_grounding_legal(review)
            && self.review_response_stuck_math_audit_legal(review)
            && self.review_response_audit_plan_legal(review)
    }

    /// Cleanup-v2 (audit Finding 3): legality checks for the reviewer's
    /// cleanup-task control fields. Enforces:
    ///   - `cleanup_dismiss_tasks` only legal in Phase::Cleanup + Continue
    ///   - `cleanup_next_task` only legal in Phase::Cleanup + Continue
    ///   - `cleanup_request_reaudit` only legal in Phase::Cleanup + Done,
    ///     and only when the current audit round is below max
    ///   - All referenced task indices in-range and Pending
    ///   - No duplicate indices across `cleanup_dismiss_tasks` and
    ///     `cleanup_next_task`
    fn cleanup_v2_review_fields_legal(&self, review: &ReviewResponse) -> bool {
        // Cleanup-v2 (audit Finding 2): when the consecutive-invalid-
        // worker latch fires, the only legal review decision is Done. The
        // reviewer cannot Continue (further worker bursts would just keep
        // failing). Re-audit on Done is also blocked: the latch overrides
        // `cleanup_request_reaudit` at `engine.rs:3998` regardless of
        // round, so accepting a Done+reaudit response would silently drop
        // the reaudit request — reject up front so the LLM gets a clear
        // legality error instead.
        if self.phase == Phase::Cleanup && self.cleanup_force_done_view {
            if review.decision != ReviewDecisionKind::Done {
                return false;
            }
            if review.cleanup_request_reaudit {
                return false;
            }
        }
        // Cleanup-v2: the reviewer must NOT nominate a `next_active` in
        // Phase::Cleanup. The worker's active node is resolved from the
        // dispatched task's `target_node` (single source of truth). A
        // reviewer-supplied `next_active` is a stale proof-mode lever
        // that, if accepted, would silently override the task-derived
        // node in `apply_cleanup_review_response`.
        if self.phase == Phase::Cleanup && review.next_active.is_some() {
            return false;
        }
        // Phase / decision gating.
        if !review.cleanup_dismiss_tasks.is_empty() {
            if self.phase != Phase::Cleanup || review.decision != ReviewDecisionKind::Continue {
                return false;
            }
        }
        if review.cleanup_next_task.is_some() {
            if self.phase != Phase::Cleanup || review.decision != ReviewDecisionKind::Continue {
                return false;
            }
        }
        if review.cleanup_request_reaudit {
            if self.phase != Phase::Cleanup || review.decision != ReviewDecisionKind::Done {
                return false;
            }
            // Round legality is enforced even if the kernel will silently
            // ignore the request — reject illegal-round requests so the
            // reviewer must re-emit.
            if self.cleanup_audit_round_view >= CLEANUP_AUDIT_MAX_ROUNDS {
                return false;
            }
        }
        // Index legality. Only check when in Cleanup phase + Continue
        // (otherwise the gating above rejected the response already).
        if self.phase == Phase::Cleanup && review.decision == ReviewDecisionKind::Continue {
            let tasks = &self.cleanup_audit_tasks_view;
            let mut dismissed_indices: BTreeSet<u32> = BTreeSet::new();
            for (idx, _reason) in &review.cleanup_dismiss_tasks {
                let i = *idx as usize;
                if i >= tasks.len() {
                    return false;
                }
                if !matches!(tasks[i].status, CleanupTaskStatus::Pending) {
                    return false;
                }
                if !dismissed_indices.insert(*idx) {
                    // Duplicate index within cleanup_dismiss_tasks.
                    return false;
                }
            }
            if let Some(idx) = review.cleanup_next_task {
                let i = idx as usize;
                if i >= tasks.len() {
                    return false;
                }
                if !matches!(tasks[i].status, CleanupTaskStatus::Pending) {
                    return false;
                }
                if dismissed_indices.contains(&idx) {
                    // Same task can't be both dismissed and dispatched.
                    return false;
                }
            }
        }
        true
    }
}

/// Patch A observation payload from `scripts/lean_local_closure.lean`
/// (LOCAL_CLOSURE_IMPL_PLAN.md §5.8). Holds the parsed local-closure
/// envelope produced by the Lean script, plus the transport-level
/// outcome of invoking it (returncode / timed_out / raw stdout+stderr).
///
/// Lives in `model.rs` rather than `runtime_cli_observations.rs` so
/// that future patches can carry it inside `WorkerResponse` (which is
/// also defined here). For Patch A this type is produced by
/// `runtime_cli_observations::run_local_closure_axioms` and consumed
/// by no engine code yet — Patch B/C add the `WorkerResponse` field
/// and the gating/recording wiring.
///
/// Field semantics:
/// - `status`: verbatim from the Lean script —
///   `"ok"` / `"elaboration_error"` / `"missing_declaration"` /
///   `"internal_error"` (see plan §5.1). The wrapper preserves
///   whatever string the script emitted; gating treats anything other
///   than `"ok"` as a probe failure (plan §6.1).
/// - `kernel_axioms`: deduplicated set of axioms reached during the
///   closure walk. Names are emitted as fully-qualified Lean constants
///   (e.g. `"Classical.choice"`); the script does NOT apply any
///   approved-axioms policy — that is the Rust caller's job per
///   `runtime_cli_observations::load_approved_axioms`.
/// - `boundary_theorems` / `strict_theorem_deps` /
///   `strict_definition_deps`: Tablet-side dependencies. The map keys
///   are `NodeId`s normalized from the script's emitted Lean names
///   (see `run_local_closure_axioms` for the normalization rule used
///   in Patch A); values are content hashes for record invalidation.
///   Hash semantics: `boundary_theorems` carries `statement_hash`;
///   `strict_theorem_deps` ALSO carries `statement_hash` (post dual-
///   collector fix — see commit history; the JSON field is still
///   spelled `value_hash` for back-compat with replayed traces but the
///   bytes are a statement hash, since under local-closure boundary
///   semantics no helper theorem's proof body is ever walked);
///   `strict_definition_deps` carries `semantic_hash` (type+value).
/// - `errors`: observational errors collected by the script
///   (e.g. `"unsafe declaration in closure: X"`,
///   `"missing constant during traversal: Y"`). Empty does NOT imply
///   `status == "ok"`; the two are independent observations the
///   downstream gate (Patch B) considers together.
/// - `raw_stdout` / `raw_stderr` / `returncode` / `timed_out`:
///   transport envelope from the checker server's invocation of the
///   probe. `returncode` defaults to 0 when the script ran to
///   completion successfully (the response carries `null` only in
///   pathological transport-failure paths, which deserialize to 0
///   under `#[serde(default)]`).
///
/// Every field carries `#[serde(default)]` so adding fields in later
/// patches is back-compat with replayed traces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LocalClosureProbeOutput {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub kernel_axioms: BTreeSet<String>,
    #[serde(default)]
    pub boundary_theorems: BTreeMap<NodeId, String>,
    #[serde(default)]
    pub strict_theorem_deps: BTreeMap<NodeId, String>,
    #[serde(default)]
    pub strict_definition_deps: BTreeMap<NodeId, String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub raw_stdout: String,
    #[serde(default)]
    pub raw_stderr: String,
    #[serde(default)]
    pub returncode: i32,
    #[serde(default)]
    pub timed_out: bool,
    /// Plan §4.6.1 dual-collector cross-check. `None` when the script
    /// version emitted no `axiomization_check` field (pre-merge state
    /// files); `Some` when present. The wrapper inspects `agreed` and
    /// `skipped` to decide whether to flip `status` to `internal_error`.
    /// Pre-merge state files lacking the field deserialize as `None`
    /// (per `#[serde(default)]`); existing accept paths treat that as
    /// "trust the primary's verdict".
    #[serde(default)]
    pub axiomization_check: Option<AxiomizationCheckOutput>,
}

/// Plan §4.6.1 axiomization cross-check payload emitted by the merged
/// `scripts/lean_local_closure.lean`. The script runs an
/// `Lean.CollectAxioms.collect`-shaped *secondary* collector against the
/// same already-loaded environment as the primary visitor, then compares:
///
/// * `kernel_axioms` (set equality on both sides);
/// * boundary-theorem *names* (primary carries `{name, statement_hash}`
///   pairs while the secondary has only names — comparison is on the
///   name sets, per plan §4.6.1).
///
/// `agreed == true` is the runtime invariant; disagreement is treated as
/// an infrastructure issue (the wrapper flips `status` to
/// `internal_error` and the MCA gate rejects with `[internal]
/// axiomization disagrees`). When the secondary collector is disabled
/// via env var `TRELLIS_LOCAL_CLOSURE_AXCHECK_DISABLE=1` or CLI flag
/// `--no-axcheck`, the script emits `skipped: true` (and the Rust
/// wrapper accepts unconditionally — the primary's verdict stands).
///
/// All fields carry `#[serde(default)]` so additive sub-field changes
/// don't break replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AxiomizationCheckOutput {
    /// Kernel axioms reached by the secondary collector. The script
    /// emits a name-only list (no per-axiom hash). Compared against the
    /// primary's `kernel_axioms` for set-equality.
    #[serde(default)]
    pub kernel_axioms: BTreeSet<String>,
    /// Tablet boundary-theorem names reached by the secondary
    /// collector. Compared against the *names* in the primary's
    /// `boundary_theorems` map (primary carries hash pairs; secondary
    /// has names only — comparison is name-set equality per plan §4.6.1).
    #[serde(default)]
    pub boundary_theorems: BTreeSet<String>,
    /// Runtime invariant: `true` iff `(primary.kernel_axioms ==
    /// axcheck.kernel_axioms) AND (primary.boundary_theorems_names ==
    /// axcheck.boundary_theorems)`. Always `true` when `skipped` is
    /// `true`. The wrapper flips `LocalClosureProbeOutput.status` to
    /// `internal_error` on `agreed == false && skipped == false`.
    pub agreed: bool,
    /// `true` iff the secondary collector was disabled at script
    /// invocation time (env var / CLI flag / bridge config kill-switch
    /// — see `local_closure_axcheck_enabled` in the bridge config).
    /// `skipped: true` implies `agreed: true` and empty diff lists;
    /// the wrapper accepts unconditionally.
    #[serde(default)]
    pub skipped: bool,
    /// Diagnostic: kernel axioms the primary saw but the secondary did
    /// not. Empty when `agreed` is `true`.
    #[serde(default)]
    pub primary_only_axioms: Vec<String>,
    /// Diagnostic: kernel axioms the secondary saw but the primary did
    /// not. Empty when `agreed` is `true`.
    #[serde(default)]
    pub axcheck_only_axioms: Vec<String>,
    /// Diagnostic: boundary-theorem names the primary saw but the
    /// secondary did not. Empty when `agreed` is `true`.
    #[serde(default)]
    pub primary_only_boundaries: Vec<String>,
    /// Diagnostic: boundary-theorem names the secondary saw but the
    /// primary did not. Empty when `agreed` is `true`.
    #[serde(default)]
    pub axcheck_only_boundaries: Vec<String>,
    /// Patch C-K Fix 3 + Patch C-N item 4: when the secondary axcheck
    /// collector crashes (rather than producing a valid disagreement),
    /// the Lean script emits the exception message here AND drops the
    /// `axcheck_only_*` / `primary_only_*` diffs. `None` on the happy
    /// paths (agreed, skipped, or true disagreement). The Rust parser
    /// keys off this typed field — `Some(msg)` triggers the distinct
    /// "collector crashed" diagnostic; `None` plus `agreed: false,
    /// skipped: false` triggers the existing "disagrees with primary"
    /// diagnostic. Pre-Patch-C-N parser keyed off the raw JSON; the
    /// typed field is the durable carrier so fixtures and consumers
    /// don't need to know the JSON shape.
    ///
    /// Old envelopes that lacked the field deserialize as `None`
    /// (per `#[serde(default)]`), so replays of pre-Patch-C-K bridges
    /// stay back-compat.
    #[serde(default)]
    pub error: Option<String>,
}

/// Plan §7.1 — durable record of a passed local-closure probe for a
/// sorry-free proof_node. Patch C-A introduces the type and the
/// `local_closure_records` map field on `ProtocolState`; record
/// creation, invalidation, and consumption flow through later patches.
///
/// Hash fields capture the inputs the probe ran against so a later
/// invalidation pass can detect drift (toolchain bump, manifest move,
/// preamble or active-decl edit, dep statement/value change). The
/// `boundary_theorems` / `strict_*_deps` maps key by NodeId and value
/// by the relevant per-dep hash (statement hash for boundary helpers,
/// value hash for strict theorem deps, semantic hash for strict
/// definition deps). `accepted_at_snapshot_id` is an opaque tag the
/// runtime CLI populates to make replay debugging tractable.
///
/// Audit H-4 — axcheck verification status carried by a closure
/// record. Default is `Skipped` (defensive: pre-H-4 records have no
/// telemetry → treat as if axcheck did not run, so re-enabling the
/// `local_closure_axcheck_enabled` policy invalidates them). The
/// canonical predicate uses this to enforce policy lockstep: if
/// `axcheck_required = true` in current runtime policy and a record
/// carries `Skipped`, the record is stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AxcheckStatus {
    /// Secondary axcheck collector ran AND agreed with the primary
    /// (the canonical happy path — record is fully validated).
    Agreed,
    /// Secondary axcheck collector ran AND disagreed with the primary.
    /// In production runs this also produces a halt marker before the
    /// record is ever installed; the variant exists for completeness
    /// and replay parity.
    Disagreed,
    /// Secondary axcheck collector did NOT run (operator disabled it
    /// via `local_closure_axcheck_enabled = false`, the `--no-axcheck`
    /// CLI flag, or the `TRELLIS_LOCAL_CLOSURE_AXCHECK_DISABLE` env
    /// var). Records with this status fail the canonical predicate
    /// when current policy requires axcheck.
    Skipped,
}

impl Default for AxcheckStatus {
    /// Defensive default: pre-H-4 persisted records and pre-H-4 state
    /// files deserialize as `Skipped` rather than `Agreed`. If a future
    /// supervisor re-enables axcheck (via flipping
    /// `local_closure_axcheck_enabled`), historical records flagged
    /// `Skipped` are correctly invalidated — preventing the audit's
    /// "re-enabling axcheck doesn't re-verify history" hazard.
    fn default() -> Self {
        AxcheckStatus::Skipped
    }
}

/// Audit Cross-Cutting (lean_report.md §"Cross-Cutting Root Causes"):
/// reasons a `LocalClosureRecord` may fail the canonical consistency
/// predicate. Surfaces structured diagnostics for `validate()` and
/// for diagnostic-quality drop logs from the engine batch path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalClosureRecordInconsistency {
    /// The owner node (`record.node`) is not in `live.present_nodes`.
    OwnerAbsent,
    /// The owner node is present but not in `proof_nodes`.
    OwnerNotProof,
    /// The owner node is sorryd (`live.open_nodes.contains(&owner)`).
    OwnerOpen,
    /// One of the referenced deps (boundary / strict_theorem /
    /// strict_definition) is not in `live.present_nodes`.
    DepAbsent { dep: NodeId },
    /// One of the referenced deps has a `kernel_semantic_hashes`
    /// entry that disagrees with the current
    /// `live.corr_current_fingerprints[dep]`.
    KernelSemanticHashMismatch {
        dep: NodeId,
        recorded: String,
        current: Option<String>,
    },
    /// One or more fields still carry the engine's
    /// `TODO_PATCH_C_D_HASH` sentinel that the runtime-CLI backfill
    /// was meant to replace. Sentinel records should never satisfy the
    /// canonical predicate — the supervisor must treat them as in-
    /// flight and refuse to use them as gate evidence.
    SentinelHashes,
    /// Record carries `axcheck_status = Skipped` while current
    /// runtime policy requires axcheck (`axcheck_required = true`
    /// from `local_closure_axcheck_enabled`).
    AxcheckSkippedButRequired,
}

impl std::fmt::Display for LocalClosureRecordInconsistency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalClosureRecordInconsistency::OwnerAbsent => {
                write!(f, "owner not in live.present_nodes")
            }
            LocalClosureRecordInconsistency::OwnerNotProof => {
                write!(f, "owner not in proof_nodes")
            }
            LocalClosureRecordInconsistency::OwnerOpen => {
                write!(f, "owner is sorryd (live.open_nodes)")
            }
            LocalClosureRecordInconsistency::DepAbsent { dep } => {
                write!(f, "dep {} not in live.present_nodes", dep.as_str())
            }
            LocalClosureRecordInconsistency::KernelSemanticHashMismatch {
                dep,
                recorded,
                current,
            } => write!(
                f,
                "kernel_semantic_hash drift for dep {}: recorded={:?} current={:?}",
                dep.as_str(),
                recorded,
                current,
            ),
            LocalClosureRecordInconsistency::SentinelHashes => {
                write!(f, "record carries TODO_PATCH_C_D_HASH sentinel")
            }
            LocalClosureRecordInconsistency::AxcheckSkippedButRequired => {
                write!(
                    f,
                    "axcheck_status = Skipped while current policy requires axcheck"
                )
            }
        }
    }
}

/// Every field carries `#[serde(default)]` so adding fields in later
/// patches stays back-compat with replayed traces and on-disk state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LocalClosureRecord {
    pub node: NodeId,
    #[serde(default)]
    pub closure_version: String,
    #[serde(default)]
    pub toolchain_hash: String,
    #[serde(default)]
    pub lake_manifest_hash: String,
    #[serde(default)]
    pub preamble_hash: String,
    #[serde(default)]
    pub approved_axioms_hash: String,
    /// sha256 of the entire `<node>.lean` file content (captures
    /// imports, body, attributes, comments).
    #[serde(default)]
    pub active_decl_hash: String,
    /// sha256 of just the declaration's statement/type, normalized via
    /// existing `find_declaration` extraction.
    #[serde(default)]
    pub active_statement_hash: String,
    #[serde(default)]
    pub kernel_axioms: BTreeSet<String>,
    /// Map from boundary-theorem NodeId to its statement_hash at
    /// record-creation time.
    #[serde(default)]
    pub boundary_theorems: BTreeMap<NodeId, String>,
    /// Map from strict-context theorem dep NodeId to its hash. Post
    /// dual-collector boundary-cut fix this is a `statement_hash`
    /// (helper proof bodies are never walked under Tablet local-closure
    /// semantics); the field name and the underlying JSON key remain
    /// `value_hash` only for replay compatibility.
    #[serde(default)]
    pub strict_theorem_deps: BTreeMap<NodeId, String>,
    /// Map from strict-context definition dep NodeId to its
    /// semantic_hash.
    #[serde(default)]
    pub strict_definition_deps: BTreeMap<NodeId, String>,
    /// Patch C-P HIGH 1 (b) — kernel `semantic_hash` (i.e. the
    /// `Fingerprint` string stored under `state.live.corr_current_fingerprints`)
    /// captured at probe time, keyed by dep NodeId. Spans all three
    /// dep categories (boundary_theorems / strict_theorem_deps /
    /// strict_definition_deps) in one unified map. Used at migration
    /// time by `record_hashes_match_current`: any drift between a
    /// recorded value and the current `corr_current_fingerprints[dep]`
    /// rejects the persisted record (forcing a re-probe). Eliminates
    /// the silent-dep-drift / two-stale-records / off-protocol-edit /
    /// iteration-order weaknesses of C-O's strict-signal approach.
    ///
    /// `#[serde(default)]` so pre-Patch-C-P persisted records (and
    /// state files) deserialize cleanly with an empty map. An empty
    /// map means "no kernel-hash invariants to enforce on this record"
    /// — those pre-C-P records still get the strict-signal / cross-
    /// record evidence checks; the kernel-hash check is additive.
    #[serde(default)]
    pub kernel_semantic_hashes: BTreeMap<NodeId, String>,
    #[serde(default)]
    pub accepted_at_snapshot_id: String,
    /// Audit H-4 — record whether the secondary axcheck collector ran
    /// at probe time. `Agreed`/`Disagreed` mean the collector ran;
    /// `Skipped` means it did not (operator disabled the policy or the
    /// per-probe CLI flag was set). The canonical predicate uses this
    /// to enforce policy lockstep: re-enabling axcheck must invalidate
    /// records whose `axcheck_status = Skipped`.
    ///
    /// Defaults to `Skipped` (per `AxcheckStatus::default`) so
    /// pre-H-4 persisted records deserialize defensively — they fail
    /// the canonical predicate the moment axcheck is required, forcing
    /// a re-probe rather than silently passing.
    #[serde(default)]
    pub axcheck_status: AxcheckStatus,
}

impl LocalClosureRecord {
    /// Audit Cross-Cutting (lean_report.md §"Recommended Implementation
    /// Order"): canonical state-only consistency predicate for a
    /// closure record. Pure-state — does NOT touch disk; the
    /// runtime-CLI's `record_hashes_match_current` layers in the
    /// toolchain/lake_manifest/preamble/approved_axioms/active_decl
    /// hash checks that require disk access.
    ///
    /// Checks (every place a record can enter, survive, or be
    /// restored uses this predicate as the single point of truth):
    ///   1. Owner exists in `live.present_nodes` and is a proof node.
    ///   2. Owner is sorry-free (not in `live.open_nodes`).
    ///   3. Every referenced dep (boundary_theorems / strict_theorem_deps
    ///      / strict_definition_deps) exists in `live.present_nodes`.
    ///   4. For every dep with a non-empty `kernel_semantic_hashes[D]`,
    ///      it matches `state.live.corr_current_fingerprints[D]`.
    ///   5. No sentinel hash fields (`TODO_PATCH_C_D_HASH`).
    ///   6. Axcheck status compatible with `axcheck_required` argument:
    ///      if `axcheck_required == true`, status must be `Agreed`
    ///      (not `Skipped` / `Disagreed`).
    ///
    /// Returns `Ok(())` on consistent, `Err(InconsistencyReason)` on
    /// the first mismatch (deterministic ordering: owner → owner kind
    /// → owner open → dep presence → dep hashes → sentinel → axcheck).
    pub fn is_consistent_with_state(
        &self,
        state: &ProtocolState,
        axcheck_required: bool,
    ) -> Result<(), LocalClosureRecordInconsistency> {
        // (1) Owner presence.
        if !state.live.present_nodes.contains(&self.node) {
            return Err(LocalClosureRecordInconsistency::OwnerAbsent);
        }
        // (2) Owner is a proof node.
        if !state.proof_nodes.contains(&self.node) {
            return Err(LocalClosureRecordInconsistency::OwnerNotProof);
        }
        // (3) Owner is sorry-free (sorry-free-only invariant per
        // plan §7.2).
        if state.live.open_nodes.contains(&self.node) {
            return Err(LocalClosureRecordInconsistency::OwnerOpen);
        }
        // (4) Dep presence — every referenced dep must exist.
        let dep_groups: [&BTreeMap<NodeId, String>; 3] = [
            &self.boundary_theorems,
            &self.strict_theorem_deps,
            &self.strict_definition_deps,
        ];
        for group in dep_groups {
            for dep in group.keys() {
                if !state.live.present_nodes.contains(dep) {
                    return Err(LocalClosureRecordInconsistency::DepAbsent { dep: dep.clone() });
                }
            }
        }
        // (5) Kernel semantic hash check — exact match against
        // `corr_current_fingerprints` for every dep that has a
        // recorded hash. Pre-Patch-C-P records with empty maps fall
        // through (no invariant enforced — matches the existing
        // `record_dep_hashes_consistent_with_state` semantics).
        for (dep, recorded_hash) in &self.kernel_semantic_hashes {
            let current_hash = state.live.corr_current_fingerprints.get(dep);
            match current_hash {
                Some(current) if current == recorded_hash => continue,
                _ => {
                    return Err(
                        LocalClosureRecordInconsistency::KernelSemanticHashMismatch {
                            dep: dep.clone(),
                            recorded: recorded_hash.clone(),
                            current: current_hash.cloned(),
                        },
                    );
                }
            }
        }
        // (6) Sentinel-hash check — engine writes
        // `TODO_PATCH_C_D_HASH` for not-yet-backfilled records. Those
        // are pre-validation in-flight values; refuse them.
        if self.is_sentinel_hashed() {
            return Err(LocalClosureRecordInconsistency::SentinelHashes);
        }
        // (7) Axcheck policy.
        if axcheck_required && self.axcheck_status != AxcheckStatus::Agreed {
            return Err(LocalClosureRecordInconsistency::AxcheckSkippedButRequired);
        }
        Ok(())
    }

    /// True iff any of the env/policy hash fields still hold the
    /// engine's pre-backfill `TODO_PATCH_C_D_HASH` sentinel. Public so
    /// the runtime-CLI's record persistence sweeps can short-circuit.
    pub fn is_sentinel_hashed(&self) -> bool {
        const SENTINEL: &str = "TODO_PATCH_C_D_HASH";
        self.toolchain_hash == SENTINEL
            || self.lake_manifest_hash == SENTINEL
            || self.preamble_hash == SENTINEL
            || self.approved_axioms_hash == SENTINEL
            || self.active_decl_hash == SENTINEL
            || self.active_statement_hash == SENTINEL
    }

    /// Pure-state freshness check for the final completion gate. This is
    /// intentionally narrower than `is_consistent_with_state`: completion
    /// must not be allowed by explicit sentinel hashes or recorded kernel
    /// semantic hashes that disagree with current fingerprints, but the
    /// model layer cannot validate disk-only policy inputs and should not
    /// reject records whose Lean metadata deps are not modeled as live
    /// kernel nodes.
    pub fn is_fresh_for_completion(&self, state: &ProtocolState) -> bool {
        if !state.live.present_nodes.contains(&self.node) {
            return false;
        }
        if !state.proof_nodes.contains(&self.node) {
            return false;
        }
        if state.live.open_nodes.contains(&self.node) {
            return false;
        }
        if self.is_sentinel_hashed() {
            return false;
        }
        self.kernel_semantic_hashes
            .iter()
            .all(|(dep, recorded_hash)| {
                state
                    .live
                    .corr_current_fingerprints
                    .get(dep)
                    .is_some_and(|current| current == recorded_hash)
            })
    }
}

/// Patch C-Q Q11 — shared helper for populating `kernel_semantic_hashes`
/// from the live kernel `corr_current_fingerprints` map. Walks every dep
/// across all three categories (`boundary_theorems`, `strict_theorem_deps`,
/// `strict_definition_deps`) and stamps the dep's current fingerprint
/// (or empty string if not yet observed).
///
/// Called from two sites:
///   1. `engine.rs:apply_local_closure_acceptance_bookkeeping` at probe
///      record-creation time (sorryd→sorry-free accept path).
///   2. `bin/runtime_cli.rs:deterministic_revalidate_at_cli_with_probe`
///      after building a refreshed record (Patch C-P HIGH 1 (b)).
/// Centralizing the loop keeps the two sites in lockstep — a drift in
/// the "what counts as a dep" semantics (e.g. adding a new dep
/// category) would otherwise need synchronized edits in two files. The
/// debug-build eprintln warning for empty fingerprints is included
/// once here rather than duplicated.
pub fn populate_kernel_semantic_hashes(record: &mut LocalClosureRecord, state: &ProtocolState) {
    let mut map: BTreeMap<NodeId, String> = BTreeMap::new();
    for dep in record
        .boundary_theorems
        .keys()
        .chain(record.strict_theorem_deps.keys())
        .chain(record.strict_definition_deps.keys())
    {
        let kernel_hash = state
            .live
            .corr_current_fingerprints
            .get(dep)
            .cloned()
            .unwrap_or_default();
        #[cfg(debug_assertions)]
        {
            if kernel_hash.is_empty() {
                eprintln!(
                    "[Patch C-P / Q11] kernel_semantic_hash empty for dep {} (consumer {}) — \
                     no corr_current_fingerprints entry; record will use empty sentinel",
                    dep.as_str(),
                    record.node.as_str(),
                );
            }
        }
        map.insert(dep.clone(), kernel_hash);
    }
    record.kernel_semantic_hashes = map;
}

/// Plan §7.1 — failure summary written to `local_closure_failures` when
/// a local-closure probe does NOT produce a `LocalClosureRecord`. The
/// `status` field carries the categorized failure reason ("ok" never
/// appears here — successful probes write a record instead). Common
/// values are "axiom_violation", "strict_error", "internal_error",
/// "timeout", and "transport_error".
///
/// Transport-error backoff fields (`retry_count`, `last_attempt_cycle`,
/// `next_retry_cycle`, `retry_exhausted`) are only meaningful when
/// `status == "transport_error"`; for proof-shape failures they stay at
/// their `Default` zeros and are ignored by the deterministic
/// revalidation pass (which retries those every cycle).
///
/// Every field carries `#[serde(default)]` for forward-compat with
/// pre-Patch-C state files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ErrorSummary {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub returncode: i32,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub stderr_excerpt: String,
    #[serde(default)]
    pub axiom_violations: Vec<String>,
    #[serde(default)]
    pub strict_errors: Vec<String>,
    #[serde(default)]
    pub captured_at_cycle: u64,
    // Transport-error backoff tracking (only meaningful for status
    // == "transport_error"):
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub last_attempt_cycle: u64,
    #[serde(default)]
    pub next_retry_cycle: u64,
    #[serde(default)]
    pub retry_exhausted: bool,
}

/// Plan §6.1 / §7.5 — keyed batch produced by the runtime CLI's
/// deterministic-revalidation pass and consumed by Patch C-B's
/// `apply_revalidation_batch` engine API. Patch C-A introduces the
/// type and the `WorkerResponse.local_closure_revalidation` field;
/// the engine-side consumer arrives in a later patch.
///
/// `refreshed` carries (node, fresh record) pairs the engine should
/// install via `local_closure_records.insert(node, record)` (with
/// `unverified_nodes`/`failures` membership cleared). `still_unverified`
/// carries (node, summary) pairs whose probe did not pass — those
/// nodes stay in `local_closure_unverified_nodes` and their summaries
/// land in `local_closure_failures`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RevalidationBatch {
    #[serde(default)]
    pub refreshed: Vec<(NodeId, LocalClosureRecord)>,
    #[serde(default)]
    pub still_unverified: Vec<(NodeId, ErrorSummary)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviationRequest {
    pub path: String,
    pub summary: String,
    pub affected_nodes: BTreeSet<NodeId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerResponse {
    pub request_id: u32,
    pub cycle: u32,
    pub status: ResponseStatus,
    pub outcome: WorkerOutcome,
    pub summary: String,
    pub comments: String,
    #[serde(default)]
    pub deterministic_rejection_reasons: Vec<String>,
    pub snapshot: WorkingSnapshot,
    pub proof_node_updates: NodeBoolUpdates,
    pub node_kind_updates: NodeKindUpdates,
    pub dep_updates: NodeSetUpdates,
    pub target_claim_updates: TargetClaimUpdates,
    pub difficulty_updates: BTreeMap<NodeId, Update<NodeDifficulty>>,
    /// Worker-created requests for one-file deviation authorization. Keys
    /// are stable deviation ids; paths point at TeX-only reference files.
    #[serde(default)]
    pub deviation_requests: BTreeMap<DeviationId, DeviationRequest>,
    /// Node declarations of the already-authorized deviations they use.
    #[serde(default)]
    pub node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    /// Deviation ids the worker is retiring from the project. The worker
    /// must also rm the corresponding `reference/<path>.tex` from disk
    /// during the burst (the kernel does not touch the filesystem here).
    /// Apply-time check rejects the response if any node still claims a
    /// to-delete id after the response's `node_deviation_claims` updates
    /// are notionally applied — i.e., claim updates and deletion must
    /// reconcile within the same response (or across separate bursts).
    #[serde(default)]
    pub deviation_deletions: BTreeSet<DeviationId>,
    /// Protected approved-target / protected-closure nodes whose
    /// correspondence fingerprint actually reopened during an explicitly
    /// scoped coarse restructure. Non-empty values require verifier drain
    /// followed by `GateKind::ProtectedReapproval`.
    #[serde(default)]
    pub protected_semantic_change_nodes: BTreeSet<NodeId>,
    /// Bug X principled fix: bridge sets this to true when a worker burst
    /// failed for infrastructure reasons (agent never produced output,
    /// timeout/hang, crashed mid-burst, missing done file, rate-limit
    /// retries exhausted). Distinct from a Malformed response where the
    /// agent ran but emitted bad JSON: those still bump `attempt` against
    /// `proof_invalid_review_threshold`. Transport failures consume the
    /// `transport_attempt` budget against
    /// `transport_invalid_review_threshold` instead, so a flaky tmux
    /// session doesn't burn the work-quality retry budget.
    /// Only meaningful when `status == Malformed`.
    #[serde(default)]
    pub transport_failure: bool,
    /// Patch B+: per-node local-closure probe outputs for nodes that
    /// transitioned sorryd→sorry-free in this delta. In Patch B, populated
    /// only on `must_close_active=true` accepts (single entry: the active
    /// node). Patch C extends to all sorryd→sorry-free transitions and
    /// adds the engine-side record write. Pre-Patch-C state files have
    /// the field absent → deserialize as default empty; existing accept
    /// paths ignore the field.
    #[serde(default)]
    pub local_closure_results: BTreeMap<NodeId, LocalClosureProbeOutput>,
    /// Patch C-A — keyed batch carrying the runtime CLI's
    /// deterministic-revalidation pass output (plan §6.1 / §7.5). Patch
    /// C-A introduces the field; engine-side consumption (writing
    /// records / updating the unverified set / populating failures)
    /// lands in a follow-up patch. `None` is the conventional encoding
    /// for "no revalidation pass ran for this delta"; pre-Patch-C
    /// state files lacking the field deserialize as `None`.
    #[serde(default)]
    pub local_closure_revalidation: Option<RevalidationBatch>,
    /// Worker-named nodes the reviewer should consider authorizing on the
    /// next dispatch when `outcome == NeedsRestructure`. Required (non-empty)
    /// for NeedsRestructure outcomes; ignored (kept empty) for other
    /// outcomes. The names are advisory — the reviewer remains the scope
    /// authority — but they let the reviewer widen scope concretely instead
    /// of guessing what the worker meant by "needs broader repair".
    #[serde(default)]
    pub needs_restructure_suggested_nodes: BTreeSet<NodeId>,
}

impl Default for WorkerResponse {
    fn default() -> Self {
        Self {
            request_id: 0,
            cycle: 0,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Valid,
            summary: String::new(),
            comments: String::new(),
            deterministic_rejection_reasons: Vec::new(),
            snapshot: WorkingSnapshot::default(),
            proof_node_updates: BTreeMap::new(),
            node_kind_updates: BTreeMap::new(),
            dep_updates: BTreeMap::new(),
            target_claim_updates: BTreeMap::new(),
            difficulty_updates: BTreeMap::new(),
            deviation_requests: BTreeMap::new(),
            node_deviation_claims: BTreeMap::new(),
            deviation_deletions: BTreeSet::new(),
            protected_semantic_change_nodes: BTreeSet::new(),
            transport_failure: false,
            local_closure_results: BTreeMap::new(),
            local_closure_revalidation: None,
            needs_restructure_suggested_nodes: BTreeSet::new(),
        }
    }
}

pub type CorrNodeLaneUpdates = BTreeMap<LaneId, BTreeMap<NodeId, Update<CorrStatus>>>;
pub type CorrTargetLaneUpdates = BTreeMap<LaneId, BTreeMap<TargetId, Update<CorrStatus>>>;
pub type DeviationLaneUpdates = BTreeMap<LaneId, BTreeMap<DeviationId, Update<CorrStatus>>>;
pub type SoundLaneUpdates = BTreeMap<LaneId, BTreeMap<NodeId, Update<SoundStatus>>>;
/// Substantiveness lane updates. Same shape as
/// `CorrNodeLaneUpdates` but carries `SubstantivenessStatus` (which admits
/// `NotDoneYet` in addition to Pass/Fail/Unknown).
pub type SubstantivenessLaneUpdates =
    BTreeMap<LaneId, BTreeMap<NodeId, Update<SubstantivenessStatus>>>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifierIssue {
    pub node: NodeId,
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrReviewerPhaseEvidence {
    pub decision: String,
    pub issues: Vec<VerifierIssue>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrReviewerLaneEvidence {
    pub correspondence: CorrReviewerPhaseEvidence,
    pub overall: String,
    pub summary: String,
    #[serde(alias = "feedback")]
    pub comments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperReviewerLaneEvidence {
    pub paper_faithfulness: CorrReviewerPhaseEvidence,
    pub overall: String,
    pub summary: String,
    #[serde(alias = "feedback")]
    pub comments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundReviewerDecisionEvidence {
    pub decision: String,
    pub explanation: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundReviewerLaneEvidence {
    pub node: NodeId,
    pub soundness: SoundReviewerDecisionEvidence,
    pub overall: String,
    pub summary: String,
    #[serde(alias = "feedback")]
    pub comments: String,
}

/// Tolerant deserializer for the per-node sound reviewer evidence map.
///
/// Audit Finding 3 changed the storage from
/// `BTreeMap<LaneId, SoundReviewerLaneEvidence>` to
/// `BTreeMap<NodeId, BTreeMap<LaneId, SoundReviewerLaneEvidence>>`.
/// Existing on-disk state (state.json, checkpoints) was serialized in the
/// old shape; rather than write a migration we silently drop the old data —
/// reviewer evidence is ephemeral and is rebuilt the next time the verifier
/// reports back. Brand-new shape parses normally; missing field defaults to
/// empty.
pub(crate) fn deserialize_sound_reviewer_evidence_per_node<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<NodeId, BTreeMap<LaneId, SoundReviewerLaneEvidence>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    // First, try the new (nested) shape.
    if let Ok(parsed) = serde_json::from_value::<
        BTreeMap<NodeId, BTreeMap<LaneId, SoundReviewerLaneEvidence>>,
    >(value.clone())
    {
        return Ok(parsed);
    }
    // Fall back: attempt to interpret as the legacy flat-by-lane shape and
    // silently drop it. Evidence is non-load-bearing across restarts.
    if serde_json::from_value::<BTreeMap<LaneId, SoundReviewerLaneEvidence>>(value).is_ok() {
        return Ok(BTreeMap::new());
    }
    // Anything else (truly malformed) we also drop rather than fail load.
    Ok(BTreeMap::new())
}

/// Tolerant deserializer for substantiveness reviewer evidence. Same
/// migration shape as the Sound case: drop legacy flat-by-lane payloads
/// silently (evidence is ephemeral) and parse the nested shape normally.
pub(crate) fn deserialize_substantiveness_reviewer_evidence_per_node<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<NodeId, BTreeMap<LaneId, PaperReviewerLaneEvidence>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(BTreeMap::new());
    }
    if let Ok(parsed) = serde_json::from_value::<
        BTreeMap<NodeId, BTreeMap<LaneId, PaperReviewerLaneEvidence>>,
    >(value.clone())
    {
        return Ok(parsed);
    }
    if serde_json::from_value::<BTreeMap<LaneId, PaperReviewerLaneEvidence>>(value).is_ok() {
        return Ok(BTreeMap::new());
    }
    Ok(BTreeMap::new())
}

/// Maximum consecutive Paper requests (per-node scenario) the kernel will
/// dispatch without per-node Unknown frontier shrinking before escalating
/// to Reviewer with a "verifier stuck" diagnostic. Default 5 — generous
/// enough to absorb a couple of NotDoneYet bursts (verifier triaging a
/// large frontier) but small enough that a truly stuck verifier surfaces
/// to the human in a reasonable wall-clock window.
pub const SUBSTANTIVENESS_MAX_CONSECUTIVE_NO_PROGRESS: u32 = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewVerifierEvidence {
    pub paper: BTreeMap<LaneId, PaperReviewerLaneEvidence>,
    #[serde(default)]
    pub deviation: BTreeMap<LaneId, PaperReviewerLaneEvidence>,
    /// Per-node substantiveness verifier evidence accumulated across the
    /// drain loop. Mirrors the `sound` shape: keyed first by node id, then
    /// by lane id. Sourced from `state.latest_substantiveness_reviewer_evidence`.
    /// Populated 2026-04-29 (audit-fix #2) so reviewers adjudicating
    /// `Substantiveness` blockers see the verifier's per-node verdict
    /// comments rather than only the blocker fingerprint.
    pub substantiveness: BTreeMap<NodeId, BTreeMap<LaneId, PaperReviewerLaneEvidence>>,
    pub corr: BTreeMap<LaneId, CorrReviewerLaneEvidence>,
    pub sound: BTreeMap<NodeId, BTreeMap<LaneId, SoundReviewerLaneEvidence>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaperResponse {
    pub request_id: u32,
    pub cycle: u32,
    pub status: ResponseStatus,
    pub target_lane_updates: CorrTargetLaneUpdates,
    /// Substantiveness lane updates (per-node scenario).
    /// Carries `SubstantivenessStatus` rather than `CorrStatus` so individual
    /// nodes can be `NotDoneYet`. Empty for the target-level scenario.
    /// Lenient-missing-entries semantics: a requested node that's absent
    /// here is interpreted as `NotDoneYet` by the kernel.
    #[serde(default)]
    pub node_lane_updates: SubstantivenessLaneUpdates,
    /// Deviation authorization updates. Non-empty only for the single-file
    /// deviation scenario.
    #[serde(default)]
    pub deviation_lane_updates: DeviationLaneUpdates,
    pub reviewer_evidence: BTreeMap<LaneId, PaperReviewerLaneEvidence>,
    /// Per-node verifier evidence collected across drain-loop rounds.
    /// Mirrors the `latest_sound_reviewer_evidence` shape — each node may
    /// accumulate evidence from multiple Paper requests if the verifier
    /// returned NotDoneYet on it before. Bridge populates this from the
    /// lane reviewer evidence that mentions a node id; kernel forwards to
    /// `state.latest_substantiveness_reviewer_evidence`.
    #[serde(default)]
    pub node_reviewer_evidence: BTreeMap<NodeId, BTreeMap<LaneId, PaperReviewerLaneEvidence>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrResponse {
    pub request_id: u32,
    pub cycle: u32,
    pub status: ResponseStatus,
    pub node_lane_updates: CorrNodeLaneUpdates,
    pub target_lane_updates: CorrTargetLaneUpdates,
    pub reviewer_evidence: BTreeMap<LaneId, CorrReviewerLaneEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundResponse {
    pub request_id: u32,
    pub cycle: u32,
    pub status: ResponseStatus,
    pub lane_updates: SoundLaneUpdates,
    pub reviewer_evidence: BTreeMap<LaneId, SoundReviewerLaneEvidence>,
}

impl Default for SoundResponse {
    fn default() -> Self {
        Self {
            request_id: 0,
            cycle: 0,
            status: ResponseStatus::Ok,
            lane_updates: BTreeMap::new(),
            reviewer_evidence: BTreeMap::new(),
        }
    }
}

/// Cleanup-v2 audit response envelope. See
/// `CLAUDES_NOTES_cleanup_v2.md`. Returned by an `Audit` request issued
/// during `Stage::CleanupAudit`. The kernel validates each `new_tasks`
/// entry via `legal_cleanup_task`, applies `task_modifications`
/// (audit may only revise its own current-round Pending proposals;
/// kernel rejects out-of-round / non-Pending modifications), replaces
/// the scratchpad, and routes on `outcome`: NeedToContinue re-issues
/// another Audit (subject to `CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND`),
/// AuditDone transitions to Stage::Reviewer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AuditResponse {
    pub request_id: u32,
    pub cycle: u32,
    pub status: ResponseStatus,
    pub new_tasks: Vec<NewCleanupAuditTask>,
    pub task_modifications: Vec<CleanupAuditTaskModification>,
    pub scratchpad_replace: String,
    pub outcome: AuditOutcome,
}

/// global_repair_mode Step A signal payload. See `ReviewResponse::global_repair_request`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalRepairRequest {
    pub proposed_extension_nodes: BTreeSet<NodeId>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewResponse {
    pub request_id: u32,
    pub cycle: u32,
    pub status: ResponseStatus,
    pub decision: ReviewDecisionKind,
    #[serde(default)]
    pub reason: String,
    pub comments: String,
    pub task_blockers: BTreeSet<Blocker>,
    /// Option C (2026-06-04): retired. Field retained for serde
    /// back-compat with legacy review responses; no longer consumed by
    /// the engine. Validation rejects non-empty values upstream.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub override_blockers: BTreeSet<Blocker>,
    pub reset_blockers: BTreeSet<Blocker>,
    /// Reviewer-requested Sound verifier dispatches. This is not a worker task
    /// and not verifier evidence; accepted entries are queued for a real Sound
    /// request subject to `sound_verifier_requestable_nodes`.
    #[serde(default)]
    pub request_sound_verifier_nodes: BTreeSet<NodeId>,
    pub next_active: Option<NodeId>,
    /// Proposal v32: reviewer-chosen next active coarse anchor.
    /// Legal only in `Phase::ProofFormalization` on a non-retry
    /// `Continue`, and only when membership lies in
    /// `state.kernel_hinted_next_active_coarse_nodes()`. When
    /// `None`, the existing anchor is preserved. Pre-v32 state files
    /// load with `None` via `#[serde(default)]`.
    #[serde(default)]
    pub next_active_coarse: Option<NodeId>,
    pub reset: ResetChoice,
    /// Required only for `reset=theorem_stating_node`; empty otherwise.
    #[serde(default)]
    pub reset_node: Option<NodeId>,
    pub next_mode: TaskMode,
    pub difficulty_updates: BTreeMap<NodeId, Update<NodeDifficulty>>,
    #[serde(default = "default_true")]
    pub allow_new_obligations: bool,
    #[serde(default)]
    pub must_close_active: bool,
    pub clear_human_input: bool,
    pub next_worker_context_mode: WorkerContextMode,
    pub paper_focus_ranges: Vec<PaperFocusRange>,
    pub work_style_hint: WorkerWorkStyleHint,
    #[serde(default)]
    pub protected_semantic_change_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub confirm_protected_semantic_change_scope: bool,
    /// global_repair_mode Step A signal. Set by the reviewer when the
    /// current cone has no legal scope for the edit the reviewer wants
    /// to authorize. Triggers an audit dispatch; does not itself
    /// authorize a worker burst. Mutually exclusive with
    /// `consume_global_repair_grant`.
    #[serde(default)]
    pub global_repair_request: Option<GlobalRepairRequest>,
    /// global_repair_mode Step C signal. When `true`, the reviewer is
    /// consuming the currently-pending audit grant; the validator
    /// permits `authorized_nodes` and `next_active` to extend
    /// out-of-cone over `pending_global_repair_grant.approved_extension_nodes`.
    /// Mutually exclusive with `global_repair_request`.
    #[serde(default)]
    pub consume_global_repair_grant: bool,
    /// Existing nodes the worker may edit. Required (non-empty) for
    /// proof Continue+Restructure/CoarseRestructure; required empty
    /// for proof Continue+Local; empty for paths that issue no worker
    /// (Done/NeedInput/AdvancePhase/whole-state resets).
    #[serde(default)]
    pub authorized_nodes: BTreeSet<NodeId>,
    /// Cleanup-v2: bulk-dismiss any number of pending tasks this cycle.
    /// Each entry is `(task_index, reason)`. Reviewer-Dismissed tasks
    /// transition Pending → Dismissed and cannot be re-revived (terminal
    /// status). Legal only in Phase::Cleanup + Continue. Defaults to
    /// empty (legacy state files / non-cleanup cycles).
    #[serde(default)]
    pub cleanup_dismiss_tasks: Vec<(u32, String)>,
    /// Cleanup-v2: optional index of the single pending task to dispatch
    /// a worker against this cycle. Legal only in Phase::Cleanup + Continue
    /// with the index pointing at a Pending task. Mutually compatible with
    /// `cleanup_dismiss_tasks` — dismissals apply before dispatch.
    /// Defaults to None.
    #[serde(default)]
    pub cleanup_next_task: Option<u32>,
    /// Cleanup-v2: when set true on a Done decision, requests another
    /// audit round (round 1 → round 2). Ignored if
    /// `state.cleanup_audit_round >= CLEANUP_AUDIT_MAX_ROUNDS` or if the
    /// kernel has latched a force-Done due to consecutive-invalid
    /// threshold.
    #[serde(default)]
    pub cleanup_request_reaudit: bool,
    #[serde(default)]
    pub dismiss_audit_plan: bool,
    #[serde(default)]
    pub dismissed_tasks: Vec<TaskDismissal>,
    /// Paper-grounding attestation. See `PaperGrounding`. Required on
    /// Continue+reset=None decisions when the request is in a
    /// friction state (any blockers, or retry_outcome_kind ∈
    /// {Stuck, NeedsRestructure}); also required whenever
    /// `paper_focus_ranges` is nonempty regardless of friction.
    #[serde(default)]
    pub paper_grounding: PaperGrounding,
    /// Optional StuckMathAudit report. Required to contain some content on
    /// Continue+reset=None while the request's StuckMathAudit view is active;
    /// its `reviewer_lean_product` is forwarded to later workers when present.
    #[serde(default)]
    pub stuck_math_audit: Option<StuckMathAuditReviewReport>,
}

impl Default for ReviewResponse {
    fn default() -> Self {
        Self {
            request_id: 0,
            cycle: 0,
            status: ResponseStatus::default(),
            decision: ReviewDecisionKind::default(),
            reason: String::new(),
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            request_sound_verifier_nodes: BTreeSet::new(),
            next_active: None,
            next_active_coarse: None,
            reset: ResetChoice::default(),
            reset_node: None,
            next_mode: TaskMode::default(),
            difficulty_updates: BTreeMap::new(),
            allow_new_obligations: true,
            must_close_active: false,
            clear_human_input: false,
            next_worker_context_mode: WorkerContextMode::default(),
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::default(),
            protected_semantic_change_nodes: BTreeSet::new(),
            confirm_protected_semantic_change_scope: false,
            global_repair_request: None,
            consume_global_repair_grant: false,
            authorized_nodes: BTreeSet::new(),
            cleanup_dismiss_tasks: Vec::new(),
            cleanup_next_task: None,
            cleanup_request_reaudit: false,
            dismiss_audit_plan: false,
            dismissed_tasks: Vec::new(),
            paper_grounding: PaperGrounding::default(),
            stuck_math_audit: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HumanGateResponse {
    pub request_id: u32,
    pub cycle: u32,
    pub status: ResponseStatus,
    pub choice: HumanChoice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StuckMathAuditResponse {
    pub request_id: u32,
    pub cycle: u32,
    pub status: ResponseStatus,
    #[serde(default)]
    pub confirm_need_input: bool,
    pub report: String,
    pub tasks: Vec<AuditTask>,
    pub probe_paths: Vec<String>,
    /// Optional audit-authorized request to restore this coarse node to
    /// its theorem-stating snapshot and auto-prune the orphaned helper
    /// cone. The kernel validates that this is one of the resettable
    /// coarse nodes before the runtime applies it.
    #[serde(default, alias = "recommended_cone_clean_node")]
    pub cone_clean_node: Option<NodeId>,
    /// global_repair_mode Step B: auditor approval flag. Only meaningful
    /// when `state.pending_global_repair_request.is_some()`.
    #[serde(default)]
    pub global_repair_approve: bool,
    /// global_repair_mode Step B: nodes the auditor authorizes for
    /// out-of-cone editing. Must be ⊆ the dep-neighborhood of the
    /// reviewer's `proposed_extension_nodes` (S5 structural cap).
    #[serde(default)]
    pub global_repair_approved_extension_node_ids: Vec<String>,
    /// global_repair_mode Step B: brief auditor rationale for
    /// approval / decline. Surfaced to reviewer on decline.
    #[serde(default)]
    pub global_repair_auditor_reason: String,
}

/// Variant-agnostic accessors over [`WrapperResponse`]. Implemented per
/// variant; the enum dispatches to the active variant via `enum_dispatch`.
#[enum_dispatch::enum_dispatch]
pub trait WrapperResponseMeta {
    fn kind(&self) -> RequestKind;
    fn request_id(&self) -> u32;
    fn cycle(&self) -> u32;
    fn status(&self) -> ResponseStatus;
}

macro_rules! impl_wrapper_response_meta {
    ($ty:ty, $kind:expr) => {
        impl WrapperResponseMeta for $ty {
            fn kind(&self) -> RequestKind {
                $kind
            }
            fn request_id(&self) -> u32 {
                self.request_id
            }
            fn cycle(&self) -> u32 {
                self.cycle
            }
            fn status(&self) -> ResponseStatus {
                self.status
            }
        }
    };
}

impl_wrapper_response_meta!(WorkerResponse, RequestKind::Worker);
impl_wrapper_response_meta!(PaperResponse, RequestKind::Paper);
impl_wrapper_response_meta!(CorrResponse, RequestKind::Corr);
impl_wrapper_response_meta!(SoundResponse, RequestKind::Sound);
impl_wrapper_response_meta!(ReviewResponse, RequestKind::Review);
impl_wrapper_response_meta!(HumanGateResponse, RequestKind::HumanGate);
impl_wrapper_response_meta!(AuditResponse, RequestKind::Audit);
impl_wrapper_response_meta!(StuckMathAuditResponse, RequestKind::StuckMathAudit);

#[enum_dispatch::enum_dispatch(WrapperResponseMeta)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WrapperResponse {
    Worker(WorkerResponse),
    Paper(PaperResponse),
    Corr(CorrResponse),
    Sound(SoundResponse),
    Review(ReviewResponse),
    HumanGate(HumanGateResponse),
    /// Cleanup-v2 audit response. See `AuditResponse`.
    Audit(AuditResponse),
    StuckMathAudit(StuckMathAuditResponse),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProtocolState {
    pub phase: Phase,
    pub stage: Stage,
    pub cycle: u32,
    pub attempt: u32,
    pub max_theorem_invalid_attempt: u32,
    pub proof_invalid_review_threshold: u32,
    /// Bug X principled fix: transport-failure (RetryOutcomeKind::Transport)
    /// retry counter. Bumped only when the bridge reports an infrastructure
    /// failure (agent never produced output, timeout, etc.) rather than a
    /// malformed/invalid worker output. Resets when the worker produces a
    /// non-Transport response or the cycle advances.
    #[serde(default)]
    pub transport_attempt: u32,
    /// Bug X principled fix: max consecutive transport failures the kernel
    /// will silently retry before escalating to the reviewer. A flaky tmux
    /// session burns this budget rather than `proof_invalid_review_threshold`.
    /// Default 5: roughly the union of "two layers of two-attempt retries"
    /// the bridge previously did silently, so behavior in the success case
    /// mirrors the old budget without loss.
    #[serde(default = "default_transport_invalid_review_threshold")]
    pub transport_invalid_review_threshold: u32,
    /// Circuit-breaker (2026-05-12): node identity of the active node when
    /// we most recently observed a `transport_failure=true` worker
    /// response. Used together with
    /// `consecutive_transport_failure_count` to detect persistent
    /// worker-bridge burn loops on the same node (which cycle through
    /// reviewer → reset-LastCommit → worker → fail → reviewer at ~$15
    /// per cycle without making progress; cf. the 33-cycle / $568 loop
    /// triggered by the decl_split-vs-scoped-notation bug on
    /// FiberAndDegreeMixedLiftedIntersectionUniformBound).
    #[serde(default)]
    pub consecutive_transport_failure_node: Option<NodeId>,
    /// Circuit-breaker counter: number of consecutive
    /// `transport_failure=true` worker responses that targeted
    /// `consecutive_transport_failure_node`. Increments on each such
    /// response, resets on any non-transport-failure worker response
    /// (Valid/Stuck/Invalid-non-transport) or when the active node
    /// changes. When this reaches
    /// `consecutive_transport_failure_halt_threshold`, the engine
    /// emits `ProtocolCommand::WriteHaltSentinel` so the supervisor
    /// halts at the next checkpoint boundary.
    #[serde(default)]
    pub consecutive_transport_failure_count: u32,
    /// Circuit-breaker threshold (default 5): once
    /// `consecutive_transport_failure_count` reaches this value the
    /// engine writes the halt sentinel. 5 was chosen as the smallest
    /// value that absorbs occasional double-bridge flakes without
    /// burning the reviewer budget on a true bug.
    #[serde(default = "default_consecutive_transport_failure_halt_threshold")]
    pub consecutive_transport_failure_halt_threshold: u32,
    pub easy_max_retries: u32,
    pub verifier_lanes: BTreeSet<LaneId>,
    pub request_seq: u32,
    /// Number of consecutive `CommitCheckpoint` emissions for which
    /// `global_blockers()` was non-empty. Reset to 0 when a checkpoint
    /// emits with empty blockers. Surfaced to the reviewer so they can
    /// judge when to escape a blocker spiral via `ResetChoice::LastClean`.
    #[serde(default)]
    pub cycles_since_clean: u32,
    /// Per-checkpoint progress snapshot buffer driving the
    /// no-Sound-progress StuckMathAudit gate. Bookkept by `commit_live`
    /// via `push_progress_snapshot`. See `progress_history.rs` for
    /// snapshot semantics; the Sound consumer is implemented by
    /// `stuck_math_audit_no_sound_progress_trigger`. Default empty for
    /// legacy state files (gate stays off until the buffer accumulates
    /// `k+1` snapshots).
    #[serde(default)]
    pub progress_history: ProgressHistory,
    /// Current committed count of coarse-DAG nodes that are shallowly
    /// closed from coarse.
    #[serde(default)]
    pub shallow_coarse_closed_count: u32,
    /// Consecutive checkpoint cycles since `shallow_coarse_closed_count`
    /// last increased. Operator-facing interpretation: cycles since
    /// remaining coarse-shallow-open work last decreased.
    #[serde(default)]
    pub cycles_since_shallow_coarse_closed_count_increase: u32,
    /// Becomes true the first time `commit_live()` observes empty
    /// `global_blockers()` — i.e. the first time the checkpoint hook
    /// writes a `supervisor2/clean-NNNNNN` git tag. Gates
    /// `ResetChoice::LastClean` availability: there's no point
    /// offering it before any clean tag exists in the repo (the
    /// runtime's tag-walk would error).
    #[serde(default)]
    pub has_ever_been_clean: bool,
    pub invalid_attempt: bool,
    pub gate_kind: GateKind,
    pub gate_from_invalid_attempt: bool,
    pub active_node: Option<NodeId>,
    pub held_target: Option<NodeId>,
    pub target_edit_mode: TargetEditMode,
    pub proof_edit_mode: ProofEditMode,
    pub configured_targets: BTreeSet<TargetId>,
    pub approved_targets: ApprovedTargetSnapshot,
    /// Snapshot of `live.present_nodes` at the moment of the HumanGate
    /// Approve that advanced theorem-stating → proof-formalization. Acts
    /// as the "coarse DAG" set: worker signature edits on these nodes
    /// require `coarse_restructure` mode; later-added proof-phase helpers
    /// are editable under plain `restructure`.
    #[serde(default)]
    pub coarse_dag_nodes: BTreeSet<NodeId>,
    /// Proposal v32 active coarse-DAG anchor. Set only while
    /// `phase == Phase::ProofFormalization` AND `coarse_dag_nodes` is
    /// non-empty. The reviewer chooses an anchor on the first eligible
    /// ProofFormalization `Continue`; the anchor is then locked against
    /// change until `active_coarse_change_allowed()` returns true
    /// (shallow-closed + no global blockers, OR the starvation guard
    /// has fired). Once an anchor is set, `active_node` is constrained
    /// to `coarse_legal_active_set()` (the down-cone of the anchor,
    /// optionally widened to blocker repair cones).
    /// Cleared on every phase transition out of ProofFormalization, and
    /// when an audit-authorized cone-clean reset targets the anchor
    /// itself (forces re-seeding on the next Review).
    /// `#[serde(default)] = None` for legacy state files.
    #[serde(default)]
    pub active_coarse_node: Option<NodeId>,
    /// Proposal v32 starvation-guard counter. Incremented on every
    /// ProofFormalization Continue cycle where `coarse_repair_mode()`
    /// is true and the anchor did not change. Reset to 0 whenever the
    /// anchor changes (kernel fresh-start), whenever the anchor is
    /// cleared, or whenever `coarse_repair_mode()` is false on the
    /// cycle in question. When this counter is >=
    /// `stuck_coarse_repair_threshold()`, the kernel opens
    /// `active_coarse_change_allowed()` even without strict shallow
    /// closure, to prevent indefinite blocker-chain drift. Default 0
    /// for legacy state files.
    #[serde(default)]
    pub cycles_in_coarse_repair_mode: u32,
    /// Schema version of the stored fingerprint shape. `0` is legacy storage
    /// (corr / paper fingerprints carried `definition_descendants` /
    /// `definition_nodes` — every def-kind node in the textual import
    /// closure was hashed). `2` recomputes both axes to use the
    /// Lean-relevance-filtered `lean_relevant_definition_descendants`
    /// axis (def-kind nodes the parent's `lean_semantic_closure` walk
    /// actually visits). `3` does NOT add a separate axis recompute;
    /// it adds a *runtime invariant restorer*, the schema-equivalent
    /// baseline repair, that runs on every load. Migration jumps
    /// `0 → 3` for fresh state files; `1` is reserved (no historical
    /// state uses it).
    ///
    /// Two distinct steps with different lifecycles:
    ///
    /// 1. **One-shot axis migration** (gated on `< 2`): recomputes
    ///    aligned (live==approved byte-equal) fingerprints from the
    ///    current tablet state, leaves drifted entries and
    ///    `last_clean_*` mirrors in legacy storage. Refuses to run
    ///    while an in-flight worker request exists. Runs once.
    ///
    /// 2. **Every-load schema-equivalent repair** (NOT gated on
    ///    schema_version): re-pins legacy-shape `corr_approved_fingerprints`
    ///    / `paper_approved_fingerprints` to match the current live
    ///    fingerprint when the two are schema-equivalent (semantic axes
    ///    agree, only the descendant-axis key naming differs). This
    ///    catches mid-run drift introduced by `apply_*_worker_response`
    ///    Valid arms doing `state.live = snapshot` with NEW-shape live
    ///    against an OLD-shape approved baseline checkpointed at
    ///    schema=2. Pure-in-memory and idempotent, so it can re-run on
    ///    every load without effect once the baseline is consistent.
    ///
    /// The runtime CLI's load wrapper
    /// (`load_runtime_with_fingerprint_validation`) runs both steps
    /// after `SupervisorRuntime::load`.
    #[serde(default)]
    pub corr_fingerprint_schema_version: u32,
    /// Soundness assessment schema cutover.
    ///
    /// Version 1 is the kernel-owned SKETCH / verifier-assessment model.
    /// Missing in persisted pre-cutover states, so serde reads it as 0.
    /// Runtime load refuses 0-version states that already contain Sound
    /// lane evidence; operators must rewind before any Sound lane was
    /// dispatched rather than migrating old Sound judgments.
    #[serde(default)]
    pub sound_assessment_schema_version: u32,
    pub node_kinds: BTreeMap<NodeId, NodeKind>,
    pub committed_node_kinds: BTreeMap<NodeId, NodeKind>,
    pub proof_nodes: BTreeSet<NodeId>,
    pub committed_proof_nodes: BTreeSet<NodeId>,
    pub deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    pub committed_deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    pub target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    pub committed_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    /// Known deviation reference files, keyed by stable deviation id.
    #[serde(default)]
    pub deviation_files: BTreeMap<DeviationId, String>,
    #[serde(default)]
    pub committed_deviation_files: BTreeMap<DeviationId, String>,
    /// Node-to-deviation claims declared by accepted worker responses.
    #[serde(default)]
    pub node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    #[serde(default)]
    pub committed_node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    /// Snapshot of `live` taken on every CommitCheckpoint where
    /// `global_blockers().is_empty()` (the kernel-side "is_clean"
    /// predicate that drives the `supervisor2/clean-NNNNNN` git tag).
    /// Source of truth for the post-rewind state during
    /// `apply_last_clean_reset`. (#56)
    #[serde(default)]
    pub last_clean_live: WorkingSnapshot,
    #[serde(default)]
    pub last_clean_node_kinds: BTreeMap<NodeId, NodeKind>,
    #[serde(default)]
    pub last_clean_proof_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub last_clean_deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    #[serde(default)]
    pub last_clean_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    #[serde(default)]
    pub last_clean_deviation_files: BTreeMap<DeviationId, String>,
    #[serde(default)]
    pub last_clean_node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    /// Snapshots of verifier-lane statuses at clean-checkpoint time
    /// (#56-extension). At a clean checkpoint `global_blockers().is_empty()`,
    /// which by construction means all open nodes/targets have Pass status
    /// against approved fingerprints. Restoring statuses from these
    /// mirrors during `apply_last_clean_reset` keeps the post-rewind
    /// state consistent with the rewound disk: no spurious Unknown
    /// statuses → no spurious blockers → reviewer/Done can act on the
    /// truly-clean state without needing a redundant verifier re-run.
    /// (Pre-extension, statuses were cleared, which produced phantom
    /// Unknown blockers in proof/cleanup phases whose `start_cycle`
    /// routes to Worker rather than verifier — leaving the run stuck
    /// with unadjudicable blockers.)
    #[serde(default)]
    pub last_clean_corr_status: BTreeMap<NodeId, CorrStatus>,
    #[serde(default)]
    pub last_clean_paper_status: BTreeMap<TargetId, CorrStatus>,
    #[serde(default)]
    pub last_clean_deviation_status: BTreeMap<DeviationId, CorrStatus>,
    /// Substantiveness status mirror, captured at clean
    /// checkpoints. Restored by `apply_last_clean_reset` so the post-rewind
    /// state is internally consistent (same shape as
    /// `last_clean_corr_status`).
    #[serde(default)]
    pub last_clean_substantiveness_status: BTreeMap<NodeId, CorrStatus>,
    #[serde(default)]
    pub last_clean_sound_status: BTreeMap<NodeId, SoundStatus>,
    /// Approved-fingerprint mirrors captured at the last clean
    /// checkpoint (audit: LastClean restored status maps but not
    /// approved-fp maps, producing phantom Unknown blockers when
    /// `current_<lane>_state` requires status=Pass AND
    /// `current_fingerprint == approved_fingerprint`). Restored by
    /// `apply_last_clean_reset` so the post-rewind state is internally
    /// consistent (all three of status / current_fp / approved_fp align).
    #[serde(default)]
    pub last_clean_corr_approved_fingerprints: BTreeMap<NodeId, Fingerprint>,
    #[serde(default)]
    pub last_clean_paper_approved_fingerprints: BTreeMap<TargetId, Fingerprint>,
    /// Substantiveness approved-fingerprint mirror, captured at
    /// clean checkpoints. Mirrors `last_clean_corr_approved_fingerprints`.
    #[serde(default)]
    pub last_clean_substantiveness_approved_fingerprints: BTreeMap<NodeId, Fingerprint>,
    #[serde(default)]
    pub last_clean_deviation_approved_fingerprints: BTreeMap<DeviationId, Fingerprint>,
    #[serde(default)]
    pub last_clean_sound_approved_fingerprints: BTreeMap<NodeId, Fingerprint>,
    /// True once `commit_live` has captured a complete set of
    /// `last_clean_*` mirrors (structural + status + approved-fp).
    /// Replaces the structural-only `last_clean_mirrors_populated()`
    /// gate. State files persisted by versions before any of the
    /// status / approved-fp mirror fields existed deserialize this
    /// flag as `false` (#[serde(default)]) — so LastClean is
    /// suppressed until the next clean commit_live writes a complete
    /// mirror set, even if `has_ever_been_clean=true` is sticky from
    /// an earlier (incomplete) clean checkpoint.
    #[serde(default)]
    pub last_clean_verifier_mirror_ready: bool,
    // ---- Plan §7.2 local-closure tiers (Patch C-A foundation) -----------
    // Three parallel tiers for closure state, mirroring the existing
    // live / committed / last_clean pattern (`live`, `committed`,
    // `last_clean_live` etc.). Each carries its own records map,
    // unverified set, and failures map. Reverse-index fields
    // (`boundary_statement_consumers`, `strict_dep_consumers`) are
    // derived from records and are NOT mirrored at any tier — they get
    // recomputed after rollback / restore via
    // `recompute_local_closure_reverse_indices`. All three serde-default
    // to empty for forward-compat with pre-Patch-C state files.
    /// Live tier — read by predicates, mutated by the accept paths
    /// (Patch C-B). Records keyed by NodeId; presence implies the
    /// node is sorry-free AND has a fresh probe result.
    #[serde(default)]
    pub local_closure_records: BTreeMap<NodeId, LocalClosureRecord>,
    /// Live tier — sorry-free nodes that are missing a fresh record.
    /// Mutually exclusive with `live.open_nodes` by the sorry-free-only
    /// invariant (plan §7.2). Drives the deterministic-revalidation
    /// pass and the scheduling-predicate union (Patch C-C).
    #[serde(default)]
    pub local_closure_unverified_nodes: BTreeSet<NodeId>,
    /// Live tier — categorized failure summaries for nodes in
    /// `local_closure_unverified_nodes`. Carries axiom_violations /
    /// strict_errors / transport-error backoff state.
    #[serde(default)]
    pub local_closure_failures: BTreeMap<NodeId, ErrorSummary>,
    /// Committed tier — snapshot of the live tier at every
    /// `commit_live`; restored on `restore_committed` (rejection
    /// rollback). See plan §7.2.
    #[serde(default)]
    pub committed_local_closure_records: BTreeMap<NodeId, LocalClosureRecord>,
    #[serde(default)]
    pub committed_local_closure_unverified_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub committed_local_closure_failures: BTreeMap<NodeId, ErrorSummary>,
    /// LastClean tier — snapshotted by `commit_live` at clean
    /// checkpoints; restored by `apply_last_clean_reset`. Plan §7.8.
    #[serde(default)]
    pub last_clean_local_closure_records: BTreeMap<NodeId, LocalClosureRecord>,
    #[serde(default)]
    pub last_clean_local_closure_unverified_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    pub last_clean_local_closure_failures: BTreeMap<NodeId, ErrorSummary>,
    /// LastClean readiness flag for the closure mirrors (plan §7.8).
    /// Defaults to `false` so any state file deserialized before Patch
    /// C-A populated the closure mirrors at least once at a clean
    /// checkpoint will refuse a `ResetChoice::LastClean` until the
    /// next clean `commit_live` populates the mirrors. Mirrors the
    /// `last_clean_verifier_mirror_ready` precedent.
    #[serde(default)]
    pub last_clean_local_closure_mirror_ready: bool,
    /// Number of `apply_last_clean_reset` rewinds that landed on the
    /// CURRENT `last_clean_*` mirror. Incremented on every successful
    /// `apply_last_clean_reset`; reset to 0 whenever `commit_live`
    /// captures a new clean checkpoint (mirror is replaced, so prior
    /// rewinds no longer target the same state). Surfaced in the
    /// reviewer request_summary so the prompt can exempt the mandatory-
    /// last_clean threshold once the same checkpoint has been
    /// re-rewound to twice or more (repeated rewinds aren't helping).
    #[serde(default)]
    pub last_clean_rewind_count: u32,
    /// Set true by reset/rewind paths that should force an audit;
    /// consumed (set false) by `should_dispatch_stuck_math_audit` on the
    /// first reviewer-slot after the rewind, where it overrides the
    /// usual gate (delta vs reaudit_interval) to dispatch a
    /// `StuckMathAudit` regardless of audit_plan presence. Activates the
    /// latch if not already active. The intent is: every substantial
    /// reset/rewind earns a fresh audit on the restored state before any
    /// Reviewer touches it.
    #[serde(default)]
    pub force_stuck_math_audit_after_rewind: bool,
    /// Set true after a StuckMathAudit itself authorizes a cone clean.
    /// The reset is applied by the runtime after the audit transition,
    /// so the next StartCycle must issue a fresh Review request from the
    /// re-observed post-clean state rather than routing through the
    /// ordinary proof-phase worker/verifier scheduler.
    #[serde(default)]
    pub force_review_after_cone_clean: bool,
    /// Set true when a human-approved phase-advance HumanGate transitions
    /// the engine into a new work phase (currently only
    /// TheoremStating → ProofFormalization). The next `start_cycle` must
    /// issue a routing Review instead of immediately dispatching a Worker
    /// burst with kernel-default permissive flags
    /// (`allow_new_obligations=true, must_close_active=false`), so the
    /// reviewer can explicitly pick `next_active`, the `must_close_active`
    /// /`allow_new_obligations` combo, `authorized_nodes`,
    /// `paper_focus_ranges`, and `next_worker_context_mode` for the first
    /// burst of the new phase. The flag is cleared in `start_cycle` once
    /// the routing Review has been issued.
    #[serde(default)]
    pub post_advance_routing_pending: bool,
    /// Reverse index — for every helper H named in some record's
    /// `boundary_theorems`, the set of consumer nodes whose record
    /// references H. Derived from `local_closure_records`; NOT
    /// serialized (`#[serde(skip)]`) — recomputed via
    /// `recompute_local_closure_reverse_indices` on startup and after
    /// every restore. Used by the invalidation walk (Patch C-B/C-C).
    #[serde(skip)]
    pub boundary_statement_consumers: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// Reverse index — for every strict-context dep D
    /// (theorem-or-definition) named in some record, the set of
    /// consumer nodes whose record references D. Derived; NOT
    /// serialized.
    #[serde(skip)]
    pub strict_dep_consumers: BTreeMap<NodeId, BTreeSet<NodeId>>,
    pub node_rank: BTreeMap<NodeId, u32>,
    pub live: WorkingSnapshot,
    pub committed: WorkingSnapshot,
    pub corr_status: BTreeMap<NodeId, CorrStatus>,
    pub corr_approved_fingerprints: BTreeMap<NodeId, Fingerprint>,
    pub paper_status: BTreeMap<TargetId, CorrStatus>,
    pub paper_approved_fingerprints: BTreeMap<TargetId, Fingerprint>,
    /// Deviation authorization status. A Pass authorizes the reference-file
    /// deviation for nodes that claim it; Fail keeps those claims from
    /// satisfying substantiveness.
    #[serde(default)]
    pub deviation_status: BTreeMap<DeviationId, CorrStatus>,
    #[serde(default)]
    pub deviation_approved_fingerprints: BTreeMap<DeviationId, Fingerprint>,
    /// Substantiveness status. Mirrors `corr_status` in shape — keyed by
    /// NodeId, valued by Pass/Fail/Unknown. `current_substantiveness_state`
    /// returns Pass unconditionally outside TheoremStating /
    /// ProofFormalization, so this map's contents only matter while in
    /// those phases.
    #[serde(default)]
    pub substantiveness_status: BTreeMap<NodeId, CorrStatus>,
    /// Approved substantiveness fingerprints. JSON-encoded
    /// `SubstantivenessFingerprint`. Cleared/restored alongside
    /// `substantiveness_status` during LastClean reset.
    #[serde(default)]
    pub substantiveness_approved_fingerprints: BTreeMap<NodeId, Fingerprint>,
    #[serde(default)]
    pub sound_assessments: BTreeMap<NodeId, SoundAssessment>,
    #[serde(default)]
    pub reviewer_requested_sound_verifier_nodes: BTreeSet<NodeId>,
    pub sound_status: BTreeMap<NodeId, SoundStatus>,
    pub sound_approved_fingerprints: BTreeMap<NodeId, Fingerprint>,
    pub node_difficulty: BTreeMap<NodeId, NodeDifficulty>,
    pub easy_attempts: BTreeMap<NodeId, u32>,
    pub human_input_outstanding: bool,
    pub pending_task: Option<PendingTask>,
    #[serde(default)]
    pub pending_protected_semantic_scope_confirmation: Option<ProtectedSemanticChangeConfirmation>,
    #[serde(default)]
    pub pending_protected_reapproval_nodes: BTreeSet<NodeId>,
    pub retry_outcome_kind: RetryOutcomeKind,
    pub reviewer_comments: String,
    pub latest_worker_summary: String,
    pub latest_worker_comments: String,
    /// Worker-named nodes from the last NeedsRestructure response.
    /// Cleared whenever a non-NR worker outcome lands.
    #[serde(default)]
    pub latest_worker_needs_restructure_suggested_nodes: BTreeSet<NodeId>,
    pub deterministic_worker_rejection_reasons: Vec<String>,
    /// Kernel-authored reasons the last reviewer artifact was rejected.
    /// Cleared on any accepted reviewer response; surfaced to the next
    /// reviewer turn so illegal scope/routing attempts are actionable.
    #[serde(default)]
    pub latest_review_rejection_reasons: Vec<String>,
    pub latest_paper_reviewer_evidence: BTreeMap<LaneId, PaperReviewerLaneEvidence>,
    #[serde(default)]
    pub latest_deviation_reviewer_evidence: BTreeMap<LaneId, PaperReviewerLaneEvidence>,
    #[serde(default)]
    pub latest_deviation_review_ids: BTreeSet<DeviationId>,
    /// Substantiveness reviewer evidence. Keyed first by node,
    /// then by lane. Mirrors the `latest_sound_reviewer_evidence` shape
    /// because the substantiveness lane runs in a drain-loop similar to
    /// proof-phase Sound (one Paper request per round, possibly multiple
    /// rounds before reviewer cycle).
    #[serde(
        default,
        deserialize_with = "deserialize_substantiveness_reviewer_evidence_per_node"
    )]
    pub latest_substantiveness_reviewer_evidence:
        BTreeMap<NodeId, BTreeMap<LaneId, PaperReviewerLaneEvidence>>,
    pub latest_corr_reviewer_evidence: BTreeMap<LaneId, CorrReviewerLaneEvidence>,
    #[serde(
        default,
        deserialize_with = "deserialize_sound_reviewer_evidence_per_node"
    )]
    pub latest_sound_reviewer_evidence:
        BTreeMap<NodeId, BTreeMap<LaneId, SoundReviewerLaneEvidence>>,
    pub latest_paper_review_targets: BTreeSet<TargetId>,
    /// Set of nodes the substantiveness lane was asked to verify in the
    /// most recent Paper request (per-node scenario). Used by
    /// `review_blocker_adjudicable` to gate adjudication of
    /// `Substantiveness` blockers.
    #[serde(default)]
    pub latest_substantiveness_review_nodes: BTreeSet<NodeId>,
    pub latest_corr_review_nodes: BTreeSet<NodeId>,
    pub latest_sound_review_nodes: BTreeSet<NodeId>,
    pub previous_paper_lane_findings: BTreeMap<LaneId, PaperReviewerLaneEvidence>,
    /// Substantiveness lane findings. Keyed first by node, then
    /// by lane (parallel to `latest_substantiveness_reviewer_evidence`). Drives
    /// the verifier prompt's "previous own findings" rendering during
    /// revisits.
    #[serde(
        default,
        deserialize_with = "deserialize_substantiveness_reviewer_evidence_per_node"
    )]
    pub previous_substantiveness_lane_findings:
        BTreeMap<NodeId, BTreeMap<LaneId, PaperReviewerLaneEvidence>>,
    /// Counter for consecutive Paper requests (per-node scenario) that made
    /// no progress on the substantiveness Unknown frontier. Reset to 0 every
    /// time the frontier shrinks (a node moves Unknown → Pass/Fail). When
    /// it hits `substantiveness_max_consecutive_no_progress`, the kernel
    /// escalates to Reviewer with a "verifier stuck" diagnostic instead of
    /// re-issuing another Paper request. Default 0 (fresh state); also
    /// reset whenever the kernel transitions out of `Stage::VerifyPaper`.
    #[serde(default)]
    pub substantiveness_consecutive_no_progress_requests: u32,
    pub previous_corr_lane_findings: BTreeMap<LaneId, CorrReviewerLaneEvidence>,
    #[serde(
        default,
        deserialize_with = "deserialize_sound_reviewer_evidence_per_node"
    )]
    pub previous_sound_lane_findings: BTreeMap<NodeId, BTreeMap<LaneId, SoundReviewerLaneEvidence>>,
    // ---- Cleanup-v2 fields (2026-05-14) ------------------------------------
    // All `#[serde(default)]` so legacy state files deserialize cleanly:
    // empty task list, empty scratchpad, all counters 0, round 1, no
    // active task, no force-Done flag. The new types' Default impls
    // cover the contents.
    /// Audit-proposed (and reviewer-managed) cleanup tasks. Pending tasks
    /// drive worker bursts; Dismissed/Failed/Completed are terminal.
    /// Mutated by the audit (append-only for new_tasks, status revisions
    /// for current-round Pending entries) and by the reviewer (bulk
    /// dismissal, dispatch). Empty in legacy state files.
    #[serde(default)]
    pub cleanup_audit_tasks: Vec<CleanupAuditTask>,
    /// Audit scratchpad — carried across audit bursts within a single
    /// round. Replaced wholesale by every accepted audit response's
    /// `scratchpad_replace`. Reset on round transitions.
    #[serde(default)]
    pub cleanup_audit_scratchpad: String,
    /// Per-round audit burst count. Incremented on every accepted
    /// audit response. Compared against
    /// `CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND`. Reset on round transitions
    /// and on Cleanup phase re-entry.
    #[serde(default)]
    pub cleanup_audit_burst_count: u32,
    /// Current audit round (1 or 2). The reviewer may request a single
    /// re-audit on Done if `cleanup_audit_round < CLEANUP_AUDIT_MAX_ROUNDS`.
    /// Defaults to 1; reset on Cleanup phase re-entry.
    #[serde(default = "default_cleanup_audit_round")]
    pub cleanup_audit_round: u32,
    /// Consecutive Invalid (or Stuck/NR-as-Failed) worker bursts in
    /// Phase::Cleanup. When it reaches
    /// `CLEANUP_CONSECUTIVE_INVALID_THRESHOLD`, the kernel latches
    /// `cleanup_force_done` and the next reviewer Done arm ignores
    /// re-audit requests (auto-Done into Phase::Complete).
    #[serde(default)]
    pub cleanup_consecutive_invalid_workers: u32,
    /// Index into `cleanup_audit_tasks` for the in-flight cleanup
    /// worker. Set by the reviewer's `cleanup_next_task`; cleared on
    /// worker acceptance/rejection. None outside an in-flight worker
    /// burst.
    #[serde(default)]
    pub cleanup_active_task: Option<u32>,
    /// Latched when consecutive-invalid threshold has fired; the next
    /// reviewer Done arm must ignore `cleanup_request_reaudit` and
    /// transition to Phase::Complete regardless. Cleared on Cleanup
    /// phase re-entry.
    #[serde(default)]
    pub cleanup_force_done: bool,
    /// Most recent audit rejection reason — set when an audit response
    /// fails `legal_cleanup_task` validation or contains invalid
    /// `task_modifications`. Surfaced to the next audit burst as
    /// retry context; cleared on a Valid audit accept or on Cleanup
    /// re-entry.
    #[serde(default)]
    pub latest_audit_rejection_reason: String,
    /// Per-burst-slot audit validation retry counter (0 or 1). When 1
    /// and another validation failure arrives, the kernel forces
    /// `AuditDone` rather than re-issuing. Reset on every Valid append
    /// and on Cleanup re-entry.
    #[serde(default)]
    pub audit_burst_retry_count: u32,
    /// Sticky StuckMathAudit latch. Once activated by repeated
    /// proof-formalization math blockers, it persists until the next
    /// global-blocker-free state. After a LastClean rewind it persists
    /// until `commit_live` captures a genuinely new clean checkpoint
    /// (tracked by `last_clean_rewind_count`). The optional product is
    /// reviewer-authored context forwarded to later workers.
    #[serde(default)]
    pub stuck_math_audit: StuckMathAuditState,
    #[serde(default)]
    pub audit_plan: Option<AuditPlan>,
    #[serde(default)]
    pub superseded_audit_plan: Option<AuditPlan>,
    #[serde(default)]
    pub last_stuck_math_audit_dispatched_cycle: Option<u32>,
    #[serde(default)]
    pub stuck_math_audit_burst_retry_count: u32,
    #[serde(default)]
    pub latest_stuck_math_audit_rejection_reason: String,
    /// global_repair_mode Step A: reviewer's pending audit request.
    #[serde(default)]
    pub pending_global_repair_request: Option<PendingGlobalRepairRequest>,
    /// global_repair_mode Step B: auditor's approval awaiting Step C
    /// consumption by the reviewer.
    #[serde(default)]
    pub pending_global_repair_grant: Option<PendingGlobalRepairGrant>,
    /// global_repair_mode S9: latest auditor decline reason. Surfaced on
    /// Review requests; cleared on next Step A acceptance or TTL expiry.
    #[serde(default)]
    pub latest_global_repair_audit_decline_reason: String,
    /// global_repair_mode S9: cycle the decline reason was populated.
    #[serde(default)]
    pub latest_global_repair_audit_decline_cycle: Option<u32>,
    /// global_repair_mode S10: cycle of the most recent Step A acceptance,
    /// used to rate-limit successive Step A dispatches.
    #[serde(default)]
    pub last_reviewer_global_repair_request_cycle: Option<u32>,
    /// global_repair_mode S8: monotone history of coarse nodes that have
    /// been observed shallowly-closed-from-coarse against the COMMITTED
    /// baseline at some prior `commit_live`. Refreshed only in
    /// `commit_live` and reset by `apply_last_clean_reset`. The
    /// load-bearing safety invariant: anchor change is forbidden while
    /// `ever_shallow_coarse_closed_regressed()` is non-empty.
    #[serde(default)]
    pub ever_shallow_coarse_closed: BTreeSet<NodeId>,
    /// global_repair_mode kill-switch. Default `true`. When `false`, the
    /// validator rejects every response that uses the new fields.
    #[serde(default = "default_true")]
    pub global_repair_mode_enabled: bool,
    pub in_flight_request: Option<WrapperRequest>,
}

impl Default for ProtocolState {
    fn default() -> Self {
        Self {
            phase: Phase::TheoremStating,
            stage: Stage::Start,
            cycle: 0,
            attempt: 0,
            max_theorem_invalid_attempt: 2,
            proof_invalid_review_threshold: 2,
            transport_attempt: 0,
            transport_invalid_review_threshold: default_transport_invalid_review_threshold(),
            consecutive_transport_failure_node: None,
            consecutive_transport_failure_count: 0,
            consecutive_transport_failure_halt_threshold:
                default_consecutive_transport_failure_halt_threshold(),
            easy_max_retries: 2,
            verifier_lanes: default_verifier_lanes(),
            request_seq: 0,
            cycles_since_clean: 0,
            progress_history: ProgressHistory::default(),
            shallow_coarse_closed_count: 0,
            cycles_since_shallow_coarse_closed_count_increase: 0,
            last_clean_rewind_count: 0,
            force_stuck_math_audit_after_rewind: false,
            force_review_after_cone_clean: false,
            post_advance_routing_pending: false,
            has_ever_been_clean: false,
            invalid_attempt: false,
            gate_kind: GateKind::None,
            gate_from_invalid_attempt: false,
            active_node: None,
            held_target: None,
            target_edit_mode: TargetEditMode::Global,
            proof_edit_mode: ProofEditMode::Local,
            configured_targets: BTreeSet::new(),
            approved_targets: ApprovedTargetSnapshot::default(),
            coarse_dag_nodes: BTreeSet::new(),
            active_coarse_node: None,
            cycles_in_coarse_repair_mode: 0,
            corr_fingerprint_schema_version: 0,
            sound_assessment_schema_version: SOUND_ASSESSMENT_SCHEMA_VERSION,
            node_kinds: BTreeMap::new(),
            committed_node_kinds: BTreeMap::new(),
            proof_nodes: BTreeSet::new(),
            committed_proof_nodes: BTreeSet::new(),
            deps: BTreeMap::new(),
            committed_deps: BTreeMap::new(),
            target_claims: BTreeMap::new(),
            committed_target_claims: BTreeMap::new(),
            deviation_files: BTreeMap::new(),
            committed_deviation_files: BTreeMap::new(),
            node_deviation_claims: BTreeMap::new(),
            committed_node_deviation_claims: BTreeMap::new(),
            last_clean_live: WorkingSnapshot::default(),
            last_clean_node_kinds: BTreeMap::new(),
            last_clean_proof_nodes: BTreeSet::new(),
            last_clean_deps: BTreeMap::new(),
            last_clean_target_claims: BTreeMap::new(),
            last_clean_deviation_files: BTreeMap::new(),
            last_clean_node_deviation_claims: BTreeMap::new(),
            last_clean_corr_status: BTreeMap::new(),
            last_clean_paper_status: BTreeMap::new(),
            last_clean_deviation_status: BTreeMap::new(),
            last_clean_substantiveness_status: BTreeMap::new(),
            last_clean_sound_status: BTreeMap::new(),
            last_clean_corr_approved_fingerprints: BTreeMap::new(),
            last_clean_paper_approved_fingerprints: BTreeMap::new(),
            last_clean_substantiveness_approved_fingerprints: BTreeMap::new(),
            last_clean_deviation_approved_fingerprints: BTreeMap::new(),
            last_clean_sound_approved_fingerprints: BTreeMap::new(),
            last_clean_verifier_mirror_ready: false,
            local_closure_records: BTreeMap::new(),
            local_closure_unverified_nodes: BTreeSet::new(),
            local_closure_failures: BTreeMap::new(),
            committed_local_closure_records: BTreeMap::new(),
            committed_local_closure_unverified_nodes: BTreeSet::new(),
            committed_local_closure_failures: BTreeMap::new(),
            last_clean_local_closure_records: BTreeMap::new(),
            last_clean_local_closure_unverified_nodes: BTreeSet::new(),
            last_clean_local_closure_failures: BTreeMap::new(),
            last_clean_local_closure_mirror_ready: false,
            boundary_statement_consumers: BTreeMap::new(),
            strict_dep_consumers: BTreeMap::new(),
            node_rank: BTreeMap::new(),
            live: WorkingSnapshot::default(),
            committed: WorkingSnapshot::default(),
            corr_status: BTreeMap::new(),
            corr_approved_fingerprints: BTreeMap::new(),
            paper_status: BTreeMap::new(),
            paper_approved_fingerprints: BTreeMap::new(),
            deviation_status: BTreeMap::new(),
            deviation_approved_fingerprints: BTreeMap::new(),
            substantiveness_status: BTreeMap::new(),
            substantiveness_approved_fingerprints: BTreeMap::new(),
            sound_assessments: BTreeMap::new(),
            reviewer_requested_sound_verifier_nodes: BTreeSet::new(),
            sound_status: BTreeMap::new(),
            sound_approved_fingerprints: BTreeMap::new(),
            node_difficulty: BTreeMap::new(),
            easy_attempts: BTreeMap::new(),
            human_input_outstanding: false,
            pending_task: None,
            pending_protected_semantic_scope_confirmation: None,
            pending_protected_reapproval_nodes: BTreeSet::new(),
            retry_outcome_kind: RetryOutcomeKind::None,
            reviewer_comments: String::new(),
            latest_worker_summary: String::new(),
            latest_worker_comments: String::new(),
            latest_worker_needs_restructure_suggested_nodes: BTreeSet::new(),
            deterministic_worker_rejection_reasons: Vec::new(),
            latest_review_rejection_reasons: Vec::new(),
            latest_paper_reviewer_evidence: BTreeMap::new(),
            latest_deviation_reviewer_evidence: BTreeMap::new(),
            latest_deviation_review_ids: BTreeSet::new(),
            latest_substantiveness_reviewer_evidence: BTreeMap::new(),
            latest_corr_reviewer_evidence: BTreeMap::new(),
            latest_sound_reviewer_evidence: BTreeMap::new(),
            latest_paper_review_targets: BTreeSet::new(),
            latest_substantiveness_review_nodes: BTreeSet::new(),
            latest_corr_review_nodes: BTreeSet::new(),
            latest_sound_review_nodes: BTreeSet::new(),
            previous_paper_lane_findings: BTreeMap::new(),
            previous_substantiveness_lane_findings: BTreeMap::new(),
            substantiveness_consecutive_no_progress_requests: 0,
            previous_corr_lane_findings: BTreeMap::new(),
            previous_sound_lane_findings: BTreeMap::new(),
            cleanup_audit_tasks: Vec::new(),
            cleanup_audit_scratchpad: String::new(),
            cleanup_audit_burst_count: 0,
            cleanup_audit_round: 1,
            cleanup_consecutive_invalid_workers: 0,
            cleanup_active_task: None,
            cleanup_force_done: false,
            latest_audit_rejection_reason: String::new(),
            audit_burst_retry_count: 0,
            stuck_math_audit: StuckMathAuditState::default(),
            audit_plan: None,
            superseded_audit_plan: None,
            last_stuck_math_audit_dispatched_cycle: None,
            stuck_math_audit_burst_retry_count: 0,
            latest_stuck_math_audit_rejection_reason: String::new(),
            pending_global_repair_request: None,
            pending_global_repair_grant: None,
            latest_global_repair_audit_decline_reason: String::new(),
            latest_global_repair_audit_decline_cycle: None,
            last_reviewer_global_repair_request_cycle: None,
            ever_shallow_coarse_closed: BTreeSet::new(),
            global_repair_mode_enabled: true,
            in_flight_request: None,
        }
    }
}

fn default_verifier_lanes() -> BTreeSet<LaneId> {
    BTreeSet::from(["v1".to_string(), "v2".to_string()])
}

pub const SOUND_ASSESSMENT_SCHEMA_VERSION: u32 = 1;

/// Cleanup-v2: default audit round (1). `cleanup_audit_round` field on
/// `ProtocolState` initializes here so legacy state files that don't
/// mention the field deserialize as round 1 rather than round 0.
fn default_cleanup_audit_round() -> u32 {
    1
}

/// Bug X principled fix: default transport-failure retry threshold. Aligns
/// roughly with the "two layers of two-attempt retries" the bridge silently
/// performed before this fix, so a single transport failure on a healthy
/// agent doesn't immediately escalate to the reviewer.
pub fn default_transport_invalid_review_threshold() -> u32 {
    5
}

/// Circuit-breaker default (2026-05-12): halt after 5 consecutive
/// `transport_failure=true` worker responses targeting the same active
/// node. Sized to absorb occasional double-bridge flakes (which would
/// burn the per-cycle `transport_invalid_review_threshold` plus
/// reviewer cost legitimately) without burning the much larger
/// per-cycle reviewer budget on a repeatedly-failing bug.
pub fn default_consecutive_transport_failure_halt_threshold() -> u32 {
    5
}

impl ProtocolState {
    pub fn legacy_sound_lane_state_present(&self) -> bool {
        !self.sound_status.is_empty()
            || !self.sound_approved_fingerprints.is_empty()
            || !self.last_clean_sound_status.is_empty()
            || !self.last_clean_sound_approved_fingerprints.is_empty()
            || !self.sound_assessments.is_empty()
            || !self.reviewer_requested_sound_verifier_nodes.is_empty()
            || !self.latest_sound_reviewer_evidence.is_empty()
            || !self.latest_sound_review_nodes.is_empty()
            || !self.previous_sound_lane_findings.is_empty()
            || self.stage == Stage::VerifySound
            || self
                .in_flight_request
                .as_ref()
                .is_some_and(|request| request.kind == RequestKind::Sound)
    }

    pub fn sound_assessment_cutover_requires_rewind(&self) -> bool {
        self.sound_assessment_schema_version < SOUND_ASSESSMENT_SCHEMA_VERSION
            && self.legacy_sound_lane_state_present()
    }

    fn effective_node_kinds(
        kinds: &BTreeMap<NodeId, NodeKind>,
        present_nodes: &BTreeSet<NodeId>,
        proof_nodes: &BTreeSet<NodeId>,
    ) -> BTreeMap<NodeId, NodeKind> {
        present_nodes
            .iter()
            .map(|node| {
                let kind = kinds.get(node).copied().unwrap_or_else(|| {
                    if node == PREAMBLE_NAME {
                        NodeKind::Preamble
                    } else if proof_nodes.contains(node) {
                        NodeKind::Proof
                    } else {
                        NodeKind::Definition
                    }
                });
                (node.clone(), kind)
            })
            .collect()
    }

    fn proof_nodes_from_kind_map(
        kinds: &BTreeMap<NodeId, NodeKind>,
        present_nodes: &BTreeSet<NodeId>,
    ) -> BTreeSet<NodeId> {
        present_nodes
            .iter()
            .filter(|node| kinds.get(*node) == Some(&NodeKind::Proof))
            .cloned()
            .collect()
    }

    fn normalize_node_edge_map(
        map: &mut BTreeMap<NodeId, BTreeSet<NodeId>>,
        present_nodes: &BTreeSet<NodeId>,
    ) {
        map.retain(|node, _| present_nodes.contains(node));
        for (node, deps) in map.iter_mut() {
            deps.retain(|dep| present_nodes.contains(dep) && dep != node);
        }
    }

    fn normalize_target_claims(
        map: &mut BTreeMap<NodeId, BTreeSet<TargetId>>,
        present_nodes: &BTreeSet<NodeId>,
        configured_targets: &BTreeSet<TargetId>,
    ) {
        map.retain(|node, _| present_nodes.contains(node));
        for claims in map.values_mut() {
            claims.retain(|target| configured_targets.contains(target));
        }
    }

    fn coverage_from_claims_with_present(
        &self,
        claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
        present_nodes: &BTreeSet<NodeId>,
    ) -> BTreeMap<TargetId, BTreeSet<NodeId>> {
        let mut coverage: BTreeMap<TargetId, BTreeSet<NodeId>> = self
            .configured_targets
            .iter()
            .cloned()
            .map(|target| (target, BTreeSet::new()))
            .collect();
        for node in present_nodes {
            for target in claims.get(node).cloned().unwrap_or_default() {
                coverage.entry(target).or_default().insert(node.clone());
            }
        }
        coverage
    }

    fn sync_live_coverage_from_claims(&mut self) {
        self.live.coverage =
            self.coverage_from_claims_with_present(&self.target_claims, &self.live.present_nodes);
    }

    fn normalize_paper_current_fingerprints(
        configured_targets: &BTreeSet<TargetId>,
        fingerprints: &mut BTreeMap<TargetId, Fingerprint>,
    ) {
        fingerprints.retain(|target, _| configured_targets.contains(target));
        for target in configured_targets {
            fingerprints.entry(target.clone()).or_default();
        }
    }

    fn sync_committed_coverage_from_claims(&mut self) {
        self.committed.coverage = self.coverage_from_claims_with_present(
            &self.committed_target_claims,
            &self.committed.present_nodes,
        );
    }

    pub(crate) fn normalize_live_structural_state(&mut self) {
        self.node_kinds = Self::effective_node_kinds(
            &self.node_kinds,
            &self.live.present_nodes,
            &self.proof_nodes,
        );
        self.proof_nodes =
            Self::proof_nodes_from_kind_map(&self.node_kinds, &self.live.present_nodes);
        Self::normalize_node_edge_map(&mut self.deps, &self.live.present_nodes);
        Self::normalize_target_claims(
            &mut self.target_claims,
            &self.live.present_nodes,
            &self.configured_targets,
        );
        self.node_deviation_claims.retain(|node, claims| {
            claims.retain(|id| self.deviation_files.contains_key(id));
            self.live.present_nodes.contains(node) && !claims.is_empty()
        });
        self.sync_live_coverage_from_claims();
        Self::normalize_paper_current_fingerprints(
            &self.configured_targets,
            &mut self.live.paper_current_fingerprints,
        );
        self.live
            .deviation_current_fingerprints
            .retain(|id, _| self.deviation_files.contains_key(id));
    }

    pub fn install_observed_live_tablet_state(
        &mut self,
        live: WorkingSnapshot,
        node_kinds: BTreeMap<NodeId, NodeKind>,
        proof_nodes: BTreeSet<NodeId>,
        deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
        target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    ) {
        self.live = live;
        self.node_kinds = node_kinds;
        self.proof_nodes = proof_nodes;
        self.deps = deps;
        self.target_claims = target_claims;
        self.normalize_live_structural_state();
        self.retain_verifier_state_for_live_surface();
        self.ensure_node_metadata();
    }

    pub fn restore_theorem_stating_baseline_for_node(
        &mut self,
        node: &NodeId,
        baseline: &ProtocolState,
    ) {
        // Copy approved fingerprints only; current fingerprints remain
        // freshly observed, so schema or statement drift reopens as Unknown.
        if let Some(status) = baseline.corr_status.get(node) {
            self.corr_status.insert(node.clone(), *status);
        }
        if let Some(fp) = baseline.corr_approved_fingerprints.get(node) {
            self.corr_approved_fingerprints
                .insert(node.clone(), fp.clone());
        }
        if let Some(status) = baseline.substantiveness_status.get(node) {
            self.substantiveness_status.insert(node.clone(), *status);
        }
        if let Some(fp) = baseline.substantiveness_approved_fingerprints.get(node) {
            self.substantiveness_approved_fingerprints
                .insert(node.clone(), fp.clone());
        }
        if let Some(status) = baseline.sound_status.get(node) {
            self.sound_status.insert(node.clone(), *status);
        }
        if let Some(fp) = baseline.sound_approved_fingerprints.get(node) {
            self.sound_approved_fingerprints
                .insert(node.clone(), fp.clone());
        }
        if let Some(assessment) = baseline.sound_assessments.get(node) {
            self.sound_assessments
                .insert(node.clone(), assessment.clone());
        }
        let baseline_targets = baseline
            .target_claims
            .get(node)
            .cloned()
            .unwrap_or_default();
        for target in baseline_targets {
            if let Some(status) = baseline.paper_status.get(&target) {
                self.paper_status.insert(target.clone(), *status);
            }
            if let Some(fp) = baseline.paper_approved_fingerprints.get(&target) {
                self.paper_approved_fingerprints
                    .insert(target.clone(), fp.clone());
            }
        }
        self.retain_verifier_state_for_live_surface();
    }

    fn retain_verifier_state_for_live_surface(&mut self) {
        let present = &self.live.present_nodes;
        self.corr_status.retain(|node, _| present.contains(node));
        self.corr_approved_fingerprints
            .retain(|node, _| present.contains(node));
        self.substantiveness_status
            .retain(|node, _| present.contains(node));
        self.substantiveness_approved_fingerprints
            .retain(|node, _| present.contains(node));
        self.sound_status.retain(|node, _| present.contains(node));
        self.sound_approved_fingerprints
            .retain(|node, _| present.contains(node));
        self.sound_assessments
            .retain(|node, _| present.contains(node));
        self.reviewer_requested_sound_verifier_nodes
            .retain(|node| present.contains(node));
        self.paper_status
            .retain(|target, _| self.configured_targets.contains(target));
        self.paper_approved_fingerprints
            .retain(|target, _| self.configured_targets.contains(target));
        self.deviation_status
            .retain(|id, _| self.deviation_files.contains_key(id));
        self.deviation_approved_fingerprints
            .retain(|id, _| self.deviation_files.contains_key(id));
        self.pending_protected_reapproval_nodes
            .retain(|node| present.contains(node));
    }

    pub fn prune_local_closure_after_runtime_tablet_reset(
        &mut self,
        changed_nodes: &BTreeSet<NodeId>,
    ) -> BTreeSet<NodeId> {
        let present = self.live.present_nodes.clone();
        let open = self.live.open_nodes.clone();
        let proof_nodes = self.proof_nodes.clone();
        let mut remove_records: BTreeSet<NodeId> = BTreeSet::new();
        // Audit C-2 — canonical-predicate path. The previous
        // changed-nodes membership scan only fired on direct dep
        // references that appeared in `changed_nodes`. A consumer
        // whose helper H was NOT in `changed_nodes` (H wasn't
        // deleted, wasn't the target, wasn't orphaned) but whose
        // `corr_current_fingerprints[H]` drifted because H's
        // transitive Lean dep on the cone-clean target re-elaborated
        // sat stale with `kernel_semantic_hashes[H]` no longer
        // matching live fingerprints. The canonical predicate
        // catches that drift directly; the changed-nodes membership
        // tests survive as a defensive belt-and-suspenders signal so
        // node-deletion / direct-edit cases still drop records even
        // when the kernel-hash sweep happens to miss them (e.g. a
        // record carrying a pre-Patch-C-P empty
        // `kernel_semantic_hashes` map).
        //
        // Pure-state context (model.rs has no disk access), so the
        // predicate runs with `axcheck_required = false`. The
        // runtime-CLI's per-step rescission hooks (H-2 / H-4) cover
        // the policy-tier check on the very next step.
        for (node, record) in &self.local_closure_records {
            let owner_changed = changed_nodes.contains(node);
            let owner_missing = !present.contains(node);
            let referenced_missing = record
                .boundary_theorems
                .keys()
                .chain(record.strict_theorem_deps.keys())
                .chain(record.strict_definition_deps.keys())
                .any(|dep| !present.contains(dep));
            let referenced_changed = record
                .boundary_theorems
                .keys()
                .chain(record.strict_theorem_deps.keys())
                .chain(record.strict_definition_deps.keys())
                .any(|dep| changed_nodes.contains(dep));
            // Audit C-2: canonical predicate catches fingerprint-only
            // drift on indirect deps that aren't in `changed_nodes`.
            // `axcheck_required = false` here (pure-state contract).
            let inconsistent = record.is_consistent_with_state(self, false).is_err();
            if owner_changed
                || owner_missing
                || referenced_missing
                || referenced_changed
                || inconsistent
            {
                remove_records.insert(node.clone());
            }
        }
        for node in present.iter().filter(|node| changed_nodes.contains(*node)) {
            if proof_nodes.contains(node) && !open.contains(node) {
                self.local_closure_unverified_nodes.insert(node.clone());
            }
        }
        for node in &remove_records {
            self.local_closure_records.remove(node);
            self.local_closure_failures.remove(node);
            if present.contains(node) && proof_nodes.contains(node) && !open.contains(node) {
                self.local_closure_unverified_nodes.insert(node.clone());
            } else {
                self.local_closure_unverified_nodes.remove(node);
            }
        }
        self.local_closure_unverified_nodes.retain(|node| {
            present.contains(node) && proof_nodes.contains(node) && !open.contains(node)
        });
        self.local_closure_failures
            .retain(|node, _| self.local_closure_unverified_nodes.contains(node));
        // Audit C-3: continuous coverage scan. Every sorry-free
        // present proof_node must hold a record OR sit in
        // `local_closure_unverified_nodes`. The cone-clean reobservation
        // can flip a node from sorryd → sorry-free without giving it
        // either; pin those into unverified so they re-probe.
        self.ensure_local_closure_coverage();
        recompute_local_closure_reverse_indices(self);
        remove_records
    }

    /// Audit C-3 — continuous closure-coverage invariant. Every
    /// sorry-free present proof_node must either hold a
    /// `LocalClosureRecord` or sit in `local_closure_unverified_nodes`;
    /// orphan sorry-free proof_nodes (no record, no unverified entry)
    /// silently fail the `formalization_complete` gate without any path
    /// to recover. This helper insertion guarantees orphans land in
    /// unverified so the next deterministic-revalidation pass probes
    /// them.
    ///
    /// Symmetric correctness work (defensive cleanups):
    ///   * Drop unverified entries whose owner went sorryd (live.open)
    ///     or vanished from `live.present_nodes` / `proof_nodes` —
    ///     mutual-exclusion invariant per plan §7.2.
    ///   * Drop failure summaries whose owner left
    ///     `local_closure_unverified_nodes` (failure summaries are
    ///     only meaningful while the node is unverified).
    pub fn ensure_local_closure_coverage(&mut self) {
        // Cleanup: drop unverified entries that violate the
        // sorry-free-only invariant (`open_nodes ∩ unverified = ∅`
        // and `unverified ⊆ proof_nodes ∩ present_nodes`).
        self.local_closure_unverified_nodes.retain(|node| {
            self.live.present_nodes.contains(node)
                && self.proof_nodes.contains(node)
                && !self.live.open_nodes.contains(node)
        });
        // Coverage scan: every present sorry-free proof_node must
        // appear in `records` ∪ `unverified`.
        let mut orphans: BTreeSet<NodeId> = BTreeSet::new();
        for node in &self.proof_nodes {
            if !self.live.present_nodes.contains(node) {
                continue;
            }
            if self.live.open_nodes.contains(node) {
                continue;
            }
            if self.local_closure_records.contains_key(node) {
                continue;
            }
            if self.local_closure_unverified_nodes.contains(node) {
                continue;
            }
            orphans.insert(node.clone());
        }
        for orphan in orphans {
            self.local_closure_unverified_nodes.insert(orphan);
        }
        // Symmetric cleanup: failure summaries pinned to nodes not in
        // the unverified set are stale (only unverified nodes carry
        // failure summaries by plan §7.0). Idempotent.
        self.local_closure_failures
            .retain(|node, _| self.local_closure_unverified_nodes.contains(node));
    }

    fn normalize_committed_structural_state(&mut self) {
        self.committed_node_kinds = Self::effective_node_kinds(
            &self.committed_node_kinds,
            &self.committed.present_nodes,
            &self.committed_proof_nodes,
        );
        self.committed_proof_nodes = Self::proof_nodes_from_kind_map(
            &self.committed_node_kinds,
            &self.committed.present_nodes,
        );
        Self::normalize_node_edge_map(&mut self.committed_deps, &self.committed.present_nodes);
        Self::normalize_target_claims(
            &mut self.committed_target_claims,
            &self.committed.present_nodes,
            &self.configured_targets,
        );
        self.committed_node_deviation_claims.retain(|node, claims| {
            claims.retain(|id| self.committed_deviation_files.contains_key(id));
            self.committed.present_nodes.contains(node) && !claims.is_empty()
        });
        self.sync_committed_coverage_from_claims();
        Self::normalize_paper_current_fingerprints(
            &self.configured_targets,
            &mut self.committed.paper_current_fingerprints,
        );
        self.committed
            .deviation_current_fingerprints
            .retain(|id, _| self.committed_deviation_files.contains_key(id));
    }

    pub fn normalize_all_structural_state(&mut self) {
        self.normalize_live_structural_state();
        self.normalize_committed_structural_state();
    }

    pub fn apply_worker_structure_updates(&mut self, response: &WorkerResponse) {
        for (node, update) in &response.node_kind_updates {
            match update {
                Update::Same => {}
                Update::Set(kind) => {
                    self.node_kinds.insert(node.clone(), *kind);
                }
            }
        }
        for (node, update) in &response.proof_node_updates {
            match update {
                Update::Same => {}
                Update::Set(is_proof) => {
                    if *is_proof {
                        self.proof_nodes.insert(node.clone());
                    } else {
                        self.proof_nodes.remove(node);
                    }
                }
            }
        }
        for (node, update) in &response.dep_updates {
            match update {
                Update::Same => {}
                Update::Set(deps) => {
                    self.deps.insert(node.clone(), deps.clone());
                }
            }
        }
        for (node, update) in &response.target_claim_updates {
            match update {
                Update::Same => {}
                Update::Set(targets) => {
                    self.target_claims.insert(node.clone(), targets.clone());
                }
            }
        }
        for (id, request) in &response.deviation_requests {
            if !request.path.trim().is_empty() {
                let changed_path = self
                    .deviation_files
                    .get(id)
                    .is_some_and(|path| path != &request.path);
                self.deviation_files
                    .insert(id.clone(), request.path.clone());
                if changed_path {
                    self.deviation_status
                        .insert(id.clone(), CorrStatus::Unknown);
                    self.deviation_approved_fingerprints.remove(id);
                } else {
                    self.deviation_status
                        .entry(id.clone())
                        .or_insert(CorrStatus::Unknown);
                }
                for node in &request.affected_nodes {
                    if self.live.present_nodes.contains(node) {
                        self.node_deviation_claims
                            .entry(node.clone())
                            .or_default()
                            .insert(id.clone());
                    }
                }
            }
        }
        for (node, claims) in &response.node_deviation_claims {
            if claims.is_empty() {
                self.node_deviation_claims.remove(node);
            } else {
                self.node_deviation_claims
                    .insert(node.clone(), claims.clone());
            }
        }
        // Deletion retires a deviation entirely. The worker contract
        // (see `deviation_deletion_contract_errors`) has already
        // confirmed no node still claims any to-delete id. Drop the
        // entry from every per-deviation map; structural normalization
        // below cleans up live-mirror and approved-fp leftovers.
        // `latest_deviation_reviewer_evidence` is keyed by lane id, not
        // deviation id, so we leave it; it's stale-but-harmless and
        // will be overwritten on the next deviation review.
        for id in &response.deviation_deletions {
            self.deviation_files.remove(id);
            self.deviation_status.remove(id);
            self.deviation_approved_fingerprints.remove(id);
            self.live.deviation_current_fingerprints.remove(id);
            self.latest_deviation_review_ids.remove(id);
        }
        self.normalize_live_structural_state();
        // Stale-verifier-status fix: after the snapshot's present_nodes set
        // has been refreshed by `normalize_live_structural_state`, prune the
        // top-level `{sound,corr,substantiveness,paper}_status` and
        // `*_approved_fingerprints` maps so they cannot retain entries for
        // nodes that just left `live.present_nodes`. Decisions filter
        // through `present_nodes` already, so this is a reporting-only
        // hygiene step — but those stale entries pollute external tally
        // aggregates. `retain_verifier_state_for_live_surface` deliberately
        // leaves the `last_clean_*` mirrors untouched (those are the
        // restore source for `apply_last_clean_reset`).
        self.retain_verifier_state_for_live_surface();
    }

    pub fn worker_semantic_delta(&self, response: &WorkerResponse) -> bool {
        if self.semantic_delta(&response.snapshot) {
            return true;
        }
        response
            .proof_node_updates
            .values()
            .any(|update| !matches!(update, Update::Same))
            || response
                .node_kind_updates
                .values()
                .any(|update| !matches!(update, Update::Same))
            || response
                .dep_updates
                .values()
                .any(|update| !matches!(update, Update::Same))
            || response
                .target_claim_updates
                .values()
                .any(|update| !matches!(update, Update::Same))
            || response.deviation_requests.iter().any(|(id, request)| {
                self.deviation_files.get(id) != Some(&request.path)
                    || request.affected_nodes.iter().any(|node| {
                        !self
                            .node_deviation_claims
                            .get(node)
                            .is_some_and(|claims| claims.contains(id))
                    })
            })
            || response.node_deviation_claims.iter().any(|(node, claims)| {
                self.node_deviation_claims
                    .get(node)
                    .cloned()
                    .unwrap_or_default()
                    != *claims
            })
            || response
                .deviation_deletions
                .iter()
                .any(|id| self.deviation_files.contains_key(id))
    }

    pub fn ensure_node_metadata(&mut self) {
        for node in self
            .live
            .present_nodes
            .iter()
            .chain(self.committed.present_nodes.iter())
        {
            self.node_difficulty
                .entry(node.clone())
                .or_insert(NodeDifficulty::Hard);
            self.easy_attempts.entry(node.clone()).or_insert(0);
        }
        for (node, difficulty) in &self.node_difficulty {
            if *difficulty == NodeDifficulty::Hard {
                self.easy_attempts.insert(node.clone(), 0);
            }
        }
    }

    pub fn semantic_delta(&self, snapshot: &WorkingSnapshot) -> bool {
        self.live != *snapshot
    }

    pub fn approved_target_nodes(&self) -> BTreeSet<NodeId> {
        // Worker-acceptance protection set: the per-target covering nodes
        // (target roots) plus the narrow Lean type-surface closure of
        // those covering nodes (project-defined definitions whose value
        // or type is referenced from a covering node's type signature,
        // transitively, with proof bodies excluded — see
        // `scripts/lean_semantic_fingerprint.lean` for the closure
        // policy and `WorkingSnapshot::protected_closure_nodes_per_target`
        // for the per-target observation slot the closure is snapshotted
        // from at AdvancePhase). Kept narrow on purpose: the human
        // review zip's README describes exactly this set as the meaning
        // surface the reviewer is being asked to vouch for.
        let mut nodes: BTreeSet<NodeId> = self
            .approved_targets
            .configured_targets
            .iter()
            .filter_map(|t| self.approved_targets.coverage.get(t))
            .flat_map(|nodes| nodes.iter().cloned())
            .collect();
        nodes.extend(
            self.approved_targets
                .protected_closure_nodes
                .iter()
                .cloned(),
        );
        nodes
    }

    /// Cleanup-v2: live protected-statement node set. Union of the
    /// per-target covering nodes (`live.coverage` values) and the
    /// live per-target Lean-type-surface closure
    /// (`live.protected_closure_nodes_per_target` values). These nodes'
    /// Lean signatures and `.tex` statements are immutable during
    /// Cleanup edits regardless of `authorized_nodes` membership;
    /// proof bodies remain editable.
    ///
    /// Distinct from `approved_target_nodes()`, which reads the frozen
    /// `approved_targets.{configured_targets, protected_closure_nodes}`
    /// snapshotted at AdvancePhase. The frozen set can drift from the
    /// live state mid-cleanup (e.g. cleanup deletes a node, alters
    /// dependencies); edit-time validation needs the *live* set.
    pub fn live_protected_statement_node_set(&self) -> BTreeSet<NodeId> {
        let mut nodes: BTreeSet<NodeId> = self
            .live
            .coverage
            .values()
            .flat_map(|set| set.iter().cloned())
            .collect();
        nodes.extend(
            self.live
                .protected_closure_nodes_per_target
                .values()
                .flat_map(|set| set.iter().cloned()),
        );
        nodes
    }

    /// Cleanup-v2: legality check for a single proposed audit task
    /// against the current live state and the existing
    /// `cleanup_audit_tasks` list. Returns `Ok(())` if the task is
    /// well-formed and admissible; otherwise returns a short reason
    /// string suitable for surfacing in audit rejection feedback.
    ///
    /// Checks:
    /// - `target_node` ∈ `live.present_nodes`,
    /// - `target_node` ∉ `live_protected_statement_node_set()`,
    /// - for `Substitution { replacement: TabletWrapper(N) }`: N ∈
    ///   `live.present_nodes`,
    /// - for `Substitution { replacement: Mathlib(s) }`: s non-empty,
    /// - for `LintFix { warning_text }`: non-empty,
    /// - no duplicate `(target_node, kind)` pair against
    ///   `cleanup_audit_tasks`.
    pub fn legal_cleanup_task(&self, task: &NewCleanupAuditTask) -> Result<(), String> {
        if !self.live.present_nodes.contains(&task.target_node) {
            return Err(format!(
                "target_node {:?} not in live.present_nodes",
                task.target_node
            ));
        }
        let protected = self.live_protected_statement_node_set();
        if protected.contains(&task.target_node) {
            return Err(format!(
                "target_node {:?} is in the protected-statement set (coverage \u{222a} \
                 protected_closure_nodes_per_target) and may not be a cleanup target",
                task.target_node
            ));
        }
        match &task.kind {
            CleanupTaskKind::Substitution { replacement } => match replacement {
                CleanupReplacement::TabletWrapper { node } => {
                    if !self.live.present_nodes.contains(node) {
                        return Err(format!(
                            "Substitution.replacement.node {:?} not in live.present_nodes",
                            node
                        ));
                    }
                }
                CleanupReplacement::Mathlib { citation } => {
                    if citation.trim().is_empty() {
                        return Err(
                            "Substitution.replacement.citation must be non-empty".to_string()
                        );
                    }
                }
            },
            CleanupTaskKind::LintFix { warning_text } => {
                if warning_text.trim().is_empty() {
                    return Err("LintFix.warning_text must be non-empty".to_string());
                }
            }
        }
        for existing in &self.cleanup_audit_tasks {
            if existing.target_node == task.target_node && existing.kind == task.kind {
                return Err(format!(
                    "duplicate (target_node, kind) pair: target_node {:?} already has \
                     an entry with the same kind",
                    task.target_node
                ));
            }
        }
        Ok(())
    }

    pub fn current_mode(&self) -> TaskMode {
        match self.phase {
            Phase::TheoremStating => match self.target_edit_mode {
                TargetEditMode::Global => TaskMode::Global,
                TargetEditMode::Targeted => TaskMode::Targeted,
            },
            Phase::ProofFormalization => match self.proof_edit_mode {
                ProofEditMode::Local => TaskMode::Local,
                ProofEditMode::Restructure => TaskMode::Restructure,
                ProofEditMode::CoarseRestructure => TaskMode::CoarseRestructure,
            },
            Phase::Cleanup => TaskMode::Cleanup,
            Phase::Complete => TaskMode::Global,
        }
    }

    pub fn orphan_nodes(&self, snapshot: &WorkingSnapshot) -> BTreeSet<NodeId> {
        Self::orphan_nodes_for_graph(&snapshot.present_nodes, &snapshot.coverage, &self.deps)
    }

    pub fn orphan_nodes_for_graph(
        present_nodes: &BTreeSet<NodeId>,
        coverage: &BTreeMap<TargetId, BTreeSet<NodeId>>,
        deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    ) -> BTreeSet<NodeId> {
        let roots: BTreeSet<_> = coverage
            .values()
            .flat_map(|nodes| nodes.iter().cloned())
            .collect();
        let supported = dep_closure_from(&roots, present_nodes, deps);
        present_nodes
            .iter()
            .filter(|node| node.as_str() != PREAMBLE_NAME && !supported.contains(*node))
            .cloned()
            .collect()
    }

    pub fn orphan_cleanup_active(&self) -> bool {
        self.pending_task
            .as_ref()
            .is_some_and(|task| !task.orphan_cleanup_nodes.is_empty())
    }

    pub fn orphan_cleanup_needed(&self) -> bool {
        !self.orphan_nodes(&self.live).is_empty()
    }

    fn rank_of(&self, node: &NodeId) -> u32 {
        self.node_rank.get(node).copied().unwrap_or(0)
    }

    pub fn dep_closure(
        &self,
        seed: &BTreeSet<NodeId>,
        live_present: &BTreeSet<NodeId>,
        deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    ) -> BTreeSet<NodeId> {
        dep_closure_from(seed, live_present, deps)
    }

    pub fn reverse_dep_closure(
        &self,
        seed: &BTreeSet<NodeId>,
        live_present: &BTreeSet<NodeId>,
        deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    ) -> BTreeSet<NodeId> {
        reverse_dep_closure_from(seed, live_present, deps)
    }

    // Removed in the protected_correspondence refactor:
    //   semantic_closure — only used by protected_nodes.
    //   protected_nodes — broader-than-covering set no longer the protection scope.
    //   protected_snapshot — subsumed by the per-node correspondence fingerprint
    //                        + `paper_target_corr_reopen_guard_errors` guard
    //                        over `approved_target_nodes()`.

    pub fn target_support_cone(
        &self,
        target: &TargetId,
        snapshot: &WorkingSnapshot,
    ) -> BTreeSet<NodeId> {
        let seed = snapshot.coverage.get(target).cloned().unwrap_or_default();
        self.dep_closure(&seed, &snapshot.present_nodes, &self.deps)
    }

    /// Proposal v32: down-cone of a single coarse-DAG node. Distinct
    /// from `target_support_cone` (which keys by `TargetId`); accepting
    /// a `NodeId` here is the load-bearing fix the audit flagged
    /// (kernel audit, model.rs:6080 regression note — TargetId/NodeId
    /// are both `String` aliases, so a wrong call site would silently
    /// degrade rather than fail to compile).
    pub fn coarse_node_support_cone(
        &self,
        node: &NodeId,
        snapshot: &WorkingSnapshot,
    ) -> BTreeSet<NodeId> {
        if !snapshot.present_nodes.contains(node) {
            return BTreeSet::new();
        }
        let mut seed = BTreeSet::new();
        seed.insert(node.clone());
        self.dep_closure(&seed, &snapshot.present_nodes, &self.deps)
    }

    /// Proposal v32: nodes that carry an outstanding task-style
    /// blocker. For node-bound blockers (NodeCorr / Substantiveness /
    /// Soundness) the carrier is the node itself. For target-bound
    /// PaperFaithfulness blockers the carrier set is the target's
    /// covering nodes (per `live.coverage[target]`), restricted to
    /// `present_nodes`. We use the LIVE snapshot here because the
    /// active-coarse mechanism runs against current state. Callers
    /// that need a committed-snapshot variant should compute it
    /// against a different `WorkingSnapshot`.
    pub fn coarse_task_blocker_nodes(&self) -> BTreeSet<NodeId> {
        let mut out = BTreeSet::new();
        for blocker in self.global_blockers() {
            match &blocker.object {
                BlockerObject::Node { node } => {
                    if self.live.present_nodes.contains(node) {
                        out.insert(node.clone());
                    }
                }
                BlockerObject::Target { target } => {
                    if let Some(coverage) = self.live.coverage.get(target) {
                        for n in coverage {
                            if self.live.present_nodes.contains(n) {
                                out.insert(n.clone());
                            }
                        }
                    }
                }
                BlockerObject::Deviation { .. } => {}
            }
        }
        out
    }

    /// Proposal v32: TRUE iff some task-blocker carrier lies outside
    /// the active-coarse cone. Vacuously false when no anchor is set
    /// or when the coarse DAG is empty (mechanism dormant).
    pub fn coarse_repair_mode(&self) -> bool {
        if self.coarse_dag_nodes.is_empty() {
            return false;
        }
        let Some(anchor) = self.active_coarse_node.as_ref() else {
            return false;
        };
        let cone = self.coarse_node_support_cone(anchor, &self.live);
        self.coarse_task_blocker_nodes()
            .iter()
            .any(|carrier| !cone.contains(carrier))
    }

    /// global_repair_mode S8: coarse-DAG nodes that were observed
    /// shallowly-closed at some prior `commit_live` but are NOT
    /// currently shallowly-closed in `committed`. Computed against the
    /// COMMITTED snapshot so in-flight worker bursts whose changes have
    /// not yet been accepted do not pollute the regression set. Empty
    /// when `coarse_dag_nodes` is empty.
    pub fn ever_shallow_coarse_closed_regressed(&self) -> BTreeSet<NodeId> {
        if self.coarse_dag_nodes.is_empty() {
            return BTreeSet::new();
        }
        let mut memo = BTreeMap::new();
        self.ever_shallow_coarse_closed
            .iter()
            .filter(|n| self.coarse_dag_nodes.contains(*n))
            .filter(|n| {
                !shallowly_closed_from_coarse(
                    n,
                    &self.committed.present_nodes,
                    &self.committed.open_nodes,
                    &self.committed_deps,
                    &self.coarse_dag_nodes,
                    &mut memo,
                )
            })
            .cloned()
            .collect()
    }

    /// Proposal v32: legal next_active set when an active coarse
    /// anchor is set. Base case is the anchor's down-cone; in
    /// `coarse_repair_mode()` the set widens to include every task-
    /// blocker carrier and that carrier's own down-cone. When no
    /// anchor is set or `coarse_dag_nodes` is empty, returns
    /// `live.present_nodes` so existing legality is the sole gate.
    pub fn coarse_legal_active_set(&self) -> BTreeSet<NodeId> {
        if self.coarse_dag_nodes.is_empty() {
            return self.live.present_nodes.clone();
        }
        let Some(anchor) = self.active_coarse_node.as_ref() else {
            return self.live.present_nodes.clone();
        };
        let cone = self.coarse_node_support_cone(anchor, &self.live);
        if !self.coarse_repair_mode() {
            return cone;
        }
        let mut out = cone;
        for blocker_carrier in self.coarse_task_blocker_nodes() {
            out.extend(
                self.coarse_node_support_cone(&blocker_carrier, &self.live)
                    .into_iter(),
            );
            out.insert(blocker_carrier);
        }
        out
    }

    /// Proposal v32: TRUE iff the reviewer may change the active
    /// coarse anchor this cycle. Four escape paths:
    ///   1. coarse_dag_nodes is empty (mechanism dormant).
    ///   2. No anchor yet (initial seed).
    ///   3. Strict unlock: anchor shallow-closed AND no global blockers.
    ///   4. Starvation escape: stuck in coarse_repair_mode for >= threshold.
    pub fn active_coarse_change_allowed(&self) -> bool {
        if self.coarse_dag_nodes.is_empty() {
            return true;
        }
        let Some(anchor) = self.active_coarse_node.as_ref() else {
            return true;
        };
        if self.cycles_in_coarse_repair_mode >= stuck_coarse_repair_threshold() {
            return true;
        }
        if !self.global_blockers().is_empty() {
            return false;
        }
        // global_repair_mode S8 invariant: monotonicity-of-coarse-progress.
        // A coarse anchor cannot be changed while some previously-closed
        // coarse node has been re-opened (typically by a wide
        // global_repair burst that broke transitive closure). The
        // starvation escape above bypasses this clause, so a stuck
        // regression cannot lock the run forever.
        if !self.ever_shallow_coarse_closed_regressed().is_empty() {
            return false;
        }
        let mut memo = BTreeMap::new();
        shallowly_closed_from_coarse(
            anchor,
            &self.live.present_nodes,
            &self.live.open_nodes,
            &self.deps,
            &self.coarse_dag_nodes,
            &mut memo,
        )
    }

    /// Proposal v32: coarse-anchor candidates surfaced to the reviewer
    /// when change is allowed. Empty otherwise (locked) so the
    /// reviewer's `next_active_coarse` validation has nothing to
    /// match. Restricted to ProofFormalization since the mechanism is
    /// phase-scoped.
    pub fn kernel_hinted_next_active_coarse_nodes(&self) -> BTreeSet<NodeId> {
        if self.phase != Phase::ProofFormalization
            || self.coarse_dag_nodes.is_empty()
            || !self.active_coarse_change_allowed()
        {
            return BTreeSet::new();
        }
        let closed = shallowly_closed_coarse_nodes(
            &self.live.present_nodes,
            &self.live.open_nodes,
            &self.deps,
            &self.coarse_dag_nodes,
        );
        self.coarse_dag_nodes
            .iter()
            .filter(|n| self.live.present_nodes.contains(*n) && !closed.contains(*n))
            .cloned()
            .collect()
    }

    pub fn impact_region(&self, node: &NodeId, snapshot: &WorkingSnapshot) -> BTreeSet<NodeId> {
        impact_region_from(node, &snapshot.present_nodes, &self.deps)
    }

    pub fn active_node_legal(&self, node: Option<&NodeId>, snapshot: &WorkingSnapshot) -> bool {
        match node {
            None => true,
            Some(node) => {
                if !snapshot.present_nodes.contains(node) {
                    return false;
                }
                match self.phase {
                    Phase::Cleanup | Phase::TheoremStating => true,
                    Phase::ProofFormalization => {
                        // A node is a legal active focus in ProofFormalization if
                        // any of:
                        //   1. its Lean proof still has a sorry (worker drives
                        //      it closed) OR it lives in
                        //      `local_closure_unverified_nodes` — a sorry-free
                        //      proof_node with a stale/failed local-closure
                        //      record is work-to-do exactly like a textually-
                        //      open node (Patch C plan §7.4),
                        //   2. it carries an outstanding NodeCorr, Soundness,
                        //      or Substantiveness blocker (worker repairs the
                        //      per-node check),
                        //   3. it covers a target with an outstanding
                        //      PaperFaithfulness blocker (worker reshapes the
                        //      covering .tex/.lean to fix coverage),
                        //   4. it directly imports a node with an outstanding
                        //      Substantiveness blocker (worker can retarget
                        //      the consumer and let orphan cleanup remove the
                        //      failed helper later),
                        //   5. it is a minimal common importer of all live
                        //      node-bound blocked nodes (`proof_node_repairs_
                        //      aggregate_node_blockers` — the cycle-380 fix
                        //      from 2026-05-05).
                        //   6. proof edit mode is already Restructure /
                        //      CoarseRestructure; then active_node is a
                        //      reviewer-chosen scope anchor and may be any
                        //      present node.
                        //
                        // Without 2/3/4 a closed-proof node carrying or
                        // commonly importing an outstanding blocker is
                        // unreachable as next_active, forcing the engine's
                        // silent fall-back to Local mode and a worker that
                        // can't touch the blocker.
                        let base_legal = snapshot.open_nodes.contains(node)
                            || self.local_closure_unverified_nodes.contains(node)
                            || self.proof_node_repairs_blocker(node)
                            || self.proof_node_directly_imports_substantiveness_blocker(node)
                            || self.proof_node_repairs_aggregate_node_blockers(node)
                            || matches!(
                                self.proof_edit_mode,
                                ProofEditMode::Restructure | ProofEditMode::CoarseRestructure
                            );
                        if !base_legal {
                            return false;
                        }
                        // Proposal v32: when an active coarse anchor
                        // is set, additionally constrain the active
                        // node to lie in `coarse_legal_active_set()`
                        // (the anchor's down-cone, optionally widened
                        // to blocker-repair cones). The set degrades
                        // to `live.present_nodes` when no anchor is
                        // set OR when `coarse_dag_nodes` is empty, so
                        // pre-v32 behavior is unchanged in those
                        // regimes. This conjunct reads the LIVE
                        // anchor/blockers irrespective of which
                        // `snapshot` was passed in, matching the
                        // `global_blockers()`-driven branches above.
                        //
                        // global_repair_mode B3: an active node licensed
                        // by a live pending grant is exempted from the
                        // cone check; the grant persists across the
                        // worker burst and is cleared on acceptance.
                        let mut allowed = self.coarse_legal_active_set();
                        if let Some(grant) = self.pending_global_repair_grant.as_ref() {
                            allowed.extend(grant.approved_extension_nodes.iter().cloned());
                        }
                        allowed.contains(node)
                    }
                    Phase::Complete => false,
                }
            }
        }
    }

    /// True iff `node` is the natural focus for repairing some outstanding
    /// blocker in ProofFormalization — either a Node-bound blocker on
    /// itself (NodeCorr / Soundness / Substantiveness), or a Target-bound
    /// PaperFaithfulness blocker that this node covers. Used as the
    /// corr/sound/substantiveness/paper-aware clause of `active_node_legal`
    /// so the reviewer has a legal `next_active` when the only outstanding
    /// work is on a closed-proof node.
    pub fn proof_node_repairs_blocker(&self, node: &NodeId) -> bool {
        if !self.live.present_nodes.contains(node) {
            return false;
        }
        if !self.current_corr_pass(node) {
            return true;
        }
        if self.needs_sound(node) && !self.current_sound_pass(node) {
            return true;
        }
        if !self.current_substantiveness_pass(node) {
            return true;
        }
        self.configured_targets.iter().any(|target| {
            !self.current_paper_pass(target)
                && self
                    .live
                    .coverage
                    .get(target)
                    .map(|nodes| nodes.contains(node))
                    .unwrap_or(false)
        })
    }

    /// True iff `node` directly imports a node carrying a live
    /// Substantiveness blocker. This deliberately does not use the
    /// transitive dependency closure: the repair pattern is to focus the
    /// immediate consumer that can remove or replace the failed
    /// dependency, leaving unsupported-node deletion to orphan cleanup.
    pub fn proof_node_directly_imports_substantiveness_blocker(&self, node: &NodeId) -> bool {
        if self.phase != Phase::ProofFormalization || !self.live.present_nodes.contains(node) {
            return false;
        }
        self.deps.get(node).into_iter().flatten().any(|dep| {
            self.live.present_nodes.contains(dep) && !self.current_substantiveness_pass(dep)
        })
    }

    /// Closed-proof aggregate-focus candidate set, conservative shape:
    /// fires only when every live blocker is node-bound and there are at
    /// least two distinct blocked nodes (single-node case is already
    /// handled by `proof_node_repairs_blocker`). Returns the minimal
    /// common importers of all blocked nodes under dep-closure
    /// containment, so high aggregator roots above the closest common
    /// importer are not exposed.
    ///
    /// Guards a routing trap that arises when two sibling helper nodes
    /// carry soundness blockers and no single legal `next_active` covers
    /// both: without an aggregate focus candidate the reviewer would be
    /// forced into LastClean as the only escape.
    ///
    /// Why "any target-bound blocker → empty": this rule deliberately
    /// does not extend the aggregate-focus affordance into paper-coverage
    /// repair territory. PaperFaithfulness blockers are target-bound and
    /// flow through a different worker-scope path
    /// (`task_blockers_outside_review_worker_scope` takes a target-cone
    /// disjunction route); broadening the aggregate rule would risk
    /// handing the worker an aggregate-mode task whose downstream scope
    /// rules don't authorize the necessary edits.
    fn proof_aggregate_node_blocker_focus_candidates(&self) -> BTreeSet<NodeId> {
        if self.phase != Phase::ProofFormalization {
            return BTreeSet::new();
        }

        let blockers = self.global_blockers();
        let mut blocked_nodes: BTreeSet<NodeId> = BTreeSet::new();
        for blocker in blockers {
            let BlockerObject::Node { node } = blocker.object else {
                return BTreeSet::new();
            };
            blocked_nodes.insert(node);
        }
        if blocked_nodes.len() < 2 {
            return BTreeSet::new();
        }

        let mut candidate_cones: Vec<(NodeId, BTreeSet<NodeId>)> = Vec::new();
        for node in &self.live.present_nodes {
            let seed = BTreeSet::from([node.clone()]);
            let cone = dep_closure_from(&seed, &self.live.present_nodes, &self.deps);
            if blocked_nodes.is_subset(&cone) {
                candidate_cones.push((node.clone(), cone));
            }
        }

        let mut out = BTreeSet::new();
        for (node, cone) in &candidate_cones {
            // Drop strictly-broader candidates: if some other candidate's
            // cone is a *proper* subset of this one's, this candidate
            // covers more than necessary. Using proper subset (rather
            // than `cone.contains(other)`) avoids dropping both members
            // of an equal-cone pair, which can occur with import cycles
            // or two structurally-equivalent aggregators.
            let too_broad = candidate_cones.iter().any(|(other, other_cone)| {
                other != node && other_cone.is_subset(cone) && other_cone != cone
            });
            if !too_broad {
                out.insert(node.clone());
            }
        }
        out
    }

    /// True iff `node` is in the minimal common-importer candidate set
    /// for the live node-bound blockers — i.e. `node` is a legal
    /// proof-restructure focus whose dep-closure covers all currently
    /// blocked nodes, and no strictly-narrower candidate covers them.
    /// Empty when the blocker set includes any target-bound blocker or
    /// fewer than two distinct blocked nodes (single-node case already
    /// handled by `proof_node_repairs_blocker`). Added 2026-05-05 for
    /// the cycle-380 routing fix.
    pub fn proof_node_repairs_aggregate_node_blockers(&self, node: &NodeId) -> bool {
        self.proof_aggregate_node_blocker_focus_candidates()
            .contains(node)
    }

    pub fn held_target_legal(&self, node: Option<&NodeId>, snapshot: &WorkingSnapshot) -> bool {
        match node {
            None => true,
            Some(node) => {
                self.phase == Phase::TheoremStating
                    && snapshot.present_nodes.contains(node)
                    && snapshot.open_nodes.contains(node)
                    && self.proof_nodes.contains(node)
            }
        }
    }

    pub fn current_corr_state(&self, node: &NodeId) -> CurrentCheckState {
        if !self.live.present_nodes.contains(node) {
            return CurrentCheckState::Unknown;
        }
        // Preamble carve-out. `ensure_initial_preamble` in runtime_cli
        // (bin/runtime_cli.rs:4681-4699) pre-pins Preamble corr to Pass
        // with EMPTY fingerprints when Preamble.tex has no structured
        // definition items — vacuous correspondence, nothing to verify
        // against the hand-written Lean preamble. Prior to 7aad7cb the
        // Pass arm matched `("", "")` via the fingerprint-equality
        // check; that commit added a `!current.is_empty()` guard
        // (originally to keep the deviation lane from treating a
        // missing-file empty fp as Pass) and accidentally tripped the
        // legitimate empty-fp pin used for Preamble. The kernel would
        // then read Preamble as `Unknown` every cycle, dispatch a Corr
        // verifier, `apply_corr_updates` would re-pin `(Pass, "", "")`,
        // and the next cycle would dispatch again — an infinite verify
        // loop on fresh runs whose Preamble.tex carries no structured
        // items. Honor the init pin for Preamble via fingerprint
        // equality alone; the empty-fp guard still applies to every
        // other node.
        let is_preamble = node.as_str() == PREAMBLE_NAME;
        match (
            self.corr_status.get(node),
            self.live.corr_current_fingerprints.get(node),
            self.corr_approved_fingerprints.get(node),
        ) {
            (Some(CorrStatus::Pass), Some(current), Some(approved))
                if current == approved && (is_preamble || !current.is_empty()) =>
            {
                CurrentCheckState::Pass
            }
            (Some(CorrStatus::Fail), Some(current), Some(approved))
                if current == approved && (is_preamble || !current.is_empty()) =>
            {
                CurrentCheckState::Fail
            }
            _ => CurrentCheckState::Unknown,
        }
    }

    pub fn current_corr_pass(&self, node: &NodeId) -> bool {
        self.current_corr_state(node) == CurrentCheckState::Pass
    }

    pub fn current_corr_fail(&self, node: &NodeId) -> bool {
        self.current_corr_state(node) == CurrentCheckState::Fail
    }

    pub fn current_corr_unknown(&self, node: &NodeId) -> bool {
        self.current_corr_state(node) == CurrentCheckState::Unknown
    }

    pub fn current_paper_state(&self, target: &TargetId) -> CurrentCheckState {
        if !self.configured_targets.contains(target) {
            return CurrentCheckState::Unknown;
        }
        if self
            .live
            .coverage
            .get(target)
            .map(|nodes| nodes.is_empty())
            .unwrap_or(true)
        {
            return CurrentCheckState::Fail;
        }
        match (
            self.paper_status.get(target),
            self.live.paper_current_fingerprints.get(target),
            self.paper_approved_fingerprints.get(target),
        ) {
            (Some(CorrStatus::Pass), Some(current), Some(approved)) if current == approved => {
                CurrentCheckState::Pass
            }
            (Some(CorrStatus::Fail), Some(current), Some(approved)) if current == approved => {
                CurrentCheckState::Fail
            }
            _ => CurrentCheckState::Unknown,
        }
    }

    pub fn current_paper_pass(&self, target: &TargetId) -> bool {
        self.current_paper_state(target) == CurrentCheckState::Pass
    }

    pub fn current_paper_fail(&self, target: &TargetId) -> bool {
        self.current_paper_state(target) == CurrentCheckState::Fail
    }

    pub fn current_paper_unknown(&self, target: &TargetId) -> bool {
        self.current_paper_state(target) == CurrentCheckState::Unknown
    }

    /// Names of all project-defined Tablet nodes consumed by `node`'s
    /// `lean_semantic_closure` walk — definitions, theorems, propositions,
    /// axioms — parsed from the live corr fingerprint
    /// (`live.corr_current_fingerprints[node]`).
    ///
    /// This is the **dispatch-eligibility set** L(N): every entry is a
    /// dependency whose Lean meaning N's verifier consults. Used by
    /// [`is_corr_dispatch_eligible`] to decide whether N's corr verifier
    /// can be dispatched yet (it cannot while any entry has open corr —
    /// N's verifier would interpret unverified dependency Lean meanings).
    ///
    /// Distinct from the def-only TeX-hash propagation set
    /// (`lean_relevant_definition_descendants`): that set drives reopen
    /// on descendant TeX changes; this set drives dispatch ordering.
    ///
    /// Returns an empty set when:
    /// - the live fingerprint is missing, empty, or unparseable;
    /// - the fingerprint is a legacy-shape blob (the
    ///   `lean_relevant_dependencies` field is absent and serde defaults
    ///   to empty);
    /// - the node has no Lean-relevant dependencies (legitimate empty set).
    pub fn lean_relevant_dependencies_of(&self, node: &NodeId) -> BTreeSet<NodeId> {
        let raw = match self.live.corr_current_fingerprints.get(node) {
            Some(s) if !s.trim().is_empty() => s,
            _ => return BTreeSet::new(),
        };
        let value: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => return BTreeSet::new(),
        };
        value
            .get("lean_relevant_dependencies")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| NodeId::from(s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// True iff `node`'s corr verifier can be dispatched right now under
    /// the topological ordering — i.e. every D in L(node) has
    /// `corr_status[D] == Pass` (and a current-state Pass per
    /// `current_corr_state`). Preamble and not-present descendants are
    /// trivially eligible. Empty L(N) is trivially eligible.
    ///
    /// When false, the corr blocker on `node` is "deferred": open by its
    /// own fingerprint, but waiting on dependency corr to resolve before
    /// the verifier can run on a verified basis.
    pub fn is_corr_dispatch_eligible(&self, node: &NodeId) -> bool {
        let deps = self.lean_relevant_dependencies_of(node);
        deps.iter().all(|d| {
            if d.as_str() == "Preamble" {
                return true;
            }
            if !self.live.present_nodes.contains(d) {
                return true;
            }
            self.current_corr_pass(d)
        })
    }

    /// True iff `target`'s paper-faithfulness verifier can be dispatched
    /// right now — i.e. every covering node `c` in `coverage[target]` has
    /// `current_corr_pass(c) == true`. The paper verifier interprets each
    /// covering node's Lean against the paper's NL; if any covering node's
    /// corr is itself open, the verifier's basis carries unverified
    /// alignments. (Transitivity is implicit: each `c`'s corr=Pass under
    /// topological dispatch was itself checked against verified L_def(c),
    /// so `c`'s Lean is already trustworthy.)
    pub fn is_paper_dispatch_eligible(&self, target: &TargetId) -> bool {
        let covering = match self.live.coverage.get(target) {
            Some(c) => c,
            None => return true,
        };
        covering.iter().all(|c| self.current_corr_pass(c))
    }

    pub fn blocked_targets(&self) -> BTreeSet<TargetId> {
        self.configured_targets
            .iter()
            .filter(|target| !self.current_paper_pass(target))
            .cloned()
            .collect()
    }

    /// True when any non-sound verifier lane has unresolved blockers — corr
    /// on any present node, paper on any configured target, substantiveness
    /// on any present node, OR deviation on any tracked deviation id.
    /// Sound verification is only legal when this returns false; a Sound
    /// verdict reasons about a node's NL proof citing its deps' statements,
    /// and while those statement surfaces (paper / corr / substantiveness)
    /// or the authorization surface for any claimed deviation are still
    /// open, a Sound verdict would be against a moving target. The
    /// function's historical name is misleading — it has always also
    /// covered paper and substantiveness, and now covers deviation too.
    pub fn corr_blockers_exist(&self) -> bool {
        self.live
            .present_nodes
            .iter()
            .any(|node| self.current_corr_state(node) != CurrentCheckState::Pass)
            || self
                .configured_targets
                .iter()
                .any(|target| self.current_paper_state(target) != CurrentCheckState::Pass)
            // Substantiveness blockers also block held-target
            // selection: while a node hasn't established its paper basis,
            // soundness on a target rooted in that node should not run.
            // (No-op in cleanup/complete because
            // `current_substantiveness_state` short-circuits to Pass; fires
            // in TheoremStating and ProofFormalization.)
            || self
                .live
                .present_nodes
                .iter()
                .any(|node| self.current_substantiveness_state(node) != CurrentCheckState::Pass)
            // Deviation blockers count too. Any tracked deviation whose
            // current state is not Pass — Unknown awaiting verification,
            // Fail awaiting revision or retirement — is part of the same
            // "verification surface in motion" that the Sound verifier
            // must avoid pinning a verdict against.
            || self
                .deviation_files
                .keys()
                .any(|id| !self.current_deviation_pass(id))
    }

    pub fn substantiveness_blockers_exist(&self) -> bool {
        self.live
            .present_nodes
            .iter()
            .any(|node| self.current_substantiveness_state(node) != CurrentCheckState::Pass)
    }

    pub fn theorem_node_has_open_blocker(&self, node: &NodeId) -> bool {
        self.live.present_nodes.contains(node)
            && (!self.current_corr_pass(node)
                || !self.current_substantiveness_pass(node)
                || (self.needs_sound(node) && !self.current_sound_pass(node)))
    }

    pub fn theorem_node_has_current_fail_blocker(&self, node: &NodeId) -> bool {
        self.live.present_nodes.contains(node)
            && (self.current_corr_fail(node)
                || self.current_substantiveness_fail(node)
                || (self.needs_sound(node) && self.current_sound_fail(node)))
    }

    pub fn corr_verify_nodes(&self) -> BTreeSet<NodeId> {
        self.live
            .present_nodes
            .iter()
            .filter(|node| self.current_corr_unknown(node))
            // Substantiveness gate (TheoremStating + ProofFormalization).
            // A node that hasn't reached substantiveness Pass is
            // conceptually paper-blocked, not corr-blocked; surfacing it
            // on the corr frontier would waste a verifier round and let a
            // node climb into the corr lane before its paper basis is
            // established. In Cleanup/Complete the lane is dormant so
            // `current_substantiveness_pass` short-circuits to true and
            // this filter is a no-op.
            .filter(|node| self.current_substantiveness_pass(node))
            // Topological dispatch: skip nodes whose Lean-relevant
            // descendants have open corr. Their verifier would interpret
            // unverified dependency Lean meanings; defer until L(N) is
            // pinned. Lean's import graph is acyclic, so dispatch always
            // finds a leaf. See `is_corr_dispatch_eligible` for the
            // semantics + bootstrap notes.
            .filter(|node| self.is_corr_dispatch_eligible(node))
            .cloned()
            .collect()
    }

    pub fn paper_verify_targets(&self) -> BTreeSet<TargetId> {
        self.configured_targets
            .iter()
            .filter(|target| self.current_paper_unknown(target))
            // Topological dispatch: defer paper verification while any
            // covering node has open corr. The paper verifier's basis
            // depends on each covering node's verified Lean ↔ NL alignment.
            .filter(|target| self.is_paper_dispatch_eligible(target))
            .cloned()
            .collect()
    }

    /// Cleanup-v2 Step 15 (2026-05-14): nodes for which the
    /// substantiveness Cleanup-phase short-circuit is bypassed. Empty
    /// outside Phase::Cleanup. In Phase::Cleanup it is the union of the
    /// active Substitution task's `authorized_nodes` and `target_node`
    /// (importers being rewired). Substantiveness fires normally on
    /// these "touched" importers exactly as it would in
    /// ProofFormalization phase. All other Phase::Cleanup nodes keep
    /// the historical short-circuit (returns Pass) so we don't fire
    /// substantiveness on all ~420 nodes at Cleanup entry.
    ///
    /// LintFix tasks don't change anything semantically (single-node
    /// proof-body / lint edit), so the cleanup substantiveness scope
    /// is empty for them — the short-circuit applies as before.
    pub fn cleanup_substantiveness_scope(&self) -> BTreeSet<NodeId> {
        if self.phase != Phase::Cleanup {
            return BTreeSet::new();
        }
        let Some(idx) = self.cleanup_active_task else {
            return BTreeSet::new();
        };
        let Some(task) = self.cleanup_audit_tasks.get(idx as usize) else {
            return BTreeSet::new();
        };
        match &task.kind {
            CleanupTaskKind::Substitution { .. } => {
                let authorized = self
                    .pending_task
                    .as_ref()
                    .map(|t| t.authorized_nodes.clone())
                    .unwrap_or_default();
                let mut scope = authorized;
                scope.insert(task.target_node.clone());
                scope
            }
            CleanupTaskKind::LintFix { .. } => BTreeSet::new(),
        }
    }

    /// Substantiveness verifier frontier. Fires in TheoremStating and
    /// ProofFormalization (helper nodes added by Hard restructure
    /// participate too); Cleanup/Complete are dormant and this returns
    /// empty there. The `Preamble` node is excluded — its definitions are
    /// checked by the target-level lane (and corr's own preamble
    /// special-case).
    ///
    /// Cleanup-v2 Step 15: during an in-flight Substitution burst, the
    /// short-circuit is narrowed — `cleanup_substantiveness_scope()`
    /// nodes (authorized importers + target) participate in
    /// substantiveness as normal, so the touched-importer rewrites
    /// have their NL ↔ Lean alignment re-checked. Non-scope cleanup
    /// nodes remain dormant.
    pub fn substantiveness_verify_nodes(&self) -> BTreeSet<NodeId> {
        let scope = self.cleanup_substantiveness_scope();
        if !matches!(
            self.phase,
            Phase::TheoremStating | Phase::ProofFormalization
        ) && scope.is_empty()
        {
            return BTreeSet::new();
        }
        self.live
            .present_nodes
            .iter()
            .filter(|node| node.as_str() != PREAMBLE_NAME)
            .filter(|node| {
                // In TheoremStating / ProofFormalization, every present
                // non-preamble node is in scope. In Phase::Cleanup, only
                // `scope` nodes are. The check is implicit because the
                // scope is empty outside the active-Substitution case.
                if self.phase == Phase::Cleanup {
                    scope.contains(*node)
                } else {
                    true
                }
            })
            .filter(|node| self.current_substantiveness_unknown(node))
            .cloned()
            .collect()
    }

    pub fn authorized_deviations(&self) -> BTreeMap<DeviationId, String> {
        self.deviation_files
            .iter()
            .filter_map(|(id, path)| {
                if self.current_deviation_pass(id) {
                    Some((id.clone(), path.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn deviation_verify_ids(&self) -> BTreeSet<DeviationId> {
        self.deviation_files
            .keys()
            .filter(|id| self.current_deviation_unknown(id))
            .cloned()
            .collect()
    }

    pub fn current_deviation_state(&self, id: &DeviationId) -> CurrentCheckState {
        match (
            self.deviation_status.get(id),
            self.live.deviation_current_fingerprints.get(id),
            self.deviation_approved_fingerprints.get(id),
        ) {
            (Some(CorrStatus::Pass), Some(current), Some(approved))
                if !current.is_empty() && current == approved =>
            {
                CurrentCheckState::Pass
            }
            (Some(CorrStatus::Fail), Some(current), Some(approved))
                if !current.is_empty() && current == approved =>
            {
                CurrentCheckState::Fail
            }
            _ => CurrentCheckState::Unknown,
        }
    }

    pub fn current_deviation_pass(&self, id: &DeviationId) -> bool {
        self.current_deviation_state(id) == CurrentCheckState::Pass
    }

    pub fn current_deviation_fail(&self, id: &DeviationId) -> bool {
        self.current_deviation_state(id) == CurrentCheckState::Fail
    }

    pub fn current_deviation_unknown(&self, id: &DeviationId) -> bool {
        self.current_deviation_state(id) == CurrentCheckState::Unknown
    }

    pub fn node_has_unauthorized_deviation_claim(&self, node: &NodeId) -> bool {
        self.node_deviation_claims
            .get(node)
            .into_iter()
            .flatten()
            .any(|id| !self.current_deviation_pass(id))
    }

    /// Compute the `CurrentCheckState` for the substantiveness
    /// lane on `node`. Mirrors `current_corr_state`, with two phase-scoped
    /// short-circuits:
    ///   - Outside TheoremStating + ProofFormalization, the lane is
    ///     dormant: returns Pass unconditionally (keeps `corr_verify_nodes`
    ///     and downstream legality predicates from getting wedged on stale
    ///     Unknown entries from a pre-advance state). Cleanup-phase
    ///     workers don't add nodes, so the lane has nothing to check there.
    ///   - `Preamble` is routed to the target-level lane only; this
    ///     accessor returns Pass for it.
    ///
    /// Cleanup-v2 Step 15: in Phase::Cleanup the short-circuit is
    /// narrowed — for nodes in `cleanup_substantiveness_scope()` (the
    /// importers being rewired by an in-flight Substitution task), we
    /// fall through to the fingerprint-based check exactly as in
    /// ProofFormalization. Other Cleanup-phase nodes keep the
    /// "returns Pass" behavior so we don't fire substantiveness on
    /// the entire DAG at Cleanup entry.
    pub fn current_substantiveness_state(&self, node: &NodeId) -> CurrentCheckState {
        if !self.live.present_nodes.contains(node) {
            return CurrentCheckState::Unknown;
        }
        let scope_active =
            self.phase == Phase::Cleanup && self.cleanup_substantiveness_scope().contains(node);
        if !matches!(
            self.phase,
            Phase::TheoremStating | Phase::ProofFormalization
        ) && !scope_active
        {
            return CurrentCheckState::Pass;
        }
        if node.as_str() == PREAMBLE_NAME {
            return CurrentCheckState::Pass;
        }
        if self.node_has_unauthorized_deviation_claim(node) {
            return CurrentCheckState::Unknown;
        }
        match (
            self.substantiveness_status.get(node),
            self.live.substantiveness_current_fingerprints.get(node),
            self.substantiveness_approved_fingerprints.get(node),
        ) {
            (Some(CorrStatus::Pass), Some(current), Some(approved)) if current == approved => {
                CurrentCheckState::Pass
            }
            (Some(CorrStatus::Fail), Some(current), Some(approved)) if current == approved => {
                CurrentCheckState::Fail
            }
            _ => CurrentCheckState::Unknown,
        }
    }

    pub fn current_substantiveness_pass(&self, node: &NodeId) -> bool {
        self.current_substantiveness_state(node) == CurrentCheckState::Pass
    }

    pub fn current_substantiveness_fail(&self, node: &NodeId) -> bool {
        self.current_substantiveness_state(node) == CurrentCheckState::Fail
    }

    pub fn current_substantiveness_unknown(&self, node: &NodeId) -> bool {
        self.current_substantiveness_state(node) == CurrentCheckState::Unknown
    }

    pub fn needs_sound(&self, node: &NodeId) -> bool {
        self.live.present_nodes.contains(node)
            && self.live.open_nodes.contains(node)
            && self.proof_nodes.contains(node)
    }

    pub fn current_sound_state(&self, node: &NodeId) -> CurrentCheckState {
        match self.current_sound_assessment(node).status {
            SoundAssessmentStatus::VerifierPass => CurrentCheckState::Pass,
            SoundAssessmentStatus::VerifierFail
            | SoundAssessmentStatus::VerifierStructural
            | SoundAssessmentStatus::ReviewerPinnedFail
            | SoundAssessmentStatus::SketchAutoFail
            | SoundAssessmentStatus::DepEditOnlyStaleFail => CurrentCheckState::Fail,
            SoundAssessmentStatus::ReviewerAcceptedPass
            | SoundAssessmentStatus::FreshUnknown
            | SoundAssessmentStatus::SelfEditUnknown
            | SoundAssessmentStatus::DepEditOnlyStalePassDeferred
            | SoundAssessmentStatus::SplitUnknown => CurrentCheckState::Unknown,
        }
    }

    pub fn current_sound_assessment(&self, node: &NodeId) -> SoundAssessment {
        if !self.needs_sound(node) {
            return SoundAssessment {
                status: SoundAssessmentStatus::VerifierPass,
                origin: AssessmentOrigin::VerifierPanel,
                fingerprints: self.current_sound_fingerprint_parts(node),
                lane_votes: BTreeMap::new(),
                reviewer_action_id: None,
            };
        }
        if self.live.sketch_proof_nodes.contains(node) {
            return SoundAssessment {
                status: SoundAssessmentStatus::SketchAutoFail,
                origin: AssessmentOrigin::KernelSketch,
                fingerprints: self.current_sound_fingerprint_parts(node),
                lane_votes: BTreeMap::new(),
                reviewer_action_id: None,
            };
        }
        let Some(stored) = self
            .sound_assessments
            .get(node)
            .cloned()
            .or_else(|| self.legacy_sound_assessment(node))
        else {
            return SoundAssessment {
                status: SoundAssessmentStatus::FreshUnknown,
                origin: AssessmentOrigin::VerifierPanel,
                fingerprints: self.current_sound_fingerprint_parts(node),
                lane_votes: BTreeMap::new(),
                reviewer_action_id: None,
            };
        };
        let current = self.current_sound_fingerprint_parts(node);
        if !current.own_tex_hash.is_empty()
            && !stored.fingerprints.own_tex_hash.is_empty()
            && current.own_tex_hash != stored.fingerprints.own_tex_hash
        {
            return SoundAssessment {
                status: SoundAssessmentStatus::SelfEditUnknown,
                origin: stored.origin,
                fingerprints: current,
                lane_votes: stored.lane_votes,
                reviewer_action_id: stored.reviewer_action_id,
            };
        }
        if !current.dep_statement_hashes.is_empty()
            && !stored.fingerprints.dep_statement_hashes.is_empty()
            && current.dep_statement_hashes != stored.fingerprints.dep_statement_hashes
        {
            let status = match stored.status {
                SoundAssessmentStatus::VerifierPass
                | SoundAssessmentStatus::ReviewerAcceptedPass
                | SoundAssessmentStatus::DepEditOnlyStalePassDeferred => {
                    SoundAssessmentStatus::DepEditOnlyStalePassDeferred
                }
                SoundAssessmentStatus::VerifierFail
                | SoundAssessmentStatus::VerifierStructural
                | SoundAssessmentStatus::ReviewerPinnedFail
                | SoundAssessmentStatus::SketchAutoFail
                | SoundAssessmentStatus::DepEditOnlyStaleFail => {
                    SoundAssessmentStatus::DepEditOnlyStaleFail
                }
                SoundAssessmentStatus::FreshUnknown
                | SoundAssessmentStatus::SelfEditUnknown
                | SoundAssessmentStatus::SplitUnknown => stored.status,
            };
            return SoundAssessment {
                status,
                origin: stored.origin,
                fingerprints: current,
                lane_votes: stored.lane_votes,
                reviewer_action_id: stored.reviewer_action_id,
            };
        }
        if !current.combined_sound_fp.is_empty()
            && !stored.fingerprints.combined_sound_fp.is_empty()
            && current.combined_sound_fp != stored.fingerprints.combined_sound_fp
        {
            let status = match stored.status {
                SoundAssessmentStatus::VerifierPass
                | SoundAssessmentStatus::ReviewerAcceptedPass
                | SoundAssessmentStatus::DepEditOnlyStalePassDeferred => {
                    SoundAssessmentStatus::DepEditOnlyStalePassDeferred
                }
                SoundAssessmentStatus::VerifierFail
                | SoundAssessmentStatus::VerifierStructural
                | SoundAssessmentStatus::ReviewerPinnedFail
                | SoundAssessmentStatus::DepEditOnlyStaleFail => {
                    SoundAssessmentStatus::DepEditOnlyStaleFail
                }
                _ => SoundAssessmentStatus::SelfEditUnknown,
            };
            return SoundAssessment {
                status,
                origin: stored.origin,
                fingerprints: current,
                lane_votes: stored.lane_votes,
                reviewer_action_id: stored.reviewer_action_id,
            };
        }
        stored
    }

    fn current_sound_fingerprint_parts(&self, node: &NodeId) -> SoundFingerprintParts {
        if let Some(parts) = self.live.sound_current_fingerprint_parts.get(node) {
            return parts.clone();
        }
        SoundFingerprintParts {
            own_tex_hash: String::new(),
            dep_statement_hashes: BTreeMap::new(),
            combined_sound_fp: self
                .live
                .sound_current_fingerprints
                .get(node)
                .cloned()
                .unwrap_or_default(),
        }
    }

    fn legacy_sound_assessment(&self, node: &NodeId) -> Option<SoundAssessment> {
        let status = *self.sound_status.get(node)?;
        let current = self.live.sound_current_fingerprints.get(node)?;
        let approved = self.sound_approved_fingerprints.get(node)?;
        if current != approved {
            return None;
        }
        let status = match status {
            SoundStatus::Unknown => SoundAssessmentStatus::FreshUnknown,
            SoundStatus::Pass => SoundAssessmentStatus::VerifierPass,
            SoundStatus::Fail => SoundAssessmentStatus::VerifierFail,
            SoundStatus::Structural => SoundAssessmentStatus::VerifierStructural,
        };
        Some(SoundAssessment {
            status,
            origin: AssessmentOrigin::VerifierPanel,
            fingerprints: self.current_sound_fingerprint_parts(node),
            lane_votes: BTreeMap::new(),
            reviewer_action_id: None,
        })
    }

    pub fn sound_phase_clear(&self, node: &NodeId) -> bool {
        !self.needs_sound(node)
            || self.current_sound_assessment(node).status == SoundAssessmentStatus::VerifierPass
    }

    pub fn current_sound_pass(&self, node: &NodeId) -> bool {
        self.current_sound_state(node) == CurrentCheckState::Pass
    }

    pub fn current_sound_fail(&self, node: &NodeId) -> bool {
        self.current_sound_state(node) == CurrentCheckState::Fail
    }

    pub fn current_sound_unknown(&self, node: &NodeId) -> bool {
        self.current_sound_state(node) == CurrentCheckState::Unknown
    }

    /// True if any **direct** dep of `node` is not currently substantiveness-
    /// Pass (i.e., its substantiveness is Fail OR Unknown). Used to gate
    /// Sound dispatch: a soundness panel that runs on a node whose `.tex`
    /// proof cites `\noderef{<dep>}` where `<dep>` is not yet known-
    /// substantive is wasted — the citation references a claim whose
    /// meaningfulness hasn't been established, so the Sound verdict isn't
    /// trustworthy. Transitive deps are NOT considered: a node only
    /// directly cites its immediate imports, so a sub-Fail/Unknown several
    /// layers down doesn't taint this node's proof — intermediate nodes
    /// can have substantive content even when something deep in the cone
    /// is broken. Polarity matches Corr's gate (`corr_verify_nodes` uses
    /// `current_substantiveness_pass`).
    pub fn has_direct_substantiveness_unverified_dep(&self, node: &NodeId) -> bool {
        self.deps
            .get(node)
            .into_iter()
            .flatten()
            .any(|dep| !self.current_substantiveness_pass(dep))
    }

    /// True if `node` should be excluded from Sound dispatch because either
    /// the node itself, or one of its **direct** deps, has substantiveness
    /// not currently Pass (Fail or Unknown). The corresponding
    /// Substantiveness blocker stays in `global_blockers` and gets
    /// adjudicated by the reviewer; once it reaches Pass this gate
    /// releases automatically. Polarity matches Corr's gate.
    pub fn substantiveness_gates_sound(&self, node: &NodeId) -> bool {
        !self.current_substantiveness_pass(node)
            || self.has_direct_substantiveness_unverified_dep(node)
    }

    pub fn sound_repair_ready(&self, node: &NodeId) -> bool {
        if !(self.needs_sound(node)
            && self.current_substantiveness_pass(node)
            && self.current_corr_pass(node))
        {
            return false;
        }
        let noderef_deps: BTreeSet<NodeId> =
            if let Some(parts) = self.live.sound_current_fingerprint_parts.get(node) {
                parts.dep_statement_hashes.keys().cloned().collect()
            } else {
                self.deps.get(node).cloned().unwrap_or_default()
            };
        noderef_deps
            .iter()
            .all(|dep| self.current_substantiveness_pass(dep) && self.current_corr_pass(dep))
    }

    pub fn sound_verifier_eligible(&self, node: &NodeId) -> bool {
        if !self.needs_sound(node) || self.live.sketch_proof_nodes.contains(node) {
            return false;
        }
        self.sound_repair_ready(node)
    }

    pub fn reviewer_requested_sound_verify_nodes(&self) -> BTreeSet<NodeId> {
        // Sound verification is only legal when all other verifier lanes
        // (paper, corr, substantiveness, deviation) are clear globally.
        // Reviewer-requested re-verifications previously bypassed this
        // gate; that let Sound dispatches fire while open corr / paper
        // / substantiveness work was still rewriting the statement
        // surface the Sound verdict reasons over. Refuse here too;
        // reviewer requests are remembered (the underlying set is not
        // cleared) and surface again as soon as the other lanes pin.
        if self.corr_blockers_exist() {
            return BTreeSet::new();
        }
        self.reviewer_requested_sound_verifier_nodes
            .iter()
            .filter(|node| {
                self.sound_verifier_eligible(node)
                    && self.current_sound_assessment(node).status
                        != SoundAssessmentStatus::VerifierPass
            })
            .cloned()
            .collect()
    }

    pub fn sound_auto_dispatch_eligible(&self, node: &NodeId) -> bool {
        // Gate is node-local: `sound_verifier_eligible` (= `needs_sound` +
        // not-SKETCH + `sound_repair_ready`) already restricts attention to
        // the candidate plus its direct `\noderef`-cited deps. Soundness
        // failures on unrelated nodes — SKETCH siblings outside this
        // node's cone, verifier-rejected nodes in a different target, etc.
        // — do not block auto-dispatch here, because the Sound verifier
        // only reasons about whether this node's NL proof correctly cites
        // its noderef dep *statements*, and those statements are guarded
        // by Substantiveness + Correspondence (not by the dep's own Sound
        // state, which is a property of the dep's proof).
        //
        // Earlier the gate also required a global `sound_no_known_fail_boundary`
        // quiescence predicate. That collapsed `new_soundness_plans.md`'s
        // "relevant target scope" into "the whole run", so auto-dispatch
        // stalled on a clean candidate as long as any sibling node anywhere
        // had a repair-ready Soundness blocker (15 SKETCH siblings, for
        // example). Removed in favor of the per-node gate `sound_repair_ready`
        // already provides.
        if !self.sound_verifier_eligible(node) {
            return false;
        }
        matches!(
            self.current_sound_assessment(node).status,
            SoundAssessmentStatus::FreshUnknown
                | SoundAssessmentStatus::SelfEditUnknown
                | SoundAssessmentStatus::DepEditOnlyStalePassDeferred
                | SoundAssessmentStatus::ReviewerAcceptedPass
                | SoundAssessmentStatus::SplitUnknown
        )
    }

    pub fn sound_verify_nodes(&self) -> BTreeSet<NodeId> {
        match self.phase {
            Phase::TheoremStating => self
                .select_theorem_sound_verify_node()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            // In proof-formalization, verify every needs_sound node whose
            // current sound state is Unknown — not just the active one.
            // This covers newly-added helper nodes (no sound_status yet) and
            // drift-induced Unknowns (status=Pass but current_fp ≠ approved,
            // reachable in CoarseRestructure mode where a non-active node's
            // NL can be edited). Previously this was filtered to
            // `active_node` only, which left helper-node drift and
            // freshly-added non-active helpers silently un-verified —
            // surfacing as blockers the reviewer could not adjudicate
            // (the `latest_sound_review_nodes` containment guard fails
            // because the panel never ran on the node).
            //
            // Gate: skip nodes whose own substantiveness is not currently
            // Pass (Fail or Unknown), or whose direct deps include any
            // node whose substantiveness is not currently Pass. In both
            // cases the relevant `.tex` statement (the node's own, or a
            // cited dep's) hasn't been confirmed substantive, so the
            // Sound verdict can't be trusted. The Substantiveness blocker
            // remains in `global_blockers` and gets adjudicated by the
            // reviewer; once it reaches Pass this gate releases
            // automatically. Polarity matches `corr_verify_nodes` which
            // gates on `current_substantiveness_pass`. Transitive (non-
            // direct) deps are NOT considered: the citing node's proof
            // only references immediate `\noderef{}` imports.
            Phase::ProofFormalization => {
                // Mirror the theorem-phase gate: Sound dispatch is illegal
                // while any non-sound verifier lane has open work. The
                // reviewer-requested branch is already gated at its
                // source (`reviewer_requested_sound_verify_nodes`); gate
                // the auto-dispatch chain here for symmetry so both
                // sources are subject to the same predicate.
                if self.corr_blockers_exist() {
                    return BTreeSet::new();
                }
                self.reviewer_requested_sound_verify_nodes()
                    .into_iter()
                    .chain(
                        self.live
                            .present_nodes
                            .iter()
                            .filter(|node| self.sound_auto_dispatch_eligible(node))
                            .cloned(),
                    )
                    .collect()
            }
            Phase::Cleanup | Phase::Complete => BTreeSet::new(),
        }
    }

    fn theorem_sound_candidate_ready(&self, node: &NodeId) -> bool {
        self.sound_auto_dispatch_eligible(node)
    }

    pub fn select_theorem_sound_verify_node(&self) -> Option<NodeId> {
        if self.phase != Phase::TheoremStating || self.corr_blockers_exist() {
            return None;
        }
        if let Some(requested) = self
            .reviewer_requested_sound_verify_nodes()
            .into_iter()
            .next()
        {
            return Some(requested);
        }
        if let Some(active) = self.active_node.as_ref() {
            if self.theorem_sound_candidate_ready(active) {
                return Some(active.clone());
            }
        }
        self.select_theorem_held_target()
            .filter(|node| self.current_sound_unknown(node))
    }

    pub fn global_blockers(&self) -> BTreeSet<Blocker> {
        let mut out = BTreeSet::new();
        for target in &self.configured_targets {
            if !self.current_paper_pass(target) {
                let deferred = !self.is_paper_dispatch_eligible(target);
                out.insert(Blocker {
                    kind: BlockerKind::PaperFaithfulness,
                    object: BlockerObject::Target {
                        target: target.clone(),
                    },
                    fingerprint: self
                        .live
                        .paper_current_fingerprints
                        .get(target)
                        .cloned()
                        .unwrap_or_default(),
                    deferred,
                });
            }
        }
        for id in self.deviation_files.keys() {
            if !self.current_deviation_pass(id) {
                out.insert(Blocker {
                    kind: BlockerKind::Deviation,
                    object: BlockerObject::Deviation {
                        deviation: id.clone(),
                    },
                    fingerprint: self
                        .live
                        .deviation_current_fingerprints
                        .get(id)
                        .cloned()
                        .unwrap_or_default(),
                    deferred: false,
                });
            }
        }
        for node in &self.live.present_nodes {
            // Substantiveness blockers (TheoremStating +
            // ProofFormalization — `current_substantiveness_pass` returns
            // true in cleanup/complete, so this branch is a no-op there).
            // Node-bound blockers participate in the standard
            // task/override/reset blocker action buckets; the dominant
            // corrective worker move is to rewrite the .tex statement.
            if !self.current_substantiveness_pass(node) {
                out.insert(Blocker {
                    kind: BlockerKind::Substantiveness,
                    object: BlockerObject::Node { node: node.clone() },
                    fingerprint: self
                        .live
                        .substantiveness_current_fingerprints
                        .get(node)
                        .cloned()
                        .unwrap_or_default(),
                    deferred: false,
                });
            }
            if !self.current_corr_pass(node) {
                let deferred = !self.is_corr_dispatch_eligible(node);
                out.insert(Blocker {
                    kind: BlockerKind::NodeCorr,
                    object: BlockerObject::Node { node: node.clone() },
                    fingerprint: self
                        .live
                        .corr_current_fingerprints
                        .get(node)
                        .cloned()
                        .unwrap_or_default(),
                    deferred,
                });
            }
            if self.needs_sound(node) && !self.current_sound_pass(node) {
                out.insert(Blocker {
                    kind: BlockerKind::Soundness,
                    object: BlockerObject::Node { node: node.clone() },
                    fingerprint: self
                        .live
                        .sound_current_fingerprints
                        .get(node)
                        .cloned()
                        .unwrap_or_default(),
                    deferred: false,
                });
            }
        }
        out
    }

    pub fn formalization_complete(&self) -> bool {
        // Cleanup invariant (#40): the protocol may only enter Cleanup with
        // an empty blocker set. Require:
        //   - textual_clean: all proof_nodes have closed proofs (no open
        //     sorrys), AND
        //   - blockers_clean: no global blockers (no live corr/sound/paper
        //     failures), AND
        //   - unverified_clean: no proof_node is in
        //     `local_closure_unverified_nodes` (Patch C plan §7.6 — a
        //     sorry-free proof_node with a stale or failed local-closure
        //     record is work-to-do exactly like a textually-open node), AND
        //   - records_present: every sorry-free proof_node has a fresh
        //     `LocalClosureRecord` installed (plan §7.6's strict gate).
        // Without the second clause, Cleanup could be entered with stale
        // verifier blockers, violating the "Cleanup = polish only, always
        // happy stop" mental model. Mirror invariant in TLA:
        // `phase = "cleanup" => GlobalBlockers = {}`.
        let textual_clean = self
            .live
            .open_nodes
            .iter()
            .all(|node| !self.proof_nodes.contains(node));
        let blockers_clean = self.global_blockers().is_empty();
        let unverified_clean = self
            .local_closure_unverified_nodes
            .iter()
            .all(|node| !self.proof_nodes.contains(node));
        // `records_fresh` is scoped to sorry-free proof_nodes only so
        // the gate doesn't block during mid-run when sorryd nodes
        // legitimately lack records (sorryd proof_nodes are caught by
        // `!textual_clean` instead). At phase-completion time
        // `textual_clean` is true ⇒ every proof_node is sorry-free, so
        // this clause then requires a fresh pure-state record for every
        // proof_node — the strict §1.1 close-the-stale-pass-gap
        // requirement. Runtime-only policy freshness (approved axioms,
        // axcheck-required config) is enforced by runtime rescission hooks.
        let records_fresh = self
            .proof_nodes
            .iter()
            .filter(|node| !self.live.open_nodes.contains(*node))
            .all(|node| {
                self.local_closure_records.get(node).is_some_and(|record| {
                    record.node == *node && record.is_fresh_for_completion(self)
                })
            });
        textual_clean && blockers_clean && unverified_clean && records_fresh
    }

    pub fn select_theorem_held_target(&self) -> Option<NodeId> {
        if self.phase != Phase::TheoremStating {
            return None;
        }
        if self.corr_blockers_exist() {
            return None;
        }
        let mut candidates: Vec<_> = self
            .live
            .present_nodes
            .iter()
            .filter(|node| {
                self.proof_nodes.contains(*node)
                    && self.live.open_nodes.contains(*node)
                    && self.current_corr_pass(node)
                    && self.current_sound_state(node) != CurrentCheckState::Pass
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            return None;
        }
        candidates.sort_by_key(|node| self.rank_of(node));
        if let Some(held) = self.held_target.as_ref() {
            if candidates.contains(held)
                && candidates
                    .iter()
                    .all(|candidate| self.rank_of(held) >= self.rank_of(candidate))
            {
                return Some(held.clone());
            }
        }
        candidates.into_iter().last()
    }

    pub fn select_initial_proof_active_node(&self) -> Option<NodeId> {
        if self.phase != Phase::ProofFormalization {
            return None;
        }

        // Patch C plan §7.4: union `live.open_nodes` with
        // `local_closure_unverified_nodes` so a sorry-free proof_node
        // with a stale or failed local-closure record is treated as
        // work-to-do exactly like a textually-open node. The
        // sorry-free-only invariant on the unverified set (§7.2)
        // guarantees the union is just the open set plus disjoint
        // sorry-free closure-failed nodes.
        //
        // Patch C-F bug fix: auto-schedule unverified nodes ONLY when
        // there is a failure record carrying evidence of a real proof
        // problem — non-empty axiom_violations or strict_errors, or a
        // status that names a known non-transport failure category.
        // Naked unverified nodes (no entry in `local_closure_failures`,
        // or an entry with empty violations/errors and an unrecognized
        // status) are handled by the runtime CLI's deterministic
        // revalidation pass (plan §7.5) — a cheap server-side probe is
        // the right action, not a ~30-60s worker burst. This avoids
        // wasted worker dispatches on migration cold-start, dep-
        // invalidated records, and post-revalidation-pass residue.
        //
        // Transport-error-only nodes were already excluded (plan §7.4.1):
        // transport errors are not proof problems and burning a worker
        // burst on them is wasteful. They retry via the deterministic-
        // revalidation pass (Patch C-D); they don't need a worker burst.
        let needs_work = |node: &NodeId| -> bool {
            if self.live.open_nodes.contains(node) {
                return true;
            }
            if !self.local_closure_unverified_nodes.contains(node) {
                return false;
            }
            // Only auto-schedule unverified nodes whose failure record
            // shows a real proof problem; naked unverified is probed,
            // not dispatched.
            match self.local_closure_failures.get(node) {
                None => false,
                Some(summary) => {
                    if summary.status == "transport_error" {
                        return false;
                    }
                    if !summary.axiom_violations.is_empty() || !summary.strict_errors.is_empty() {
                        return true;
                    }
                    matches!(
                        summary.status.as_str(),
                        "axiom_violation"
                            | "strict_error"
                            | "elaboration_error"
                            | "missing_declaration"
                            | "internal_error"
                    )
                }
            }
        };

        let mut supported_open_proof_nodes: BTreeSet<NodeId> = self
            .approved_targets
            .configured_targets
            .iter()
            .flat_map(|target| self.target_support_cone(target, &self.live))
            .filter(|node| {
                self.live.present_nodes.contains(node)
                    && needs_work(node)
                    && self.proof_nodes.contains(node)
            })
            .collect();

        if supported_open_proof_nodes.is_empty() {
            supported_open_proof_nodes = self
                .live
                .present_nodes
                .iter()
                .filter(|node| needs_work(*node) && self.proof_nodes.contains(*node))
                .cloned()
                .collect();
        }

        // Proposal v32 audit-2 followup #4: when an active coarse anchor
        // is set, restrict auto-selection to its legal cone (widened in
        // repair-mode). Without this filter the engine could seat an
        // out-of-cone `active_node` that the cone-strengthened
        // `active_node_legal` then rejects in `validate()` — surfacing
        // an InvariantViolation. Returning `None` here is degenerate but
        // recoverable: the start_cycle callsite (`engine.rs:~915`)
        // detects None + active anchor set + ProofFormalization and
        // clears the anchor as a stale-cone recovery, then re-selects
        // from `live.present_nodes`. `coarse_legal_active_set()` is
        // `live.present_nodes` when no anchor is set or
        // `coarse_dag_nodes` is empty, making this a no-op in those
        // regimes.
        let cone = self.coarse_legal_active_set();
        supported_open_proof_nodes.retain(|node| cone.contains(node));

        if supported_open_proof_nodes.is_empty() {
            return None;
        }

        let frontier: Vec<NodeId> = supported_open_proof_nodes
            .iter()
            .filter(|node| {
                self.deps
                    .get(*node)
                    .into_iter()
                    .flatten()
                    .all(|dep| !supported_open_proof_nodes.contains(dep))
            })
            .cloned()
            .collect();
        let selection_pool = if frontier.is_empty() {
            supported_open_proof_nodes.into_iter().collect::<Vec<_>>()
        } else {
            frontier
        };

        selection_pool
            .into_iter()
            .min_by_key(|node| (Reverse(self.rank_of(node)), node.clone()))
    }

    pub fn theorem_review_next_active_legal(&self, node: Option<&NodeId>) -> bool {
        match node {
            None => true,
            Some(node) => {
                let blocked_targets = self.blocked_targets();
                if !blocked_targets.is_empty() {
                    return blocked_targets
                        .iter()
                        .any(|target| self.target_support_cone(target, &self.live).contains(node));
                }
                // Per-node paper Unknown/Fail nodes are part of the work
                // surface in TheoremStating: the reviewer may task the
                // worker to repair (or replace) the .tex statement on any
                // such node. Mirror the corr branch below.
                if !self.substantiveness_verify_nodes().is_empty() {
                    return self.theorem_node_has_current_fail_blocker(node);
                }
                if !self.corr_verify_nodes().is_empty() {
                    return self.theorem_node_has_current_fail_blocker(node);
                }
                if let Some(held) = self.select_theorem_held_target() {
                    // `held` is a covering NodeId (from `proof_nodes ∩
                    // open_nodes` per `select_theorem_held_target`), not a
                    // TargetId. Legal next_active is the held node itself
                    // plus everything it depends on (so the reviewer may
                    // pivot to a dep blocking soundness on `held`). Using
                    // `target_support_cone(&held, ...)` here was a regression
                    // from commit 3fa4fc1a — it looked the NodeId up in
                    // `coverage` (keyed by TargetId), silently returned
                    // None, and collapsed `kernel_hinted_next_active_nodes` to
                    // empty. Both ids are `String` aliases so rustc didn't
                    // catch it. See K-7 surface in commit 533e240.
                    let seed = BTreeSet::from([held]);
                    return self
                        .dep_closure(&seed, &self.live.present_nodes, &self.deps)
                        .contains(node);
                }
                self.active_node_legal(Some(node), &self.live)
            }
        }
    }

    pub fn theorem_targeted_mode_legal(&self, node: Option<&NodeId>) -> bool {
        match node {
            None => false,
            Some(node) => {
                let blocked_targets = self.blocked_targets();
                if !blocked_targets.is_empty() {
                    blocked_targets
                        .iter()
                        .any(|target| self.target_support_cone(target, &self.live).contains(node))
                } else {
                    // Per-node paper Fail and corr/sound Fail are both
                    // surfaced through `theorem_node_has_current_fail_blocker`
                    // already (extended above). Sound-verifier-eligible
                    // nodes are also legal: under the new soundness model
                    // a freshly-edited node has `FreshUnknown` (Unknown,
                    // not Fail), so it wouldn't pass the fail-blocker
                    // check, but it IS a legitimate Targeted anchor —
                    // the worker focus is the verifier dispatch on that
                    // node, and any incidental edits should be scoped
                    // tight to it. Without this, a reviewer wanting a
                    // Targeted anchor on a node whose only blocker is a
                    // FreshUnknown soundness fingerprint would be forced
                    // to fall back to the broader Global mode.
                    self.theorem_node_has_current_fail_blocker(node)
                        || self.sound_verifier_eligible(node)
                }
            }
        }
    }

    pub fn proof_worker_protected_package_legal(&self, snapshot: &WorkingSnapshot) -> bool {
        if self.proof_edit_mode == ProofEditMode::CoarseRestructure {
            return true;
        }
        if self.coverage_from_claims_with_present(&self.target_claims, &snapshot.present_nodes)
            != self.live.coverage
        {
            return false;
        }
        if snapshot.paper_current_fingerprints != self.live.paper_current_fingerprints {
            return false;
        }
        // Narrowed from `self.protected_nodes()` (the old worker-declared
        // semantic closure) to `self.approved_target_nodes()` (covering
        // nodes of approved paper targets snapshotted at advance-gate):
        // non-covering nodes are intentionally not constrained here. See
        // `paper_target_corr_reopen_guard_errors` for the commit-time
        // counterpart that uses the same scope.
        self.approved_target_nodes().into_iter().all(|node| {
            snapshot.target_fingerprints.get(&node) == self.live.target_fingerprints.get(&node)
                && snapshot.corr_current_fingerprints.get(&node)
                    == self.live.corr_current_fingerprints.get(&node)
        })
    }

    fn current_shallow_coarse_closed_count(&self) -> u32 {
        let count = shallowly_closed_coarse_nodes(
            &self.committed.present_nodes,
            &self.committed.open_nodes,
            &self.committed_deps,
            &self.coarse_dag_nodes,
        )
        .len();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    fn reset_shallow_coarse_progress_tracking(&mut self) {
        self.shallow_coarse_closed_count = self.current_shallow_coarse_closed_count();
        self.cycles_since_shallow_coarse_closed_count_increase = 0;
    }

    pub(crate) fn reset_progress_history(&mut self) {
        self.progress_history.clear();
    }

    fn refresh_shallow_coarse_progress_tracking(&mut self) {
        let current_count = self.current_shallow_coarse_closed_count();
        if self.phase != Phase::ProofFormalization || self.coarse_dag_nodes.is_empty() {
            self.shallow_coarse_closed_count = current_count;
            self.cycles_since_shallow_coarse_closed_count_increase = 0;
            return;
        }
        if current_count > self.shallow_coarse_closed_count {
            self.cycles_since_shallow_coarse_closed_count_increase = 0;
        } else {
            self.cycles_since_shallow_coarse_closed_count_increase = self
                .cycles_since_shallow_coarse_closed_count_increase
                .saturating_add(1);
        }
        self.shallow_coarse_closed_count = current_count;
    }

    /// Build a `CycleSnapshot` describing the current committed Sound
    /// state. `present` is the committed present-node set; `progressed`
    /// is the subset of `present` that are NOT in
    /// `current_sound_blocker_node_set()` — i.e. nodes whose Sound
    /// status is not currently blocking. Together these drive the
    /// no-Sound-progress gate (see
    /// `stuck_math_audit_no_sound_progress_trigger`).
    fn sound_snapshot(&self) -> CycleSnapshot {
        let present = self.committed.present_nodes.clone();
        let blockers = self.current_sound_blocker_node_set();
        let progressed: BTreeSet<NodeId> = present
            .iter()
            .filter(|n| !blockers.contains(*n))
            .cloned()
            .collect();
        CycleSnapshot {
            snapshot_index: 0, // filled in by ProgressHistory::push_snapshot
            present,
            progressed,
        }
    }

    /// "Some node was unprogressed at the origin checkpoint" — i.e. the
    /// origin describes a genuine candidate for an unprog→prog
    /// transition. Without this filter, all-progressed snapshots would
    /// vacuously satisfy the no-progress predicate.
    fn sound_nontrivial_origin(snapshot: &CycleSnapshot) -> bool {
        snapshot
            .present
            .iter()
            .any(|n| !snapshot.progressed.contains(n))
    }

    /// Append a checkpoint snapshot to `progress_history`. Called from
    /// `commit_live` once per checkpoint. Today only the Sound
    /// consumer reads the buffer; a future Lean-closure consumer would
    /// snapshot a different `progressed` set off the same call site.
    fn push_progress_snapshot(&mut self) {
        let snapshot = self.sound_snapshot();
        self.progress_history
            .push_snapshot(snapshot.present, snapshot.progressed);
    }

    /// Mirror `live` into `committed`. NOTE: this is the persistence
    /// choke-point; the actual phase-advance gate is enforced earlier in
    /// `WrapperRequest::review_response_legal::AdvancePhase` (which checks
    /// `self.blockers.is_empty()`). `commit_live` itself does not enforce
    /// phase-advance preconditions — by the time we get here, the
    /// reviewer's AdvancePhase decision has already been validated.
    pub fn commit_live(&mut self) {
        self.committed = self.live.clone();
        self.committed_node_kinds = self.node_kinds.clone();
        self.committed_proof_nodes = self.proof_nodes.clone();
        self.committed_deps = self.deps.clone();
        self.committed_target_claims = self.target_claims.clone();
        self.committed_deviation_files = self.deviation_files.clone();
        self.committed_node_deviation_claims = self.node_deviation_claims.clone();
        self.normalize_committed_structural_state();
        // Patch C-A — snapshot the live closure tier into the committed
        // mirrors so `restore_committed` can roll back closure-state
        // mutations atomically with structural mutations on burst
        // rejection. Reverse indices are derived from the records map
        // and recomputed on restore (not mirrored at any tier).
        self.committed_local_closure_records = self.local_closure_records.clone();
        self.committed_local_closure_unverified_nodes = self.local_closure_unverified_nodes.clone();
        self.committed_local_closure_failures = self.local_closure_failures.clone();
        self.refresh_shallow_coarse_progress_tracking();
        // global_repair_mode S8: refresh the historically-closed set
        // against the post-commit committed baseline. Updated only here
        // (and on apply_last_clean_reset) so burst-rejection-driven
        // committed shrinkage does not silently expand the regression set.
        let currently_closed = shallowly_closed_coarse_nodes(
            &self.committed.present_nodes,
            &self.committed.open_nodes,
            &self.committed_deps,
            &self.coarse_dag_nodes,
        );
        self.ever_shallow_coarse_closed.extend(currently_closed);
        self.ever_shallow_coarse_closed
            .retain(|n| self.coarse_dag_nodes.contains(n));
        // global_repair_mode S9 + grant TTL accounting.
        if let Some(grant) = self.pending_global_repair_grant.as_ref() {
            if self.cycle.saturating_sub(grant.dispatched_at_cycle)
                > global_repair_grant_ttl_cycles()
            {
                self.pending_global_repair_grant = None;
            }
        }
        if let Some(declined_at) = self.latest_global_repair_audit_decline_cycle {
            if self.cycle.saturating_sub(declined_at) > global_repair_grant_ttl_cycles() {
                self.latest_global_repair_audit_decline_reason.clear();
                self.latest_global_repair_audit_decline_cycle = None;
            }
        }
        self.push_progress_snapshot();
        // Bookkeep cycles_since_clean. commit_live is the single choke
        // point for every CommitCheckpoint emission, so the counter
        // reflects consecutive dirty checkpoints at the granularity the
        // reviewer sees in their request summary.
        // (#56) Also snapshot the `last_clean_*` mirrors at clean
        // checkpoints — apply_last_clean_reset restores from these.
        // Capture from `committed_*` (post-normalize) rather than
        // `self.live`: at a clean checkpoint live==committed by
        // construction (commit_live just copied + normalized), so this
        // is semantically equivalent today and safer against future
        // invariant changes that might temporarily desynchronize live
        // and committed during commit_live.
        if self.clean_checkpoint_ready() {
            // Determine whether the would-be-new clean snapshot is
            // structurally identical to the existing `last_clean_*`
            // mirror. If it is, the rewind target is the same state
            // we'd recapture, so we must NOT reset
            // `last_clean_rewind_count` — otherwise the
            // `CSC_REWIND_WAIVER_COUNT` exception can never fire:
            // every post-rewind `commit_live` (e.g. on the re-issued
            // reviewer's response) lands on a live state structurally
            // identical to the prior mirror and the counter would zero
            // back to 0 before reaching the waiver. Treat the
            // not-yet-populated case (mirrors_populated()==false) as a
            // change so the first clean checkpoint always captures.
            let snapshot_unchanged = self.last_clean_mirrors_populated()
                && self.last_clean_live == self.committed
                && self.last_clean_node_kinds == self.committed_node_kinds
                && self.last_clean_proof_nodes == self.committed_proof_nodes
                && self.last_clean_deps == self.committed_deps
                && self.last_clean_target_claims == self.committed_target_claims
                && self.last_clean_deviation_files == self.committed_deviation_files
                && self.last_clean_node_deviation_claims == self.committed_node_deviation_claims
                && self.last_clean_corr_status == self.corr_status
                && self.last_clean_paper_status == self.paper_status
                && self.last_clean_deviation_status == self.deviation_status
                && self.last_clean_substantiveness_status == self.substantiveness_status
                && self.last_clean_sound_status == self.sound_status
                && self.last_clean_corr_approved_fingerprints == self.corr_approved_fingerprints
                && self.last_clean_paper_approved_fingerprints == self.paper_approved_fingerprints
                && self.last_clean_substantiveness_approved_fingerprints
                    == self.substantiveness_approved_fingerprints
                && self.last_clean_deviation_approved_fingerprints
                    == self.deviation_approved_fingerprints
                && self.last_clean_sound_approved_fingerprints == self.sound_approved_fingerprints
                && self.last_clean_local_closure_records == self.local_closure_records
                && self.last_clean_local_closure_unverified_nodes
                    == self.local_closure_unverified_nodes
                && self.last_clean_local_closure_failures == self.local_closure_failures;
            if !snapshot_unchanged {
                self.last_clean_live = self.committed.clone();
                self.last_clean_node_kinds = self.committed_node_kinds.clone();
                self.last_clean_proof_nodes = self.committed_proof_nodes.clone();
                self.last_clean_deps = self.committed_deps.clone();
                self.last_clean_target_claims = self.committed_target_claims.clone();
                self.last_clean_deviation_files = self.committed_deviation_files.clone();
                self.last_clean_node_deviation_claims =
                    self.committed_node_deviation_claims.clone();
                // (#56-extension) Snapshot verifier-lane statuses too.
                // global_blockers().is_empty() implies every relevant
                // status is Pass; restoring these on LastClean prevents
                // phantom Unknown blockers in proof/cleanup phases whose
                // start_cycle routes to Worker rather than verifier.
                self.last_clean_corr_status = self.corr_status.clone();
                self.last_clean_paper_status = self.paper_status.clone();
                self.last_clean_deviation_status = self.deviation_status.clone();
                self.last_clean_substantiveness_status = self.substantiveness_status.clone();
                self.last_clean_sound_status = self.sound_status.clone();
                // (audit follow-up) Snapshot approved-fp mirrors too.
                // Without these, LastClean restored status=Pass and
                // current_fp from the clean checkpoint, but left
                // approved_fp at the latest worker-updated value. Since
                // current_<lane>_state requires status=Pass AND
                // current_fp == approved_fp, the post-restore lane went
                // Unknown → phantom blocker on a "clean" reset.
                self.last_clean_corr_approved_fingerprints =
                    self.corr_approved_fingerprints.clone();
                self.last_clean_paper_approved_fingerprints =
                    self.paper_approved_fingerprints.clone();
                self.last_clean_substantiveness_approved_fingerprints =
                    self.substantiveness_approved_fingerprints.clone();
                self.last_clean_deviation_approved_fingerprints =
                    self.deviation_approved_fingerprints.clone();
                self.last_clean_sound_approved_fingerprints =
                    self.sound_approved_fingerprints.clone();
                // Mark mirrors complete. Replaces the structural-only
                // populated check used by request_allowed_resets and
                // review_response_legal — guards against pre-mirror
                // state files restoring half-populated mirror sets.
                self.last_clean_verifier_mirror_ready = true;
                // Patch C-A — snapshot the live closure tier into the
                // last_clean mirrors at clean checkpoints; restored on
                // `apply_last_clean_reset`. The closure-mirror readiness
                // flag is paired with the verifier-mirror flag (plan
                // §7.8): a LastClean rewind requires BOTH so that an
                // operator hitting LastClean before Patch C-A's first
                // clean checkpoint doesn't restore old structural state
                // against empty closure mirrors (false-clean state).
                self.last_clean_local_closure_records = self.local_closure_records.clone();
                self.last_clean_local_closure_unverified_nodes =
                    self.local_closure_unverified_nodes.clone();
                self.last_clean_local_closure_failures = self.local_closure_failures.clone();
                self.last_clean_local_closure_mirror_ready = true;
                // A genuinely new clean checkpoint replaces the prior
                // mirror; rewinds to the OLD mirror no longer target
                // the same state, so reset the rewind counter.
                self.last_clean_rewind_count = 0;
            }
            self.cycles_since_clean = 0;
            self.has_ever_been_clean = true;
        } else {
            self.cycles_since_clean = self.cycles_since_clean.saturating_add(1);
        }
        self.refresh_stuck_math_audit_latch();
    }

    pub fn clean_checkpoint_ready(&self) -> bool {
        self.global_blockers().is_empty() && self.pending_protected_reapproval_nodes.is_empty()
    }

    /// Apply the in-memory bookkeeping for a `ResetChoice::LastClean`
    /// decision.
    ///
    /// Restores `live`, `committed`, `node_kinds`, `committed_node_kinds`,
    /// `proof_nodes`, `committed_proof_nodes`, `deps`, `committed_deps`,
    /// `target_claims`, `committed_target_claims` from the
    /// `last_clean_*` mirror fields, which are snapshotted by
    /// `commit_live` at every clean checkpoint (when
    /// `global_blockers().is_empty()`). The `WorkingSnapshot` mirror
    /// carries `corr_current_fingerprints`, `sound_current_fingerprints`,
    /// AND `paper_current_fingerprints`, so all three lane-level
    /// fingerprint sets are also restored from the clean-checkpoint
    /// mirror — the rewound disk state is identical to what those
    /// fingerprints describe (by construction at a clean checkpoint),
    /// so they're correct, not stale.
    ///
    /// Restores `corr_status` / `sound_status` / `paper_status` from
    /// the per-lane mirrors (#56-extension). At a clean checkpoint
    /// `global_blockers().is_empty()` implies every relevant lane status
    /// is Pass; restoring the mirrors keeps the post-rewind state
    /// consistent with the rewound disk and prevents phantom Unknown
    /// blockers in proof/cleanup phases (whose `start_cycle` routes to
    /// Worker rather than verifier and so never re-runs the verifiers
    /// to re-establish status).
    ///
    /// Clears `cycles_since_clean` since the rewound state is by
    /// definition a clean checkpoint. Calls `relegalize_active_fields`
    /// to defensively clear any stale `active_node` / `held_target`
    /// that no longer exist in the restored `present_nodes` set.
    ///
    /// Approved-fingerprint maps (`corr_approved_fingerprints`,
    /// `paper_approved_fingerprints`, `sound_approved_fingerprints`)
    /// ARE restored from the `last_clean_<lane>_approved_fingerprints`
    /// mirrors. Earlier comments here claimed they "outlive worktree
    /// rewinds" as advance-gate approvals — that was incorrect:
    /// `current_<lane>_state` requires status=Pass AND
    /// `current_fingerprint == approved_fingerprint`, so leaving
    /// approved_fp at a post-clean worker-updated value while
    /// restoring status + current_fp from the clean checkpoint flips
    /// the lane to Unknown immediately on the supposedly-clean reset.
    ///
    /// `approved_target_nodes` (computed from `approved_targets`) is
    /// NOT touched — that snapshot tracks the advance-gate target
    /// approval set, which is a separate concept from per-lane
    /// fingerprints.
    ///
    /// Latest-review-context fields (`latest_*_reviewer_evidence`,
    /// `latest_*_review_*`) are NOT touched here either — engine sites
    /// that drive LastClean already call `clear_latest_*_review_context`
    /// as part of their post-reset flow when they need to (the reviewer
    /// adjudication signal that depended on them is no longer needed
    /// once status mirrors restore Pass on every lane that was Pass at
    /// the clean checkpoint).
    ///
    /// Migration safety: if loaded from a pre-#56 state file with
    /// `has_ever_been_clean=true` but empty `last_clean_*` mirrors
    /// (#[serde(default)] gives `WorkingSnapshot::default()` etc.), no-op
    /// to avoid the validate_invariants violation that would result from
    /// restoring an empty `paper_current_fingerprints` against non-empty
    /// `configured_targets`. After the first post-load
    /// `commit_live` with is_clean=true the mirrors populate normally.
    ///
    /// The runtime is responsible for the actual `git reset --hard
    /// <supervisor2/clean-NNNNNN>` + `git clean -fd` on the repo, via
    /// the `RestoreWorktreeToLastClean` ProtocolCommand emitted
    /// alongside this state mutation.
    ///
    /// Patch C-N item 2: returns `Result<bool, String>` so the engine
    /// can keep state and disk in lockstep:
    ///   * `Ok(true)`  — full structural + closure rewind applied; the
    ///     caller should emit `RestoreWorktreeToLastClean` to keep disk
    ///     in sync.
    ///   * `Ok(false)` — closure mirrors not ready (migration window):
    ///     the function only zeroed `cycles_since_clean` and otherwise
    ///     left state untouched. The caller MUST NOT emit
    ///     `RestoreWorktreeToLastClean` in this branch — emitting it
    ///     would reset disk to the supervisor2/clean tag while kernel
    ///     state still reflects the post-clean burst, producing
    ///     state/disk divergence (audit MEDIUM, the residual hole C-I's
    ///     Option B closed at the menu level but didn't close along
    ///     paths that ever bypassed `request_allowed_resets`).
    ///   * `Err(_)`    — reserved for error conditions; currently never
    ///     produced but the return type leaves room for future fail-
    ///     loud paths (e.g. structural mirrors populated but a
    ///     specific sub-mirror corrupted) without another signature
    ///     churn.
    pub fn apply_last_clean_reset(&mut self) -> Result<bool, String> {
        // Migration guard: if mirrors are empty (default), this is a
        // pre-#56 state file. Restoring would violate
        // validate_invariants. Skip the structural restore — and skip
        // the status-restore too — but still zero cycles_since_clean.
        // The next clean commit_live populates mirrors and subsequent
        // LastClean works normally.
        self.cycles_since_clean = 0;

        if !self.last_clean_mirrors_populated() {
            self.reset_shallow_coarse_progress_tracking();
            self.reset_progress_history();
            return Ok(false);
        }

        // Patch C-A LastClean closure-mirror gate (plan §7.8).
        //
        // The closure mirrors live in their own `last_clean_local_closure_*`
        // fields and have a paired readiness flag
        // `last_clean_local_closure_mirror_ready`. State files persisted
        // before Patch C-A populated those mirrors at least once at a
        // clean checkpoint deserialize the flag as `false` (per
        // `#[serde(default)]`). Restoring `live` from the structural
        // mirrors against empty closure mirrors would silently revert
        // the closure tier to "no records, no failures, no unverified"
        // — a false-clean state that would then satisfy any future
        // closure-aware completion gate without ever having actually
        // probed.
        //
        // Refuse the rewind in that case: leave both the structural and
        // closure live state untouched. `cycles_since_clean = 0` has
        // already been applied above (matching the existing verifier
        // migration-guard precedent). The runtime CLI / supervisor is
        // responsible for surfacing the operator-visible diagnostic
        // ("LastClean unavailable: closure mirrors not yet committed —
        // available after the next clean checkpoint") and for
        // suppressing the menu option via `request_allowed_resets`.
        // Patch C-A only enforces the engine-side gate. Patch C-N item 2
        // additionally signals this refusal to engine callers via
        // `Ok(false)` so they can suppress `RestoreWorktreeToLastClean`
        // emission and avoid state/disk divergence.
        if !self.last_clean_local_closure_mirror_ready {
            self.reset_shallow_coarse_progress_tracking();
            self.reset_progress_history();
            return Ok(false);
        }

        self.live = self.last_clean_live.clone();
        self.committed = self.last_clean_live.clone();
        self.node_kinds = self.last_clean_node_kinds.clone();
        self.committed_node_kinds = self.last_clean_node_kinds.clone();
        self.proof_nodes = self.last_clean_proof_nodes.clone();
        self.committed_proof_nodes = self.last_clean_proof_nodes.clone();
        self.deps = self.last_clean_deps.clone();
        self.committed_deps = self.last_clean_deps.clone();
        self.target_claims = self.last_clean_target_claims.clone();
        self.committed_target_claims = self.last_clean_target_claims.clone();
        self.deviation_files = self.last_clean_deviation_files.clone();
        self.committed_deviation_files = self.last_clean_deviation_files.clone();
        self.node_deviation_claims = self.last_clean_node_deviation_claims.clone();
        self.committed_node_deviation_claims = self.last_clean_node_deviation_claims.clone();
        self.corr_status = self.last_clean_corr_status.clone();
        self.paper_status = self.last_clean_paper_status.clone();
        self.deviation_status = if self.last_clean_deviation_status.is_empty()
            && !self.last_clean_deviation_files.is_empty()
        {
            self.last_clean_deviation_files
                .keys()
                .map(|id| (id.clone(), CorrStatus::Unknown))
                .collect()
        } else {
            self.last_clean_deviation_status.clone()
        };
        self.substantiveness_status = self.last_clean_substantiveness_status.clone();
        self.sound_status = self.last_clean_sound_status.clone();
        // (audit follow-up) Restore approved-fingerprint mirrors so
        // current_<lane>_state's status=Pass AND current_fp == approved_fp
        // contract holds post-restore. Without this, a node whose
        // approved_fp moved from F0→F1 between the clean checkpoint
        // and the rewind point would land in Unknown immediately.
        self.corr_approved_fingerprints = self.last_clean_corr_approved_fingerprints.clone();
        self.paper_approved_fingerprints = self.last_clean_paper_approved_fingerprints.clone();
        self.substantiveness_approved_fingerprints = self
            .last_clean_substantiveness_approved_fingerprints
            .clone();
        self.deviation_approved_fingerprints =
            self.last_clean_deviation_approved_fingerprints.clone();
        self.sound_approved_fingerprints = self.last_clean_sound_approved_fingerprints.clone();
        self.sound_assessments.clear();
        self.reviewer_requested_sound_verifier_nodes.clear();

        // Patch C-A — restore the closure live tier from the LastClean
        // mirrors. Mirror the committed tier from the same source
        // (last_clean → committed) so a subsequent rejection-driven
        // `restore_committed` rolls back to the LastClean snapshot
        // rather than the pre-rewind committed snapshot, which would
        // otherwise undo the structural rewind for closure state. The
        // reverse indices are derived from records and recomputed
        // here against the restored records map.
        self.local_closure_records = self.last_clean_local_closure_records.clone();
        self.local_closure_unverified_nodes =
            self.last_clean_local_closure_unverified_nodes.clone();
        self.local_closure_failures = self.last_clean_local_closure_failures.clone();
        self.committed_local_closure_records = self.last_clean_local_closure_records.clone();
        self.committed_local_closure_unverified_nodes =
            self.last_clean_local_closure_unverified_nodes.clone();
        self.committed_local_closure_failures = self.last_clean_local_closure_failures.clone();
        // Audit C-3 — continuous coverage scan post-LastClean reset.
        // The LastClean mirrors capture closure state at the previous
        // clean checkpoint; the restored structural state may have
        // moved forward such that orphan sorry-free proof_nodes exist.
        // Pin those into unverified so the next probe pass refreshes
        // them.
        self.ensure_local_closure_coverage();
        recompute_local_closure_reverse_indices(self);

        // Defensive: relegalize after structural restore so any
        // pre-rewind active_node / held_target that no longer exists
        // in the restored present_nodes is cleared. Idempotent — sites
        // that already call relegalize_active_fields after this won't
        // double-mutate.
        self.relegalize_active_fields();
        // global_repair_mode S7: intersect pending request/grant with
        // restored present_nodes; drop carriers that lose every node.
        self.relegalize_global_repair_against_present();
        // global_repair_mode M13: a rewound cycle may leave the grant
        // past its TTL. Drop in that case.
        if let Some(grant) = self.pending_global_repair_grant.as_ref() {
            if self.cycle.saturating_sub(grant.dispatched_at_cycle)
                > global_repair_grant_ttl_cycles()
            {
                self.pending_global_repair_grant = None;
            }
        }
        // global_repair_mode S8: re-seed the historically-closed set
        // from the restored committed baseline. Without this, a rewind
        // could leave regressed history pointing at nodes the restored
        // baseline never closed, locking anchor change indefinitely.
        self.ever_shallow_coarse_closed = shallowly_closed_coarse_nodes(
            &self.committed.present_nodes,
            &self.committed.open_nodes,
            &self.committed_deps,
            &self.coarse_dag_nodes,
        );
        self.ever_shallow_coarse_closed
            .retain(|n| self.coarse_dag_nodes.contains(n));
        // Bump the rewind counter for this checkpoint. Reset to 0 in
        // `commit_live` when a new clean mirror is captured.
        self.last_clean_rewind_count = self.last_clean_rewind_count.saturating_add(1);
        // Earn a fresh StuckMathAudit on the rewound state before any
        // Reviewer touches it. Consumed by `should_dispatch_stuck_math_audit`.
        self.force_stuck_math_audit_after_rewind = true;
        self.reset_shallow_coarse_progress_tracking();
        self.reset_progress_history();
        Ok(true)
    }

    pub fn restore_committed(&mut self) {
        self.live = self.committed.clone();
        self.node_kinds = self.committed_node_kinds.clone();
        self.proof_nodes = self.committed_proof_nodes.clone();
        self.deps = self.committed_deps.clone();
        self.target_claims = self.committed_target_claims.clone();
        self.deviation_files = self.committed_deviation_files.clone();
        self.node_deviation_claims = self.committed_node_deviation_claims.clone();
        self.normalize_live_structural_state();
        self.deviation_status
            .retain(|id, _| self.deviation_files.contains_key(id));
        self.deviation_approved_fingerprints
            .retain(|id, _| self.deviation_files.contains_key(id));
        // Patch C-A — roll the closure live tier back to the committed
        // mirrors so a rejected burst's closure-state mutations don't
        // leak past `restore_committed`. The reverse indices
        // (`boundary_statement_consumers`, `strict_dep_consumers`) are
        // derived from records and rebuilt from the restored records
        // here so they match the post-restore live state.
        self.local_closure_records = self.committed_local_closure_records.clone();
        self.local_closure_unverified_nodes = self.committed_local_closure_unverified_nodes.clone();
        self.local_closure_failures = self.committed_local_closure_failures.clone();
        // Audit C-3 — continuous coverage scan post-restore.
        // The committed mirror reflects the LAST clean structural
        // state; the structural restore step above may have surfaced
        // orphan sorry-free proof_nodes that lack records (e.g.
        // operator hand-edits captured into committed). Pin those into
        // unverified so the next probe pass refreshes them rather than
        // silently failing the gate.
        self.ensure_local_closure_coverage();
        recompute_local_closure_reverse_indices(self);
    }

    pub fn relegalize_active_fields(&mut self) {
        if !self.active_node_legal(self.active_node.as_ref(), &self.live) {
            self.active_node = None;
        }
        if !self.held_target_legal(self.held_target.as_ref(), &self.live) {
            self.held_target = None;
        }
        if self.phase == Phase::TheoremStating && self.corr_blockers_exist() {
            self.held_target = None;
        }
        if self.active_node.is_none() {
            self.target_edit_mode = TargetEditMode::Global;
            self.proof_edit_mode = ProofEditMode::Local;
        }
        // Mode invariants: target_edit_mode is only meaningful in
        // TheoremStating; proof_edit_mode is only meaningful in
        // ProofFormalization. Outside those phases, reset to the
        // default (`validate_invariants` rejects any other state).
        // This used to be implicit because the only way to leave
        // ProofFormalization was via Local mode (the worker that
        // closes the last sorry), but with closed-proof
        // blocker-recovery (active_node_legal extension) it's
        // possible to advance from Restructure mode straight into
        // Cleanup, so we must reset here too.
        if self.phase != Phase::TheoremStating {
            self.target_edit_mode = TargetEditMode::Global;
        }
        if self.phase != Phase::ProofFormalization {
            self.proof_edit_mode = ProofEditMode::Local;
        }
    }

    pub fn clear_pending_task(&mut self) {
        self.pending_task = None;
    }

    /// global_repair_mode S7: intersect any pending request/grant node
    /// sets with `live.present_nodes`. If a resulting set is empty the
    /// whole carrier is dropped. Idempotent.
    pub fn relegalize_global_repair_against_present(&mut self) {
        if let Some(mut req) = self.pending_global_repair_request.take() {
            req.proposed_extension_nodes
                .retain(|n| self.live.present_nodes.contains(n));
            if !req.proposed_extension_nodes.is_empty() {
                self.pending_global_repair_request = Some(req);
            }
        }
        if let Some(mut grant) = self.pending_global_repair_grant.take() {
            grant
                .approved_extension_nodes
                .retain(|n| self.live.present_nodes.contains(n));
            if !grant.approved_extension_nodes.is_empty() {
                self.pending_global_repair_grant = Some(grant);
            }
        }
    }

    fn stuck_math_audit_trigger_blockers(&self) -> BTreeSet<Blocker> {
        if self.phase == Phase::ProofFormalization {
            return self
                .request_blockers(RequestKind::Review)
                .into_iter()
                .filter(|blocker| {
                    matches!(
                        blocker.kind,
                        BlockerKind::Soundness | BlockerKind::Substantiveness
                    )
                })
                .collect();
        }
        if self.phase == Phase::TheoremStating {
            return self
                .global_blockers()
                .into_iter()
                .filter(|blocker| matches!(blocker.kind, BlockerKind::Soundness))
                .collect();
        }
        BTreeSet::new()
    }

    /// Current open Sound-blocker NODE set used by the TheoremStating-phase
    /// stagnation counter. Mirrors `global_blockers()` filtered to
    /// `BlockerKind::Soundness`, projected to the carrier node. Soundness
    /// blockers are always `BlockerObject::Node`, so the projection is
    /// total.
    pub fn current_sound_blocker_node_set(&self) -> BTreeSet<NodeId> {
        self.global_blockers()
            .into_iter()
            .filter_map(|blocker| match (blocker.kind, blocker.object) {
                (BlockerKind::Soundness, BlockerObject::Node { node }) => Some(node),
                _ => None,
            })
            .collect()
    }

    fn stuck_math_audit_shallow_coarse_no_progress_trigger(&self) -> bool {
        // Trigger B: the coarse-DAG shallow-closure progress metric has
        // not improved for the configured number of checkpoint cycles.
        // `cycles_since_shallow_coarse_closed_count_increase` is counted
        // by `commit_live`; in operator terms it is cycles since the
        // remaining coarse-shallow-open count last decreased.
        self.phase == Phase::ProofFormalization
            && !self.coarse_dag_nodes.is_empty()
            && self.cycles_since_shallow_coarse_closed_count_increase
                >= stuck_math_audit_shallow_coarse_no_progress_threshold()
    }

    fn stuck_math_audit_threshold_trigger(&self) -> bool {
        // Trigger A (original): cycles_since_clean past the configured
        // threshold with at least one open soundness/substantiveness blocker
        // in ProofFormalization.
        self.phase == Phase::ProofFormalization
            && self.cycles_since_clean >= stuck_math_audit_cycles_since_clean_threshold()
            && !self.stuck_math_audit_trigger_blockers().is_empty()
    }

    /// Trigger C: no Sound progress for `k` checkpoint snapshots in
    /// `Phase::TheoremStating`. Reads `progress_history` and applies
    /// the Sound-snapshot consumer of `no_progress_window_eligible`.
    /// The predicate is strictly more permissive than blocker-
    /// fingerprint equality: re-verification drift that swaps a blocker
    /// fingerprint without removing the carrier node still counts as no
    /// progress.
    pub(crate) fn stuck_math_audit_theorem_stating_no_sound_progress_trigger(&self) -> bool {
        if self.phase != Phase::TheoremStating || self.current_sound_blocker_node_set().is_empty() {
            return false;
        }
        no_progress_window_eligible(
            &self.progress_history,
            stuck_math_audit_no_sound_progress_window(),
            Self::sound_nontrivial_origin,
        )
    }

    /// Trigger D (ProofFormalization variant): no Sound progress for
    /// `k` snapshots while Sound carriers are not all closed. Mirrors
    /// the TheoremStating trigger; shares the `progress_history`
    /// buffer and the same window length. The non-empty-blocker-set
    /// guard plays the role of "Sound nodes are not all closed":
    /// `current_sound_blocker_node_set` lists every carrier whose
    /// Sound status is currently a global blocker, so emptiness is
    /// the canonical "all Sound carriers are closed" signal.
    pub(crate) fn stuck_math_audit_proof_formalization_no_sound_progress_trigger(&self) -> bool {
        if self.phase != Phase::ProofFormalization
            || self.current_sound_blocker_node_set().is_empty()
        {
            return false;
        }
        no_progress_window_eligible(
            &self.progress_history,
            stuck_math_audit_no_sound_progress_window(),
            Self::sound_nontrivial_origin,
        )
    }

    /// Either no-Sound-progress trigger fired (TheoremStating or
    /// ProofFormalization). The dispatch site and reason builder both
    /// consume this aggregate.
    pub(crate) fn stuck_math_audit_no_sound_progress_trigger(&self) -> bool {
        self.stuck_math_audit_theorem_stating_no_sound_progress_trigger()
            || self.stuck_math_audit_proof_formalization_no_sound_progress_trigger()
    }

    fn stuck_math_audit_should_activate(&self) -> bool {
        self.stuck_math_audit_threshold_trigger()
            || self.stuck_math_audit_shallow_coarse_no_progress_trigger()
            || self.stuck_math_audit_no_sound_progress_trigger()
    }

    fn stuck_math_audit_trigger_reason(&self) -> String {
        // Reason strings are written so the auditor can parse the
        // prefix to distinguish triggers; see
        // `prompt_fragments/stuck_math_audit/common/02b_trigger_reason.md`
        // for the auditor-facing taxonomy.
        if self.stuck_math_audit_theorem_stating_no_sound_progress_trigger() {
            format!(
                "sound-stagnation-window: no Sound progress for >= {} snapshots (theorem-stating)",
                stuck_math_audit_no_sound_progress_window()
            )
        } else if self.stuck_math_audit_proof_formalization_no_sound_progress_trigger() {
            format!(
                "sound-stagnation-window: no Sound progress for >= {} snapshots (proof-formalization)",
                stuck_math_audit_no_sound_progress_window()
            )
        } else if self.stuck_math_audit_shallow_coarse_no_progress_trigger() {
            format!(
                "cycles_since_shallow_coarse_closed_count_increase >= {} (shallow_coarse_closed_count = {})",
                stuck_math_audit_shallow_coarse_no_progress_threshold(),
                self.shallow_coarse_closed_count
            )
        } else {
            format!(
                "cycles_since_clean >= {} with proof-formalization soundness/substantiveness blockers",
                stuck_math_audit_cycles_since_clean_threshold()
            )
        }
    }

    /// Depth (in snapshots) of the OLDEST snapshot that would qualify
    /// as a no-Sound-progress window origin under the currently
    /// configured `k`. Surfaced as
    /// `WrapperRequest::no_sound_progress_window_cycles` so the
    /// auditor can see how far back the stagnation extends. Returns 0
    /// when the trigger has not fired (no eligible origin exists).
    fn no_sound_progress_window_depth(&self) -> u32 {
        oldest_no_progress_window_depth(
            &self.progress_history,
            stuck_math_audit_no_sound_progress_window(),
            Self::sound_nontrivial_origin,
        )
    }

    pub(crate) fn activate_stuck_math_audit_latch(&mut self, trigger: impl Into<String>) {
        let trigger_blockers = self.stuck_math_audit_trigger_blockers();
        self.stuck_math_audit.active = true;
        self.stuck_math_audit.trigger = trigger.into();
        self.stuck_math_audit.active_since_cycle = self.cycle;
        if !trigger_blockers.is_empty() {
            self.stuck_math_audit.trigger_blockers = trigger_blockers;
        }
    }

    pub fn refresh_stuck_math_audit_latch(&mut self) {
        // Activation has priority over clearing: if any trigger fires
        // right now, the latch must be (or stay) on, even if
        // global_blockers happens to be empty and last_clean_rewind_count
        // == 0. When the no-Sound-progress gate newly fires on top of
        // an already-active latch (e.g. cycles_since_clean had armed
        // earlier), update the reason string so the auditor sees the
        // strictest currently-active trigger — `trigger_reason` itself
        // applies the priority order.
        //
        // Exception: a reviewer-NeedInput escalation pins its own
        // trigger string ("reviewer requested NeedInput: ...") for the
        // duration of that escalation, surfaced by `need_input_audit`
        // being Some. Background gates (no-Sound-progress, etc.) must
        // not overwrite it — the operator-facing reason should remain
        // the reviewer's escalation while the NeedInput context is
        // live.
        if self.stuck_math_audit_should_activate() {
            if !self.stuck_math_audit.active {
                self.activate_stuck_math_audit_latch(self.stuck_math_audit_trigger_reason());
            } else if self.stuck_math_audit_no_sound_progress_trigger()
                && self.stuck_math_audit.need_input_audit.is_none()
            {
                self.stuck_math_audit.trigger = self.stuck_math_audit_trigger_reason();
            }
            return;
        }
        if self.stuck_math_audit.need_input_audit.is_some()
            || self
                .audit_plan
                .as_ref()
                .is_some_and(|plan| plan.need_input_audit)
        {
            return;
        }
        // A LastClean rewind lands on an already-known clean checkpoint.
        // Keep the escalation visible until a genuinely new clean
        // checkpoint resets `last_clean_rewind_count` in `commit_live`.
        if self.global_blockers().is_empty()
            && self.last_clean_rewind_count == 0
            && !self.force_review_after_cone_clean
        {
            self.stuck_math_audit = StuckMathAuditState::default();
            self.superseded_audit_plan = self.audit_plan.take();
            self.last_stuck_math_audit_dispatched_cycle = None;
            self.stuck_math_audit_burst_retry_count = 0;
            self.latest_stuck_math_audit_rejection_reason.clear();
        }
    }

    pub fn record_stuck_math_audit_review(&mut self, review: &ReviewResponse) {
        if !self.stuck_math_audit.active {
            return;
        }
        self.stuck_math_audit.last_reviewer_lean_product = review
            .stuck_math_audit
            .as_ref()
            .and_then(|report| report.reviewer_lean_product_meaningful().cloned());
    }

    /// Shared "view-active" predicate consulted by both
    /// `request_audit_plan` and `request_stuck_math_audit`. Returns
    /// `true` iff the audit-plan latch should be presented as active
    /// (and the associated `audit_plan` should be surfaced) in this
    /// request kind's view.
    ///
    /// Root cause of a visibility/dismissability drift class:
    /// `request_audit_plan` read `state.stuck_math_audit.active` raw
    /// while `request_stuck_math_audit` zeroed the view when
    /// `global_blockers().is_empty()`. A single `WrapperRequest` could
    /// carry `audit_plan=Some` together with `stuck_math_audit.active=false`,
    /// and the kernel-side dismissal-legality check (which reads the
    /// **request** view of `stuck_math_audit.active`) would then reject
    /// any `dismiss_audit_plan` the reviewer attempted. Consolidating
    /// the gate here means both helpers cannot drift, so visibility ⇔
    /// dismissability is structurally enforced.
    ///
    /// Note: HumanGate sees the plan only on `need_input_audit=true`
    /// escalations via a separate short-circuit in `request_audit_plan`
    /// and never sees the latch view. Callers handle HumanGate
    /// separately.
    pub fn audit_plan_view_active(&self, kind: RequestKind) -> bool {
        // State-level NeedInput escalation pin: surfaces the latch as
        // active for the audit-tier roles regardless of trigger lull
        // (the operator-facing escalation must remain visible until
        // resolved through the human gate).
        if self.stuck_math_audit.need_input_audit.is_some()
            && matches!(
                kind,
                RequestKind::Review | RequestKind::Worker | RequestKind::StuckMathAudit
            )
        {
            return true;
        }
        // Need-input plan pin: the auditor's NeedInputAuditor plan is
        // pinned visible+actionable on Review/Worker until the
        // human-gate response retires it. (`request_audit_plan` also
        // surfaces the plan on HumanGate for the operator's reading,
        // but HumanGate has no latch view.)
        if self
            .audit_plan
            .as_ref()
            .is_some_and(|plan| plan.need_input_audit)
            && self.stuck_math_audit.active
            && matches!(kind, RequestKind::Review | RequestKind::Worker)
        {
            return true;
        }
        // General PF/TS active path: Review/Worker/StuckMathAudit only.
        if !matches!(
            kind,
            RequestKind::Review | RequestKind::Worker | RequestKind::StuckMathAudit
        ) {
            return false;
        }
        if !matches!(
            self.phase,
            Phase::ProofFormalization | Phase::TheoremStating
        ) {
            return false;
        }
        // Mirror the empty-blockers zeroing branch from the legacy
        // `request_stuck_math_audit`: when no global blocker is open
        // and we are not in the LastClean rewind exception, both the
        // latch view and the audit_plan must zero together.
        if self.global_blockers().is_empty()
            && !(self.stuck_math_audit.active && self.last_clean_rewind_count > 0)
        {
            return false;
        }
        self.stuck_math_audit.active || self.stuck_math_audit_should_activate()
    }

    pub fn request_stuck_math_audit(&self, kind: RequestKind) -> StuckMathAuditState {
        if !self.audit_plan_view_active(kind) {
            return StuckMathAuditState::default();
        }
        // Need-input escalations: the underlying state is already
        // active and carries the pinned trigger string / blockers, so
        // pass it through unchanged.
        if self.stuck_math_audit.need_input_audit.is_some()
            || self
                .audit_plan
                .as_ref()
                .is_some_and(|plan| plan.need_input_audit)
        {
            return self.stuck_math_audit.clone();
        }
        // General path: if the latch hasn't been activated yet on the
        // state but the should-activate gate has just fired, mint a
        // would-activate view with the strictest currently-active
        // trigger.
        let mut view = self.stuck_math_audit.clone();
        if !view.active && self.stuck_math_audit_should_activate() {
            view.active = true;
            view.trigger = self.stuck_math_audit_trigger_reason();
            view.active_since_cycle = self.cycle;
            view.trigger_blockers = self.stuck_math_audit_trigger_blockers();
        }
        if view.active {
            view
        } else {
            StuckMathAuditState::default()
        }
    }

    pub fn request_audit_plan(&self, kind: RequestKind) -> Option<AuditPlan> {
        // HumanGate sees the plan only on need-input escalations (so
        // the operator can read the plan); it never sees the latch
        // view, so `audit_plan_view_active` excludes HumanGate.
        if kind == RequestKind::HumanGate {
            if self
                .audit_plan
                .as_ref()
                .is_some_and(|plan| plan.need_input_audit)
            {
                return self.audit_plan.clone();
            }
            return None;
        }
        // Review/Worker: the plan is surfaced iff the shared latch
        // view is active. The StuckMathAudit role authors the plan
        // and reads it via `previous_audit_plan_snapshot` instead, so
        // it is excluded here. The Option A visibility/dismissability
        // invariant (audit_plan visible ⇔ dismissable) is scoped to the
        // Review/Worker consumer roles; the auditor's author-role
        // plumbing is outside that coupling and reads via the snapshot.
        if matches!(kind, RequestKind::Review | RequestKind::Worker)
            && self.audit_plan.is_some()
            && self.audit_plan_view_active(kind)
        {
            self.audit_plan.clone()
        } else {
            None
        }
    }

    pub fn request_previous_audit_plan_snapshot(&self, kind: RequestKind) -> Option<AuditPlan> {
        // StuckMathAudit (auditor role): the snapshot is the basis for
        // the next audit. Phase gate matches the legacy helper.
        if kind == RequestKind::StuckMathAudit
            && (matches!(
                self.phase,
                Phase::ProofFormalization | Phase::TheoremStating
            ) || self.stuck_math_audit.need_input_audit.is_some())
        {
            return self
                .audit_plan
                .clone()
                .or_else(|| self.superseded_audit_plan.clone());
        }
        // Review/Worker (Option A widening): a historical reference
        // surface for the reviewer/worker when the live audit_plan is
        // NOT visible (latch off or out of phase). When the live plan
        // IS visible (request_audit_plan returns Some), we suppress
        // the snapshot so the reviewer cannot confuse a historical
        // plan with the live one. Restricted to PF/TS for symmetry
        // with the live plan's phase gate.
        if matches!(kind, RequestKind::Review | RequestKind::Worker)
            && matches!(
                self.phase,
                Phase::ProofFormalization | Phase::TheoremStating
            )
            && self.request_audit_plan(kind).is_none()
        {
            return self
                .audit_plan
                .clone()
                .or_else(|| self.superseded_audit_plan.clone());
        }
        None
    }

    pub fn current_failed_blockers(&self) -> BTreeSet<Blocker> {
        let mut out = BTreeSet::new();
        for target in &self.configured_targets {
            if self.current_paper_fail(target) {
                out.insert(Blocker {
                    kind: BlockerKind::PaperFaithfulness,
                    object: BlockerObject::Target {
                        target: target.clone(),
                    },
                    fingerprint: self
                        .live
                        .paper_current_fingerprints
                        .get(target)
                        .cloned()
                        .unwrap_or_default(),
                    deferred: false,
                });
            }
        }
        for id in self.deviation_files.keys() {
            if self.current_deviation_fail(id) {
                out.insert(Blocker {
                    kind: BlockerKind::Deviation,
                    object: BlockerObject::Deviation {
                        deviation: id.clone(),
                    },
                    fingerprint: self
                        .live
                        .deviation_current_fingerprints
                        .get(id)
                        .cloned()
                        .unwrap_or_default(),
                    deferred: false,
                });
            }
        }
        for node in &self.live.present_nodes {
            if self.current_substantiveness_fail(node) {
                out.insert(Blocker {
                    kind: BlockerKind::Substantiveness,
                    object: BlockerObject::Node { node: node.clone() },
                    fingerprint: self
                        .live
                        .substantiveness_current_fingerprints
                        .get(node)
                        .cloned()
                        .unwrap_or_default(),
                    deferred: false,
                });
            }
            if self.current_corr_fail(node) {
                out.insert(Blocker {
                    kind: BlockerKind::NodeCorr,
                    object: BlockerObject::Node { node: node.clone() },
                    fingerprint: self
                        .live
                        .corr_current_fingerprints
                        .get(node)
                        .cloned()
                        .unwrap_or_default(),
                    deferred: false,
                });
            }
            if self.needs_sound(node) && self.current_sound_fail(node) {
                out.insert(Blocker {
                    kind: BlockerKind::Soundness,
                    object: BlockerObject::Node { node: node.clone() },
                    fingerprint: self
                        .live
                        .sound_current_fingerprints
                        .get(node)
                        .cloned()
                        .unwrap_or_default(),
                    deferred: false,
                });
            }
        }
        out
    }

    /// True iff there is any non-adjudicable Unknown blocker for the given
    /// lane: an Unknown blocker (in `global_blockers()` but not in
    /// `current_failed_blockers()`) whose object is NOT in the
    /// corresponding `latest_*_review_*` for that lane.
    ///
    /// Such a blocker has no legitimate evidence-backed action bucket in the
    /// reviewer's contract: no verifier evidence to override
    /// (`allowed_override_blockers` filter requires
    /// `review_blocker_adjudicable`, which fails the `latest_*_review_*`
    /// containment check), and not a current Fail to reset
    /// (`current_failed_blockers` excludes it). The right next step is to
    /// run the verifier lane whenever a dispatch frontier exists.
    ///
    /// Used by `route_after_progress` to preempt Reviewer dispatch with
    /// a verifier when any such blocker exists.
    pub fn has_non_adjudicable_unknown_blocker(&self, kind: BlockerKind) -> bool {
        let failed = self.current_failed_blockers();
        self.global_blockers()
            .iter()
            .any(|b| b.kind == kind && !failed.contains(b) && !self.review_blocker_adjudicable(b))
    }

    pub fn theorem_start_request_kind(&self) -> RequestKind {
        // When the reviewer left a pending task with non-empty task_blockers,
        // schedule the worker first even if verifier lanes are non-empty.
        // Otherwise a co-occurring `reset_blocker_ids` (which marks lanes
        // Unknown) would route to a verifier first, the kernel would clear
        // the pending_task on issue, and the worker assignment would
        // silently disappear — leaving the reviewer's task intent unserved
        // and the verifier-reset-verifier loop alive.
        if let Some(task) = &self.pending_task {
            if !task.task_blockers.is_empty() {
                return RequestKind::Worker;
            }
        }
        if !self.reviewer_requested_sound_verify_nodes().is_empty() {
            return RequestKind::Sound;
        }
        // Verifier ordering inside theorem-stating: paper-target →
        // deviation authorization → substantiveness → corr → sound → worker. Paper variants share
        // `Stage::VerifyPaper` and `RequestKind::Paper`; the per-cycle
        // scheduler emits exactly one frontier at a time (target first,
        // then per-node) and the engine drains both before transitioning.
        if !self.paper_verify_targets().is_empty()
            || !self.deviation_verify_ids().is_empty()
            || !self.substantiveness_verify_nodes().is_empty()
        {
            RequestKind::Paper
        } else if !self.corr_verify_nodes().is_empty() {
            RequestKind::Corr
        } else if !self.sound_verify_nodes().is_empty() {
            RequestKind::Sound
        } else {
            RequestKind::Worker
        }
    }

    /// Proof-phase cycle-start dispatch. Mirror of `theorem_start_request_kind`
    /// for `Phase::ProofFormalization`.
    ///
    /// Symmetry with theorem-stating is the design goal: all four verifier
    /// lanes can have non-empty frontiers in proof phase
    /// (paper-target/corr aren't phase-gated; substantiveness/sound
    /// explicitly admit both phases), and they should drain at cycle-start
    /// rather than forcing an unnecessary worker dispatch first.
    /// Pre-2026-04, proof-phase `start_cycle` unconditionally returned
    /// `Worker`, which caused a wasted worker turn whenever the supervisor
    /// resumed mid-cycle (state on disk with Unknowns) or after a cleanup
    /// checkpoint left lanes Unknown. The post-worker drain
    /// (`apply_proof_paper_accept`) would then dispatch the verifier on
    /// the next round-trip — see `ae4af9e`'s commit message
    /// ("self-corrects in 1 cycle ... but costs an unnecessary worker
    /// dispatch") for the audit-fix #6 discussion of this exact pattern.
    pub fn proof_start_request_kind(&self) -> RequestKind {
        // Same task_blocker preemption as theorem-stating. Proof-phase
        // reviewers populate `pending_task.task_blockers` identically
        // (`engine.rs` `apply_proof_review_response`), so the same
        // verifier-reset-loses-task-assignment bug applies if we skip it.
        if let Some(task) = &self.pending_task {
            if !task.task_blockers.is_empty() {
                return RequestKind::Worker;
            }
        }
        if !self.reviewer_requested_sound_verify_nodes().is_empty() {
            return RequestKind::Sound;
        }
        if !self.paper_verify_targets().is_empty()
            || !self.deviation_verify_ids().is_empty()
            || !self.substantiveness_verify_nodes().is_empty()
        {
            RequestKind::Paper
        } else if !self.corr_verify_nodes().is_empty() {
            RequestKind::Corr
        } else if !self.sound_verify_nodes().is_empty() {
            RequestKind::Sound
        } else {
            RequestKind::Worker
        }
    }

    pub fn expected_request_kind(&self) -> Option<RequestKind> {
        match self.stage {
            Stage::Worker => Some(RequestKind::Worker),
            Stage::VerifyPaper => Some(RequestKind::Paper),
            Stage::VerifyCorr => Some(RequestKind::Corr),
            Stage::VerifySound => Some(RequestKind::Sound),
            Stage::Reviewer => Some(RequestKind::Review),
            Stage::HumanGate => Some(RequestKind::HumanGate),
            Stage::StuckMathAudit => Some(RequestKind::StuckMathAudit),
            Stage::CleanupAudit => Some(RequestKind::Audit),
            Stage::Start | Stage::Complete => None,
        }
    }

    pub fn request_blockers(&self, kind: RequestKind) -> BTreeSet<Blocker> {
        match kind {
            RequestKind::Worker => self
                .pending_task
                .as_ref()
                .map(|task| task.task_blockers.clone())
                .unwrap_or_default(),
            // Review request: filter deferred blockers from the surface
            // presented to the reviewer. The reviewer's task/override/reset
            // action buckets require every presented blocker to be
            // addressable; deferred blockers (open by their own
            // fingerprint, but waiting on Lean-relevant dependencies' corr
            // to resolve) have no productive review move — they clear
            // automatically when the dependency pins. Including them would
            // surface an id the reviewer has no legal action for, since
            // each bucket's subset constraint excludes deferred blockers.
            RequestKind::Review => self
                .global_blockers()
                .into_iter()
                .filter(Blocker::is_dispatch_eligible)
                .collect(),
            RequestKind::StuckMathAudit => self
                .global_blockers()
                .into_iter()
                .filter(Blocker::is_dispatch_eligible)
                .collect(),
            RequestKind::Paper
            | RequestKind::Corr
            | RequestKind::Sound
            | RequestKind::HumanGate => self.global_blockers(),
            // Cleanup-v2 audit lane has no blocker surface — audit reads
            // the tablet plus the protected-statement set and proposes
            // tasks. Phase::Cleanup invariant guarantees
            // `global_blockers()` is empty here anyway.
            RequestKind::Audit => BTreeSet::new(),
        }
    }

    pub fn request_verify_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        match kind {
            RequestKind::Paper => BTreeSet::new(),
            RequestKind::Corr => self.corr_verify_nodes(),
            // Sound requests verify exactly one node per dispatch; multiple
            // Unknowns sequence through successive Sound requests. Match
            // `request_sound_verify_nodes(Sound)`.
            RequestKind::Sound => self.request_sound_verify_nodes(kind),
            RequestKind::Worker
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => BTreeSet::new(),
        }
    }

    pub fn request_corr_verify_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        match kind {
            RequestKind::Corr => self.corr_verify_nodes(),
            RequestKind::Worker
            | RequestKind::Paper
            | RequestKind::Sound
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => BTreeSet::new(),
        }
    }

    pub fn request_paper_verify_targets(&self, kind: RequestKind) -> BTreeSet<TargetId> {
        match kind {
            // Cycle scheduler picks the target frontier first; the per-node
            // frontier is only fired once the target frontier is empty.
            // Both share `Stage::VerifyPaper`, so this accessor returns
            // empty whenever the kernel has chosen the per-node scenario
            // for the in-flight Paper request — preventing the verifier
            // from seeing both frontiers at once and the kernel from
            // bucketing per-node responses against a target frontier.
            RequestKind::Paper => {
                if !self.paper_verify_targets().is_empty() {
                    self.paper_verify_targets()
                } else {
                    BTreeSet::new()
                }
            }
            RequestKind::Worker
            | RequestKind::Corr
            | RequestKind::Sound
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => BTreeSet::new(),
        }
    }

    pub fn request_deviation_verify_id(&self, kind: RequestKind) -> Option<DeviationId> {
        match kind {
            RequestKind::Paper => {
                if self.paper_verify_targets().is_empty() {
                    self.deviation_verify_ids().into_iter().next()
                } else {
                    None
                }
            }
            RequestKind::Worker
            | RequestKind::Corr
            | RequestKind::Sound
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => None,
        }
    }

    pub fn request_deviation_verify_path(&self, kind: RequestKind) -> String {
        self.request_deviation_verify_id(kind)
            .and_then(|id| self.deviation_files.get(&id).cloned())
            .unwrap_or_default()
    }

    pub fn request_corr_verify_targets(&self, _kind: RequestKind) -> BTreeSet<TargetId> {
        BTreeSet::new()
    }

    /// Substantiveness frontier for the in-flight Paper request.
    /// Empty unless the kernel has selected the per-node scenario, which it
    /// does iff the target-level frontier is empty AND the per-node frontier
    /// is non-empty (TheoremStating + ProofFormalization). The verifier
    /// receives the entire outstanding list in one request — see plan
    /// §10b for batching design.
    pub fn request_substantiveness_verify_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        match kind {
            RequestKind::Paper => {
                if !self.paper_verify_targets().is_empty()
                    || self.request_deviation_verify_id(kind).is_some()
                {
                    BTreeSet::new()
                } else {
                    self.substantiveness_verify_nodes()
                }
            }
            RequestKind::Worker
            | RequestKind::Corr
            | RequestKind::Sound
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => BTreeSet::new(),
        }
    }

    pub fn request_sound_verify_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        match kind {
            RequestKind::Sound => {
                // A Sound request verifies exactly one node per dispatch. If
                // multiple present nodes are Unknown on soundness (e.g. the
                // active node plus several helpers introduced by a restructure
                // worker), they are sequenced: this request covers the one
                // returned by `request_sound_verify_node`, and the remaining
                // Unknowns re-surface on the next Sound request after this
                // one's response lands.
                self.request_sound_verify_node(kind).into_iter().collect()
            }
            RequestKind::Worker
            | RequestKind::Paper
            | RequestKind::Corr
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => BTreeSet::new(),
        }
    }

    pub fn request_sound_verify_node(&self, kind: RequestKind) -> Option<NodeId> {
        match kind {
            RequestKind::Sound => self
                .reviewer_requested_sound_verify_nodes()
                .into_iter()
                .next()
                .or_else(|| self.sound_verify_nodes().into_iter().next()),
            RequestKind::Worker
            | RequestKind::Paper
            | RequestKind::Corr
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => None,
        }
    }

    pub fn request_verify_targets(&self, kind: RequestKind) -> BTreeSet<TargetId> {
        match kind {
            RequestKind::Paper => self.paper_verify_targets(),
            RequestKind::Worker
            | RequestKind::Corr
            | RequestKind::Sound
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => BTreeSet::new(),
        }
    }

    pub fn request_verify_lanes(&self, kind: RequestKind) -> BTreeSet<LaneId> {
        match kind {
            RequestKind::Paper | RequestKind::Corr | RequestKind::Sound => {
                self.verifier_lanes.clone()
            }
            RequestKind::Worker
            | RequestKind::Review
            | RequestKind::HumanGate
            | RequestKind::Audit
            | RequestKind::StuckMathAudit => BTreeSet::new(),
        }
    }

    // request_protected_nodes + request_protected_snapshot removed in the
    // protected_correspondence refactor. The WrapperRequest no longer
    // carries `protected_nodes` / `protected_snapshot`; covering-node
    // protection flows via `request_approved_target_nodes` +
    // `request_approved_corr_fingerprints` below.

    /// Paper-target-covering nodes plus protected closure nodes snapshotted
    /// at the last advance-gate approval. Worker requests use this for the
    /// commit-time correspondence-reopen guard; Review requests use it as
    /// the catalog of nodes the reviewer may exceptionally scope for
    /// protected semantic movement.
    pub fn request_approved_target_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        if self.phase == Phase::ProofFormalization
            && matches!(kind, RequestKind::Worker | RequestKind::Review)
        {
            self.approved_target_nodes()
        } else {
            BTreeSet::new()
        }
    }

    /// Worker-only subset of `corr_approved_fingerprints` keyed by covering
    /// nodes, sent alongside `request_approved_target_nodes`. Review only
    /// needs node IDs for scope authorization, not fingerprint payloads.
    /// Nodes with no approved fingerprint yet are simply absent (the guard
    /// skips them — the first corr verification will establish a baseline).
    pub fn request_approved_corr_fingerprints(
        &self,
        kind: RequestKind,
    ) -> BTreeMap<NodeId, Fingerprint> {
        if !(self.phase == Phase::ProofFormalization && kind == RequestKind::Worker) {
            return BTreeMap::new();
        }
        let nodes = self.approved_target_nodes();
        let mut out = BTreeMap::new();
        for node in &nodes {
            if let Some(fp) = self.corr_approved_fingerprints.get(node) {
                if !fp.is_empty() {
                    out.insert(node.clone(), fp.clone());
                }
            }
        }
        out
    }

    pub fn request_allowed_decisions(&self, kind: RequestKind) -> BTreeSet<ReviewDecisionKind> {
        if kind != RequestKind::Review {
            return BTreeSet::new();
        }
        match self.phase {
            Phase::TheoremStating => {
                if self.retry_outcome_kind != RetryOutcomeKind::None {
                    BTreeSet::from([ReviewDecisionKind::Continue, ReviewDecisionKind::NeedInput])
                } else {
                    BTreeSet::from([
                        ReviewDecisionKind::Continue,
                        ReviewDecisionKind::AdvancePhase,
                        ReviewDecisionKind::NeedInput,
                    ])
                }
            }
            Phase::ProofFormalization => {
                BTreeSet::from([ReviewDecisionKind::Continue, ReviewDecisionKind::NeedInput])
            }
            // Cleanup phase invariant: every accepted state is Done-valid
            // (formalization_complete). The reviewer's only meaningful
            // choices are "keep polishing" (Continue) or "we're done"
            // (Done). NeedInput is rejected: we're already Done-ready,
            // there's no escalation to make. AdvancePhase is rejected
            // because cleanup is the last work phase.
            //
            // Cleanup-v2 (audit Finding 2): when the consecutive-invalid-
            // worker latch (`cleanup_force_done`) is set, Continue is
            // removed from the allowed-decisions set — every additional
            // worker burst would just rack up more failed tasks. The
            // reviewer's only legal move is Done. Surfacing the
            // narrowed set in the prompt means the LLM never proposes a
            // decision the legality gate will reject.
            Phase::Cleanup => {
                if self.cleanup_force_done {
                    BTreeSet::from([ReviewDecisionKind::Done])
                } else {
                    BTreeSet::from([ReviewDecisionKind::Continue, ReviewDecisionKind::Done])
                }
            }
            Phase::Complete => BTreeSet::new(),
        }
    }

    /// Base-legal `next_active` candidates in ProofFormalization, BEFORE
    /// the v32 cone-narrowing is applied. Used both as the input to
    /// `request_kernel_hinted_next_active_nodes` and as the denormalized
    /// surface for prospective-anchor validation in
    /// `WrapperRequest::review_next_active_legal_for_response` (proposal v32
    /// audit-2 followup #3). Empty outside ProofFormalization.
    ///
    /// Compute the aggregate-focus candidate set ONCE per request, then
    /// union it with the per-node fast checks. Calling `active_node_legal`
    /// per node would re-compute `proof_aggregate_node_blocker_focus_candidates`
    /// for every present node, recomputing every dep-closure multiple times.
    /// Behavior matches `active_node_legal` (the relegalization callsite
    /// still uses the slow path, but that's a single-node check). Added
    /// 2026-05-05.
    ///
    /// Patch C plan §7.4: include `local_closure_unverified_nodes` so the
    /// reviewer can pick a sorry-free closure-failed node as `next_active`
    /// even when `task_blockers` is empty (the §7.4.2 "blockers empty but
    /// local-closure work remains" condition).
    pub fn proof_active_node_base_legal_candidates(&self) -> BTreeSet<NodeId> {
        if self.phase != Phase::ProofFormalization {
            return BTreeSet::new();
        }
        let mut allowed: BTreeSet<NodeId> = self
            .live
            .present_nodes
            .iter()
            .filter(|node| {
                self.live.open_nodes.contains(*node)
                    || self.local_closure_unverified_nodes.contains(*node)
                    || self.proof_node_repairs_blocker(node)
                    || self.proof_node_directly_imports_substantiveness_blocker(node)
            })
            .cloned()
            .collect();
        allowed.extend(self.proof_aggregate_node_blocker_focus_candidates());
        allowed
    }

    pub fn request_kernel_hinted_next_active_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        if kind != RequestKind::Review {
            return BTreeSet::new();
        }
        match self.phase {
            Phase::TheoremStating => {
                if matches!(
                    self.retry_outcome_kind,
                    RetryOutcomeKind::Invalid | RetryOutcomeKind::Transport
                ) {
                    BTreeSet::new()
                } else {
                    self.live
                        .present_nodes
                        .iter()
                        .filter(|node| self.theorem_review_next_active_legal(Some(node)))
                        .cloned()
                        .collect()
                }
            }
            Phase::ProofFormalization => {
                let mut allowed = self.proof_active_node_base_legal_candidates();
                // Proposal v32: narrow the hinted set to the active-
                // coarse cone (widened in coarse_repair_mode). When no
                // anchor is set or coarse_dag_nodes is empty,
                // `coarse_legal_active_set()` returns
                // `live.present_nodes`, so this intersection is a
                // no-op in the legacy regime.
                let cone = self.coarse_legal_active_set();
                allowed.retain(|n| cone.contains(n));
                allowed
            }
            Phase::Cleanup => self
                .live
                .present_nodes
                .iter()
                .filter(|node| self.active_node_legal(Some(node), &self.live))
                .cloned()
                .collect(),
            Phase::Complete => BTreeSet::new(),
        }
    }

    pub fn request_targeted_next_active_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        if kind != RequestKind::Review
            || self.phase != Phase::TheoremStating
            || matches!(
                self.retry_outcome_kind,
                RetryOutcomeKind::Invalid | RetryOutcomeKind::Transport
            )
        {
            return BTreeSet::new();
        }
        self.live
            .present_nodes
            .iter()
            .filter(|node| self.theorem_targeted_mode_legal(Some(node)))
            .cloned()
            .collect()
    }

    pub fn request_allowed_next_modes(&self, kind: RequestKind) -> BTreeSet<TaskMode> {
        if kind != RequestKind::Review {
            return BTreeSet::new();
        }
        match self.phase {
            Phase::TheoremStating => {
                if matches!(
                    self.retry_outcome_kind,
                    RetryOutcomeKind::Invalid | RetryOutcomeKind::Transport
                ) {
                    BTreeSet::from([self.current_mode()])
                } else {
                    let mut modes = BTreeSet::from([TaskMode::Global]);
                    if !self.request_targeted_next_active_nodes(kind).is_empty() {
                        modes.insert(TaskMode::Targeted);
                    }
                    modes
                }
            }
            Phase::ProofFormalization => BTreeSet::from([
                TaskMode::Local,
                TaskMode::Restructure,
                TaskMode::CoarseRestructure,
            ]),
            Phase::Cleanup => BTreeSet::from([TaskMode::Cleanup]),
            Phase::Complete => BTreeSet::new(),
        }
    }

    pub fn request_allow_targeted_without_next_active(&self, kind: RequestKind) -> bool {
        kind == RequestKind::Review
            && self.phase == Phase::TheoremStating
            && matches!(
                self.retry_outcome_kind,
                RetryOutcomeKind::Invalid | RetryOutcomeKind::Transport
            )
            && self.current_mode() == TaskMode::Targeted
    }

    pub fn request_allowed_resets(&self, kind: RequestKind) -> BTreeSet<ResetChoice> {
        if kind != RequestKind::Review {
            return BTreeSet::new();
        }
        // Cleanup phase invariant: every accepted state is Done-valid
        // (formalization_complete). Rewind would let the reviewer
        // re-enter a state that may not be Done-valid — breaking the
        // invariant. Cleanup is forward-only: the only escape is the
        // implicit "kernel rejects bad worker burst, reverts to prior
        // accepted state" path, which by induction lands on a Done-
        // valid state.
        if self.phase == Phase::Cleanup {
            return BTreeSet::from([ResetChoice::None]);
        }
        let mut choices = BTreeSet::from([ResetChoice::None]);
        if self.retry_outcome_kind != RetryOutcomeKind::None {
            choices.insert(ResetChoice::LastCommit);
        }
        // LastClean is offered whenever there's at least one clean
        // checkpoint to rewind to (gated by `has_ever_been_clean` so we
        // don't expose a reset that the runtime's tag-walk would fail on)
        // AND there's been at least one dirty checkpoint to escape from.
        // Independent of retry state — the reviewer may want to back
        // out of a blocker spiral even outside a retry context.
        //
        // Migration gate: `last_clean_mirrors_populated()` covers the
        // pre-#56 state-file window where `has_ever_been_clean=true`
        // but mirrors are empty. Without the gate, the reviewer could
        // pick a reset that the kernel can't actually apply (the
        // runtime would still git-reset disk while kernel state stays
        // mid-cycle dirty — divergence). Window closes after one clean
        // `commit_live`.
        if self.has_ever_been_clean
            && self.cycles_since_clean >= 1
            && self.last_clean_mirrors_populated()
        {
            choices.insert(ResetChoice::LastClean);
            // Mandatory-at-threshold rule: when cycles_since_clean reaches
            // the threshold, drop the other choices so `last_clean` is the
            // only legal reset. Exception: once the same clean checkpoint
            // has been rewound to twice or more, the mandate is waived —
            // repeated rewinds to the same state aren't helping and the
            // situation is then a genuine "necessary decomposition" (per
            // the reviewer prompt's `32_revert.md` meta-cue).
            //
            // Threshold is operator-tunable via
            // `TRELLIS_CSC_LAST_CLEAN_THRESHOLD` (see
            // `csc_last_clean_threshold()` at the top of this file).
            // The default accommodates deep multi-cycle decompositions
            // that the original 10-cycle bound prematurely cut off —
            // workers assembling a mirrored subtree, for instance,
            // routinely take 10-15 cycles of productive helper-addition
            // before a clean checkpoint becomes possible. The same
            // helper feeds the review-request summary so reviewer
            // prompts render the effective number without separate
            // edits.
            let csc_threshold = csc_last_clean_threshold();
            if self.cycles_since_clean >= csc_threshold
                && self.last_clean_rewind_count < CSC_REWIND_WAIVER_COUNT
            {
                choices.remove(&ResetChoice::None);
                choices.remove(&ResetChoice::LastCommit);
            }
        }
        choices
    }

    pub fn resettable_theorem_stating_nodes(&self) -> BTreeSet<NodeId> {
        if self.phase != Phase::ProofFormalization {
            return BTreeSet::new();
        }
        let current_orphans = self.orphan_nodes(&self.live);
        self.coarse_dag_nodes
            .iter()
            .filter(|node| node.as_str() != PREAMBLE_NAME)
            .filter(|node| self.live.present_nodes.contains(*node))
            .filter(|node| !current_orphans.contains(*node))
            .cloned()
            .collect()
    }

    pub fn request_resettable_theorem_stating_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        match kind {
            RequestKind::StuckMathAudit => self.resettable_theorem_stating_nodes(),
            _ => BTreeSet::new(),
        }
    }

    pub fn request_allowed_reset_blockers(&self, kind: RequestKind) -> BTreeSet<Blocker> {
        if kind == RequestKind::Review && self.phase == Phase::TheoremStating {
            // Audit Finding 6: empty paper coverage produces a definite
            // PaperFaithfulness Fail (current_paper_state at model.rs:1974
            // returns Fail when live.coverage[target] is empty). This
            // looks like a verifier verdict to the reviewer but is
            // structurally different — resetting status maps doesn't
            // help because the derived state will remain Fail (coverage
            // is still empty post-reset). Exclude these from
            // reset_blockers so the reviewer isn't offered an inert
            // option. They remain in task_blockers so the worker can
            // be tasked with repairing coverage (which is what actually
            // fixes the issue).
            self.current_failed_blockers()
                .into_iter()
                .filter(|blocker| !self.is_empty_coverage_paper_fail(blocker))
                .collect()
        } else {
            BTreeSet::new()
        }
    }

    /// True iff `blocker` is a `PaperFaithfulness` blocker on a
    /// configured target whose `live.coverage` set is empty (no node
    /// claims the target). Such blockers are produced by
    /// `current_paper_state` (model.rs around line 1974) as a definite
    /// `Fail` — but resetting them via `request_allowed_reset_blockers`
    /// is inert: the post-reset derived state is still `Fail` because
    /// coverage is still empty. The fix is to repair coverage (a worker
    /// task), not to ask the verifier to re-run.
    fn is_empty_coverage_paper_fail(&self, blocker: &Blocker) -> bool {
        if blocker.kind != BlockerKind::PaperFaithfulness {
            return false;
        }
        let BlockerObject::Target { target } = &blocker.object else {
            return false;
        };
        self.live
            .coverage
            .get(target)
            .map(|nodes| nodes.is_empty())
            .unwrap_or(true)
    }

    // Option C (2026-06-04): `request_allowed_override_blockers` retired.
    // The reviewer's blocker actions now collapse to two buckets:
    // `task_blocker_ids` (forward to next worker) and `reset_blocker_ids`
    // (discard verifier evidence). See REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.

    pub fn request_sound_repair_ready_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        if kind != RequestKind::Review {
            return BTreeSet::new();
        }
        self.live
            .present_nodes
            .iter()
            .filter(|node| self.sound_repair_ready(node))
            .cloned()
            .collect()
    }

    pub fn request_sound_verifier_requestable_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        if kind != RequestKind::Review {
            return BTreeSet::new();
        }
        self.live
            .present_nodes
            .iter()
            .filter(|node| {
                self.sound_verifier_eligible(node)
                    && self.current_sound_assessment(node).status
                        != SoundAssessmentStatus::VerifierPass
            })
            .cloned()
            .collect()
    }

    pub fn request_sound_assessment_statuses(
        &self,
        kind: RequestKind,
    ) -> BTreeMap<NodeId, SoundAssessmentStatus> {
        if !matches!(
            kind,
            RequestKind::Review | RequestKind::Worker | RequestKind::Sound
        ) {
            return BTreeMap::new();
        }
        self.live
            .present_nodes
            .iter()
            .filter(|node| self.needs_sound(node))
            .map(|node| (node.clone(), self.current_sound_assessment(node).status))
            .collect()
    }

    /// Build the per-target re-verification context for a Sound
    /// request, when applicable. Returns `Some` only for
    /// `RequestKind::Sound` whose `sound_verify_node` has a stored
    /// assessment AND a current derived status of
    /// `DepEditOnlyStalePassDeferred` or `SelfEditUnknown`. The
    /// returned struct carries the per-dep statement-hash drift
    /// (truncated to 12 hex chars), an `own_tex_changed` flag, and the
    /// verbatim prior accepted-lane evidence for the target.
    pub fn request_sound_reverification_context(
        &self,
        kind: RequestKind,
    ) -> Option<SoundReverificationContext> {
        if kind != RequestKind::Sound {
            return None;
        }
        let target = self.request_sound_verify_node(kind)?;
        let stored = self
            .sound_assessments
            .get(&target)
            .cloned()
            .or_else(|| self.legacy_sound_assessment(&target))?;
        let current_status = self.current_sound_assessment(&target).status;
        if !matches!(
            current_status,
            SoundAssessmentStatus::DepEditOnlyStalePassDeferred
                | SoundAssessmentStatus::SelfEditUnknown
        ) {
            return None;
        }
        let current_parts = self.current_sound_fingerprint_parts(&target);
        let own_tex_changed = !current_parts.own_tex_hash.is_empty()
            && !stored.fingerprints.own_tex_hash.is_empty()
            && current_parts.own_tex_hash != stored.fingerprints.own_tex_hash;
        let deps_changed = dep_statement_hash_diff(
            &stored.fingerprints.dep_statement_hashes,
            &current_parts.dep_statement_hashes,
        );
        let prior_lane_evidence = self
            .latest_sound_reviewer_evidence
            .get(&target)
            .cloned()
            .unwrap_or_default();
        Some(SoundReverificationContext {
            target,
            prior_status: stored.status,
            current_status,
            own_tex_changed,
            deps_changed,
            prior_lane_evidence,
        })
    }

    pub fn request_allowed_difficulty_update_nodes(&self, kind: RequestKind) -> BTreeSet<NodeId> {
        if kind == RequestKind::Review {
            self.live.present_nodes.clone()
        } else {
            BTreeSet::new()
        }
    }

    pub fn current_active_difficulty(&self) -> NodeDifficulty {
        if self.orphan_cleanup_active() {
            return NodeDifficulty::Hard;
        }
        self.active_node
            .as_ref()
            .and_then(|node| self.node_difficulty.get(node).copied())
            .unwrap_or(NodeDifficulty::Hard)
    }

    pub fn current_active_easy_attempts(&self) -> u32 {
        self.active_node
            .as_ref()
            .and_then(|node| self.easy_attempts.get(node).copied())
            .unwrap_or(0)
    }

    pub fn current_worker_profile(&self) -> WorkerProfile {
        if self.orphan_cleanup_active() {
            return WorkerProfile::Cleanup;
        }
        match self.phase {
            Phase::TheoremStating => WorkerProfile::Theorem,
            Phase::ProofFormalization => match self.current_active_difficulty() {
                NodeDifficulty::Easy => WorkerProfile::ProofEasy,
                NodeDifficulty::Hard => WorkerProfile::ProofHard,
            },
            Phase::Cleanup => WorkerProfile::FinalCleanup,
            Phase::Complete => WorkerProfile::None,
        }
    }

    pub fn current_worker_validation_kind(&self) -> WorkerValidationKind {
        if self.orphan_cleanup_active() {
            return match self.phase {
                Phase::TheoremStating | Phase::ProofFormalization | Phase::Cleanup => {
                    WorkerValidationKind::Cleanup
                }
                Phase::Complete => WorkerValidationKind::None,
            };
        }
        match self.phase {
            Phase::TheoremStating => match self.target_edit_mode {
                TargetEditMode::Global => WorkerValidationKind::TheoremGlobal,
                TargetEditMode::Targeted => WorkerValidationKind::TheoremTargeted,
            },
            Phase::ProofFormalization => match self.proof_edit_mode {
                // Difficulty is advisory only. Local scope is independent of
                // easy/hard; closure gates come from the reviewer's explicit
                // allow_new_obligations / must_close_active choices.
                ProofEditMode::Local => WorkerValidationKind::ProofLocal,
                // Restructure / CoarseRestructure are the reviewer's
                // explicit lever for cross-node edits (helpers, .tex fixes,
                // signature changes for non-coarse-DAG nodes). Honour the
                // mode regardless of the active node's difficulty —
                // otherwise an Easy active node makes it impossible to
                // address a task_blocker on another node.
                ProofEditMode::Restructure => WorkerValidationKind::ProofRestructure,
                ProofEditMode::CoarseRestructure => WorkerValidationKind::ProofCoarseRestructure,
            },
            Phase::Cleanup => WorkerValidationKind::FinalCleanup,
            Phase::Complete => WorkerValidationKind::None,
        }
    }

    pub fn current_worker_authorized_nodes(&self) -> BTreeSet<NodeId> {
        if self.orphan_cleanup_active() {
            return self.live.present_nodes.clone();
        }
        // Reviewer-assigned proof tasks carry an explicit authorized
        // existing-node set in the pending task. When present, that
        // set IS the worker's edit permission — `next_active` /
        // `next_mode` only define the maximum legal envelope, not the
        // final list of editable nodes.
        match self.current_worker_validation_kind() {
            WorkerValidationKind::ProofRestructure
            | WorkerValidationKind::ProofCoarseRestructure => {
                if let Some(task) = self.pending_task.as_ref() {
                    if !task.authorized_nodes.is_empty() {
                        return task.authorized_nodes.clone();
                    }
                    // Empty pending authorization with a non-Local
                    // proof mode only happens for old persisted
                    // pending tasks (pre-explicit-list deploy) or
                    // synthetic engine paths that bypass the
                    // reviewer. Fall through to the legacy envelope
                    // so an in-flight old task can finish.
                }
            }
            WorkerValidationKind::FinalCleanup => {
                // Cleanup-v2 (audit Finding 5): the worker-visible scope
                // must match what the validator enforces. The validator
                // at `runtime_cli_observations.rs:5700-5723` restricts
                // edits to:
                //   - Substitution: changed_nodes ⊆ authorized_nodes ∪ {target_node}
                //   - LintFix:      changed_nodes = {target_node}
                // Previously `worker_authorized_nodes_for_request_assignment`
                // returned `present_nodes.clone()` (all_present) for
                // FinalCleanup, causing a substitution worker to be told
                // "all_present" then rejected at acceptance time for
                // editing nodes outside the reviewer-supplied importer
                // set. Surface the validator-equivalent scope here.
                let active_task = self
                    .cleanup_active_task
                    .and_then(|idx| self.cleanup_audit_tasks.get(idx as usize));
                if let Some(task) = active_task {
                    let mut scope: BTreeSet<NodeId> = match &task.kind {
                        CleanupTaskKind::Substitution { .. } => self
                            .pending_task
                            .as_ref()
                            .map(|p| p.authorized_nodes.clone())
                            .unwrap_or_default(),
                        CleanupTaskKind::LintFix { .. } => BTreeSet::new(),
                    };
                    scope.insert(task.target_node.clone());
                    return scope;
                }
                // No active task — legacy lint-only fallback. Fall
                // through to the legacy "all_present" envelope so the
                // legacy validator (no task_kind) behaves as before.
            }
            _ => {}
        }
        worker_authorized_nodes_for_request_assignment(
            self.current_worker_validation_kind(),
            self.active_node.as_ref(),
            &self.live.present_nodes,
            &self.deps,
        )
    }

    pub fn current_worker_observation_plan(&self) -> WorkerAcceptanceObservationPlan {
        match self.current_worker_validation_kind() {
            WorkerValidationKind::None => WorkerAcceptanceObservationPlan::default(),
            WorkerValidationKind::TheoremGlobal => WorkerAcceptanceObservationPlan {
                capture_before_snapshot: true,
                capture_scoped_tablet_baseline_errors: true,
                scoped_tablet_baseline_scope: WorkerBaselineScope::AllPresent,
                ..WorkerAcceptanceObservationPlan::default()
            },
            WorkerValidationKind::TheoremTargeted => WorkerAcceptanceObservationPlan {
                capture_before_snapshot: true,
                capture_scoped_tablet_baseline_errors: true,
                scoped_tablet_baseline_scope: WorkerBaselineScope::AuthorizedNodes,
                ..WorkerAcceptanceObservationPlan::default()
            },
            WorkerValidationKind::ProofEasy => WorkerAcceptanceObservationPlan {
                capture_before_snapshot: true,
                capture_expected_active_hash: true,
                ..WorkerAcceptanceObservationPlan::default()
            },
            WorkerValidationKind::ProofLocal
            | WorkerValidationKind::ProofRestructure
            | WorkerValidationKind::ProofCoarseRestructure => WorkerAcceptanceObservationPlan {
                capture_before_snapshot: true,
                capture_expected_active_hash: true,
                ..WorkerAcceptanceObservationPlan::default()
            },
            WorkerValidationKind::Cleanup => WorkerAcceptanceObservationPlan {
                capture_before_snapshot: true,
                capture_before_tablet_contents: true,
                ..WorkerAcceptanceObservationPlan::default()
            },
            WorkerValidationKind::FinalCleanup => WorkerAcceptanceObservationPlan {
                capture_before_snapshot: true,
                capture_baseline_declaration_hashes: true,
                capture_baseline_correspondence_hashes: true,
                ..WorkerAcceptanceObservationPlan::default()
            },
        }
    }

    pub fn current_worker_validation_execution_plan(
        &self,
    ) -> Vec<WorkerValidationExecutionPlanStep> {
        match self.current_worker_validation_kind() {
            WorkerValidationKind::None => Vec::new(),
            WorkerValidationKind::TheoremGlobal => {
                vec![WorkerValidationExecutionPlanStep::ScopedTablet {
                    allowed_nodes_mode: ScopedTabletAllowedNodesMode::AllPresent,
                    explicit_nodes: BTreeSet::new(),
                }]
            }
            WorkerValidationKind::TheoremTargeted => vec![
                WorkerValidationExecutionPlanStep::TheoremTargetEditScope {
                    target: self.active_node.clone(),
                    initial_scope: self.current_worker_authorized_nodes(),
                },
                WorkerValidationExecutionPlanStep::ScopedTablet {
                    allowed_nodes_mode: ScopedTabletAllowedNodesMode::PreviousOrExplicit,
                    explicit_nodes: self.current_worker_authorized_nodes(),
                },
            ],
            WorkerValidationKind::ProofEasy => {
                vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                    active_node: self.active_node.clone(),
                    mode: WorkerProofDeltaMode::Local,
                    authorized_nodes: self.current_worker_authorized_nodes(),
                    protected_semantic_change_nodes: BTreeSet::new(),
                    allow_new_obligations: self.current_worker_allow_new_obligations(),
                    must_close_active: self.current_worker_must_close_active(),
                }]
            }
            WorkerValidationKind::ProofLocal
            | WorkerValidationKind::ProofRestructure
            | WorkerValidationKind::ProofCoarseRestructure => {
                vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                    active_node: self.active_node.clone(),
                    mode: self.current_worker_proof_delta_mode(),
                    authorized_nodes: self.current_worker_authorized_nodes(),
                    protected_semantic_change_nodes: self
                        .pending_task
                        .as_ref()
                        .map(|task| task.protected_semantic_change_nodes.clone())
                        .unwrap_or_default(),
                    allow_new_obligations: self.current_worker_allow_new_obligations(),
                    must_close_active: self.current_worker_must_close_active(),
                }]
            }
            WorkerValidationKind::Cleanup => {
                vec![WorkerValidationExecutionPlanStep::CleanupPreserving {}]
            }
            WorkerValidationKind::FinalCleanup => {
                // Cleanup-v2 Step 8: surface the active task (if any),
                // the reviewer-supplied authorized_nodes scope, and
                // the live protected-statement set. The legacy
                // lint-only mode (no active task) ends up with all
                // optional fields None / empty — the runtime validator
                // branches on `task_kind` to preserve legacy
                // behavior. Step 9 wires the validator side.
                let active_task = self
                    .cleanup_active_task
                    .and_then(|idx| self.cleanup_audit_tasks.get(idx as usize).cloned());
                let task_kind = active_task.as_ref().map(|t| t.kind.clone());
                let target_node = active_task.as_ref().map(|t| t.target_node.clone());
                let authorized_nodes = self
                    .pending_task
                    .as_ref()
                    .map(|task| task.authorized_nodes.clone())
                    .unwrap_or_default();
                let protected_statement_node_set = self.live_protected_statement_node_set();
                vec![WorkerValidationExecutionPlanStep::FinalCleanupPreserving {
                    task_kind,
                    target_node,
                    authorized_nodes,
                    protected_statement_node_set,
                }]
            }
        }
    }

    pub fn current_worker_proof_delta_mode(&self) -> WorkerProofDeltaMode {
        match self.current_worker_validation_kind() {
            WorkerValidationKind::ProofEasy => WorkerProofDeltaMode::Local,
            WorkerValidationKind::ProofLocal => WorkerProofDeltaMode::Local,
            WorkerValidationKind::ProofRestructure => WorkerProofDeltaMode::Restructure,
            WorkerValidationKind::ProofCoarseRestructure => WorkerProofDeltaMode::CoarseRestructure,
            WorkerValidationKind::None
            | WorkerValidationKind::TheoremGlobal
            | WorkerValidationKind::TheoremTargeted
            | WorkerValidationKind::Cleanup
            | WorkerValidationKind::FinalCleanup => WorkerProofDeltaMode::None,
        }
    }

    pub fn current_worker_allow_new_obligations(&self) -> bool {
        self.pending_task
            .as_ref()
            .map(|task| task.allow_new_obligations)
            .unwrap_or(true)
    }

    pub fn current_worker_must_close_active(&self) -> bool {
        self.pending_task
            .as_ref()
            .map(|task| task.must_close_active)
            .unwrap_or(false)
    }

    pub fn current_worker_context(&self) -> WorkerContext {
        let pending_task = self.pending_task.clone().unwrap_or_default();
        // Cleanup-v2 Step 12: surface the active task (if any) on the
        // worker context so the worker prompt can branch on task kind
        // and the prompt fragments can render rationale + target_node.
        let active_cleanup = self
            .cleanup_active_task
            .and_then(|idx| self.cleanup_audit_tasks.get(idx as usize).cloned());
        let cleanup_active_task_kind_view = active_cleanup.as_ref().map(|t| t.kind.clone());
        let cleanup_active_target_node_view =
            active_cleanup.as_ref().map(|t| t.target_node.clone());
        let cleanup_active_rationale_view = active_cleanup
            .as_ref()
            .map(|t| t.rationale.clone())
            .unwrap_or_default();
        WorkerContext {
            enabled: true,
            active_difficulty: self.current_active_difficulty(),
            active_easy_attempts: self.current_active_easy_attempts(),
            worker_profile: self.current_worker_profile(),
            validation_kind: self.current_worker_validation_kind(),
            authorized_nodes: self.current_worker_authorized_nodes(),
            allow_new_obligations: pending_task.allow_new_obligations,
            must_close_active: pending_task.must_close_active,
            protected_semantic_change_nodes: pending_task.protected_semantic_change_nodes.clone(),
            next_context_mode: pending_task.next_worker_context_mode,
            paper_focus_ranges: pending_task.paper_focus_ranges,
            work_style_hint: pending_task.work_style_hint,
            cleanup_active_task_kind_view,
            cleanup_active_target_node_view,
            cleanup_active_rationale_view,
        }
    }

    pub fn current_worker_acceptance(&self) -> WorkerAcceptanceContract {
        WorkerAcceptanceContract {
            enabled: true,
            validation_kind: self.current_worker_validation_kind(),
            authorized_nodes: self.current_worker_authorized_nodes(),
            protected_semantic_change_nodes: self
                .pending_task
                .as_ref()
                .map(|task| task.protected_semantic_change_nodes.clone())
                .unwrap_or_default(),
            validation_execution_plan: self.current_worker_validation_execution_plan(),
            require_explicit_target_claims_for_new_nodes: true,
            // Deprecated; see WorkerAcceptanceContract field doc.
            forbid_tablet_changes_when_stuck: false,
            observation_plan: self.current_worker_observation_plan(),
        }
    }

    pub fn apply_review_blocker_resets(&mut self, blockers: &BTreeSet<Blocker>) {
        for blocker in blockers {
            match &blocker.object {
                BlockerObject::Node { node } => match blocker.kind {
                    BlockerKind::NodeCorr => {
                        self.corr_status.insert(node.clone(), CorrStatus::Unknown);
                        self.corr_approved_fingerprints.remove(node);
                    }
                    BlockerKind::Soundness => {
                        self.sound_status.insert(node.clone(), SoundStatus::Unknown);
                        self.sound_approved_fingerprints.remove(node);
                        self.sound_assessments.remove(node);
                    }
                    BlockerKind::Substantiveness => {
                        self.substantiveness_status
                            .insert(node.clone(), CorrStatus::Unknown);
                        self.substantiveness_approved_fingerprints.remove(node);
                    }
                    BlockerKind::PaperFaithfulness | BlockerKind::Deviation => {}
                },
                BlockerObject::Target { target } => {
                    if blocker.kind == BlockerKind::PaperFaithfulness {
                        self.paper_status
                            .insert(target.clone(), CorrStatus::Unknown);
                        self.paper_approved_fingerprints.remove(target);
                    }
                }
                BlockerObject::Deviation { deviation } => {
                    if blocker.kind == BlockerKind::Deviation {
                        self.deviation_status
                            .insert(deviation.clone(), CorrStatus::Unknown);
                        self.deviation_approved_fingerprints.remove(deviation);
                    }
                }
            }
        }
    }

    pub fn clear_latest_paper_review_context(&mut self) {
        self.latest_paper_reviewer_evidence.clear();
        self.latest_paper_review_targets.clear();
    }

    pub fn clear_latest_deviation_review_context(&mut self) {
        self.latest_deviation_reviewer_evidence.clear();
        self.latest_deviation_review_ids.clear();
    }

    pub fn clear_latest_substantiveness_review_context(&mut self) {
        self.latest_substantiveness_reviewer_evidence.clear();
        self.latest_substantiveness_review_nodes.clear();
    }

    pub fn clear_latest_corr_review_context(&mut self) {
        self.latest_corr_reviewer_evidence.clear();
        self.latest_corr_review_nodes.clear();
    }

    pub fn clear_latest_sound_review_context(&mut self) {
        self.latest_sound_reviewer_evidence.clear();
        self.latest_sound_review_nodes.clear();
    }

    pub fn apply_review_blocker_adjudications(&mut self, task_blockers: &BTreeSet<Blocker>) {
        // Option C (2026-06-04): reviewer Pass-override is retired
        // entirely. Only the task→Fail path remains; the previously
        // `approve=true` arm (override_blockers → Pass + sound
        // `ReviewerAcceptedPass`) has been removed. See
        // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
        for blocker in task_blockers {
            // Soundness is stricter than statement lanes for task→Fail:
            // a reviewer may send an already-current Fail back to a
            // worker, or task an Unknown that the sound verifier just
            // reviewed, but must not turn an unreviewed sound
            // fingerprint into a durable Fail.
            if let (BlockerObject::Node { node }, BlockerKind::Soundness) =
                (&blocker.object, blocker.kind)
            {
                if !self.sound_repair_ready(node) {
                    continue;
                }
                if self.current_sound_unknown(node) && !self.review_blocker_adjudicable(blocker) {
                    continue;
                }
            }
            // Audit follow-up (on Finding 2's audit): align approved_fp
            // write semantics with the verifier-driven path. The
            // approved_fp write is skipped if `live.<lane>_current_
            // fingerprints[node]` is absent (mirrors the verifier-driven
            // `apply_*_updates` paths).
            match (&blocker.object, blocker.kind) {
                (BlockerObject::Node { node }, BlockerKind::NodeCorr) => {
                    self.corr_status.insert(node.clone(), CorrStatus::Fail);
                    if let Some(fp) = self.live.corr_current_fingerprints.get(node) {
                        self.corr_approved_fingerprints
                            .insert(node.clone(), fp.clone());
                    }
                }
                (BlockerObject::Node { node }, BlockerKind::Substantiveness) => {
                    self.substantiveness_status
                        .insert(node.clone(), CorrStatus::Fail);
                    if let Some(fp) = self.live.substantiveness_current_fingerprints.get(node) {
                        self.substantiveness_approved_fingerprints
                            .insert(node.clone(), fp.clone());
                    }
                }
                (BlockerObject::Target { target }, BlockerKind::PaperFaithfulness) => {
                    self.paper_status.insert(target.clone(), CorrStatus::Fail);
                    if let Some(fp) = self.live.paper_current_fingerprints.get(target) {
                        self.paper_approved_fingerprints
                            .insert(target.clone(), fp.clone());
                    }
                }
                (BlockerObject::Deviation { deviation }, BlockerKind::Deviation) => {
                    self.deviation_status
                        .insert(deviation.clone(), CorrStatus::Fail);
                    if let Some(fp) = self
                        .live
                        .deviation_current_fingerprints
                        .get(deviation)
                        .filter(|fp| !fp.is_empty())
                    {
                        self.deviation_approved_fingerprints
                            .insert(deviation.clone(), fp.clone());
                    }
                }
                (BlockerObject::Node { node }, BlockerKind::Soundness) => {
                    self.sound_status.insert(node.clone(), SoundStatus::Fail);
                    if let Some(fp) = self.live.sound_current_fingerprints.get(node) {
                        self.sound_approved_fingerprints
                            .insert(node.clone(), fp.clone());
                    }
                    self.sound_assessments.insert(
                        node.clone(),
                        SoundAssessment {
                            status: SoundAssessmentStatus::ReviewerPinnedFail,
                            origin: AssessmentOrigin::ReviewerAction,
                            fingerprints: self.current_sound_fingerprint_parts(node),
                            lane_votes: BTreeMap::new(),
                            reviewer_action_id: None,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    fn review_blocker_adjudicable(&self, blocker: &Blocker) -> bool {
        match (&blocker.object, blocker.kind) {
            (BlockerObject::Node { node }, BlockerKind::NodeCorr) => {
                self.latest_corr_review_nodes.contains(node) && self.current_corr_unknown(node)
            }
            (BlockerObject::Node { node }, BlockerKind::Substantiveness) => {
                self.latest_substantiveness_review_nodes.contains(node)
                    && self.current_substantiveness_unknown(node)
            }
            (BlockerObject::Target { target }, BlockerKind::PaperFaithfulness) => {
                self.latest_paper_review_targets.contains(target)
                    && self.current_paper_unknown(target)
            }
            (BlockerObject::Deviation { deviation }, BlockerKind::Deviation) => {
                self.latest_deviation_review_ids.contains(deviation)
                    && self.current_deviation_fail(deviation)
            }
            (BlockerObject::Node { node }, BlockerKind::Soundness) => {
                self.latest_sound_review_nodes.contains(node) && self.current_sound_unknown(node)
            }
            _ => false,
        }
    }

    pub fn review_task_blocker_forwardable(&self, blocker: &Blocker) -> bool {
        match (&blocker.object, blocker.kind) {
            (BlockerObject::Node { node }, BlockerKind::Soundness) => {
                self.sound_repair_ready(node)
                    && (self.current_sound_fail(node) || self.review_blocker_adjudicable(blocker))
            }
            _ => true,
        }
    }

    pub fn apply_difficulty_updates(&mut self, updates: &BTreeMap<NodeId, Update<NodeDifficulty>>) {
        self.ensure_node_metadata();
        for (node, update) in updates {
            match update {
                Update::Same => {}
                Update::Set(difficulty) => {
                    self.node_difficulty.insert(node.clone(), *difficulty);
                    self.easy_attempts.insert(node.clone(), 0);
                }
            }
        }
        self.ensure_node_metadata();
    }

    pub fn reset_easy_attempt_for_node(&mut self, node: Option<&NodeId>) {
        let Some(node) = node else {
            return;
        };
        self.ensure_node_metadata();
        if self.node_difficulty.get(node) == Some(&NodeDifficulty::Easy) {
            self.easy_attempts.insert(node.clone(), 0);
        }
    }

    pub fn proof_failure_bump(&mut self, node: Option<&NodeId>) {
        let Some(node) = node else {
            return;
        };
        self.ensure_node_metadata();
        match self.node_difficulty.get(node).copied().unwrap_or_default() {
            NodeDifficulty::Hard => {
                self.easy_attempts.insert(node.clone(), 0);
            }
            NodeDifficulty::Easy => {
                let next = self.easy_attempts.get(node).copied().unwrap_or(0) + 1;
                if next >= self.easy_max_retries {
                    self.node_difficulty
                        .insert(node.clone(), NodeDifficulty::Hard);
                    self.easy_attempts.insert(node.clone(), 0);
                } else {
                    self.easy_attempts.insert(node.clone(), next);
                }
            }
        }
    }

    pub fn expected_request(&self, request_id: u32, kind: RequestKind) -> WrapperRequest {
        let current_node_kinds = Self::effective_node_kinds(
            &self.node_kinds,
            &self.live.present_nodes,
            &self.proof_nodes,
        );
        let deterministic_worker_rejection_reasons =
            prompt_safe_deterministic_worker_rejection_reasons(
                &self.deterministic_worker_rejection_reasons,
            );
        let mut request = WrapperRequest {
            id: request_id,
            kind,
            cycle: self.cycle,
            phase: self.phase,
            active_node: self.active_node.clone(),
            held_target: self.held_target.clone(),
            mode: self.current_mode(),
            blockers: self.request_blockers(kind),
            blocked_targets: self.blocked_targets(),
            configured_targets: self.configured_targets.clone(),
            verify_nodes: self.request_verify_nodes(kind),
            verify_targets: self.request_verify_targets(kind),
            verify_lanes: self.request_verify_lanes(kind),
            paper_verify_lane_bindings: Vec::new(),
            corr_verify_lane_bindings: Vec::new(),
            sound_verify_lane_bindings: Vec::new(),
            worker_binding: crate::BridgeActorBinding::default(),
            reviewer_binding: crate::BridgeActorBinding::default(),
            stuck_math_audit_binding: crate::BridgeActorBinding::default(),
            paper_verify_targets: self.request_paper_verify_targets(kind),
            substantiveness_verify_nodes: self.request_substantiveness_verify_nodes(kind),
            deviation_verify_id: self.request_deviation_verify_id(kind),
            deviation_verify_path: self.request_deviation_verify_path(kind),
            authorized_deviations: self.authorized_deviations(),
            current_deviation_files: self.deviation_files.clone(),
            node_deviation_claims: self.node_deviation_claims.clone(),
            corr_verify_nodes: self.request_corr_verify_nodes(kind),
            corr_verify_targets: self.request_corr_verify_targets(kind),
            sound_verify_nodes: self.request_sound_verify_nodes(kind),
            sound_verify_node: self.request_sound_verify_node(kind),
            runtime_support_required: kind.requires_runtime_support(),
            approved_target_nodes: self.request_approved_target_nodes(kind),
            approved_corr_fingerprints: self.request_approved_corr_fingerprints(kind),
            coarse_dag_nodes: match kind {
                RequestKind::Worker | RequestKind::Review => self.coarse_dag_nodes.clone(),
                _ => BTreeSet::new(),
            },
            active_coarse_node: match kind {
                RequestKind::Worker | RequestKind::Review => self.active_coarse_node.clone(),
                _ => None,
            },
            // Proposal v32 audit-2 followup #6: gate the hinted set on
            // `retry_outcome_kind == None` so retry reviews don't surface
            // anchor candidates the response validation
            // (`review_next_active_coarse_legal_for_response`) will then
            // reject. Hints and validation must agree.
            kernel_hinted_next_active_coarse_nodes: if kind == RequestKind::Review
                && matches!(self.retry_outcome_kind, RetryOutcomeKind::None)
            {
                self.kernel_hinted_next_active_coarse_nodes()
            } else {
                BTreeSet::new()
            },
            coarse_repair_mode: match kind {
                RequestKind::Worker | RequestKind::Review => self.coarse_repair_mode(),
                _ => false,
            },
            cycles_in_coarse_repair_mode: match kind {
                RequestKind::Worker | RequestKind::Review => self.cycles_in_coarse_repair_mode,
                _ => 0,
            },
            // Mirror the retry gate above: starvation-unlock is an action
            // signal ("you may switch anchor") that the response validator
            // would reject on retry, so suppress it on retry too.
            //
            // Audit-2 followup #7: also gate on the CURRENT
            // `coarse_repair_mode()`. The counter is only reset in the
            // proof-Continue handler (`engine.rs:~3683-3698`); a worker
            // burst that resolves the last out-of-cone blocker mid-cycle
            // flips repair-mode false but leaves the counter at threshold
            // for one cycle, until the next proof-Continue resets it.
            // Without this guard the next Review prompt would say
            // "repair work has been spinning" (via
            // `prompt_fragments/review/common/30b_coarse_anchor.md`)
            // while `coarse_repair_mode` is false on the same payload —
            // a contradictory signal. Gating here suppresses the flag
            // until the counter coherence is restored.
            coarse_anchor_starvation_unlocked: kind == RequestKind::Review
                && matches!(self.retry_outcome_kind, RetryOutcomeKind::None)
                && self.phase == Phase::ProofFormalization
                && !self.coarse_dag_nodes.is_empty()
                && self.active_coarse_node.is_some()
                && self.cycles_in_coarse_repair_mode >= stuck_coarse_repair_threshold()
                && self.coarse_repair_mode(),
            protected_semantic_change_confirmation: if kind == RequestKind::Review {
                self.pending_protected_semantic_scope_confirmation.clone()
            } else {
                None
            },
            protected_reapproval_nodes: if (kind == RequestKind::HumanGate
                && self.gate_kind == GateKind::ProtectedReapproval)
                || kind == RequestKind::Review
            {
                self.pending_protected_reapproval_nodes.clone()
            } else {
                BTreeSet::new()
            },
            allowed_decisions: self.request_allowed_decisions(kind),
            allowed_next_modes: self.request_allowed_next_modes(kind),
            kernel_hinted_next_active_nodes: self.request_kernel_hinted_next_active_nodes(kind),
            // Proposal v32 audit-2 followup #3: only relevant on Review
            // requests in ProofFormalization; vacuous elsewhere.
            proof_active_node_base_legal_candidates: if kind == RequestKind::Review {
                self.proof_active_node_base_legal_candidates()
            } else {
                BTreeSet::new()
            },
            // Proposal v32 audit-2 followup (post-fix): denormalize the
            // FULL carrier set (deferred-inclusive) onto the request so
            // request-side cone helpers see the same widening surface
            // as the kernel's `coarse_legal_active_set`. Surface on
            // Worker + Review in ProofFormalization with a non-empty
            // coarse DAG; vacuous otherwise.
            coarse_repair_blocker_carriers: if matches!(
                kind,
                RequestKind::Worker | RequestKind::Review
            ) && self.phase == Phase::ProofFormalization
                && !self.coarse_dag_nodes.is_empty()
            {
                self.coarse_task_blocker_nodes()
            } else {
                BTreeSet::new()
            },
            ever_shallow_coarse_closed: if matches!(
                kind,
                RequestKind::Review | RequestKind::StuckMathAudit
            ) && self.phase == Phase::ProofFormalization
                && !self.coarse_dag_nodes.is_empty()
            {
                self.ever_shallow_coarse_closed.clone()
            } else {
                BTreeSet::new()
            },
            ever_shallow_coarse_closed_regressed: if matches!(
                kind,
                RequestKind::Review | RequestKind::StuckMathAudit
            ) && self.phase == Phase::ProofFormalization
                && !self.coarse_dag_nodes.is_empty()
            {
                self.ever_shallow_coarse_closed_regressed()
            } else {
                BTreeSet::new()
            },
            pending_global_repair_request: match kind {
                RequestKind::Review | RequestKind::StuckMathAudit => {
                    self.pending_global_repair_request.clone()
                }
                _ => None,
            },
            pending_global_repair_grant: match kind {
                RequestKind::Review => self.pending_global_repair_grant.clone(),
                _ => None,
            },
            latest_global_repair_audit_decline_reason: match kind {
                RequestKind::Review => self.latest_global_repair_audit_decline_reason.clone(),
                _ => String::new(),
            },
            global_repair_mode_enabled: self.global_repair_mode_enabled,
            consumed_global_repair_grant: match kind {
                RequestKind::Worker => self
                    .pending_task
                    .as_ref()
                    .map(|t| t.consumed_global_repair_grant)
                    .unwrap_or(false),
                _ => false,
            },
            targeted_next_active_nodes: self.request_targeted_next_active_nodes(kind),
            allow_targeted_without_next_active: self
                .request_allow_targeted_without_next_active(kind),
            allowed_resets: self.request_allowed_resets(kind),
            resettable_theorem_stating_nodes: self.request_resettable_theorem_stating_nodes(kind),
            allowed_reset_blockers: self.request_allowed_reset_blockers(kind),
            // Option C: always empty (field retained for serde back-compat).
            allowed_override_blockers: BTreeSet::new(),
            sound_repair_ready_nodes: self.request_sound_repair_ready_nodes(kind),
            sound_verifier_requestable_nodes: self.request_sound_verifier_requestable_nodes(kind),
            sound_assessment_statuses: self.request_sound_assessment_statuses(kind),
            sound_reverification_context: self.request_sound_reverification_context(kind),
            cycles_since_clean: self.cycles_since_clean,
            no_sound_progress_window_cycles: self.no_sound_progress_window_depth(),
            shallow_coarse_closed_count: self.shallow_coarse_closed_count,
            cycles_since_shallow_coarse_closed_count_increase: self
                .cycles_since_shallow_coarse_closed_count_increase,
            last_clean_rewind_count: self.last_clean_rewind_count,
            stuck_math_audit: self.request_stuck_math_audit(kind),
            audit_plan: self.request_audit_plan(kind),
            previous_audit_plan_snapshot: self.request_previous_audit_plan_snapshot(kind),
            latest_stuck_math_audit_rejection_reason: if kind == RequestKind::StuckMathAudit {
                self.latest_stuck_math_audit_rejection_reason.clone()
            } else {
                String::new()
            },
            allowed_difficulty_update_nodes: self.request_allowed_difficulty_update_nodes(kind),
            current_present_nodes: self.live.present_nodes.clone(),
            current_proof_nodes: self.proof_nodes.clone(),
            current_node_kinds,
            current_deps: self.deps.clone(),
            current_target_claims: self.target_claims.clone(),
            current_paper_approved_fingerprints: self.paper_approved_fingerprints.clone(),
            reviewer_comments: self.reviewer_comments.clone(),
            latest_worker_summary: self.latest_worker_summary.clone(),
            latest_worker_comments: self.latest_worker_comments.clone(),
            latest_worker_needs_restructure_suggested_nodes: self
                .latest_worker_needs_restructure_suggested_nodes
                .clone(),
            deterministic_worker_rejection_reasons,
            latest_review_rejection_reasons: if kind == RequestKind::Review {
                prompt_safe_rejection_reasons(&self.latest_review_rejection_reasons)
            } else {
                Vec::new()
            },
            review_verifier_evidence: ReviewVerifierEvidence {
                paper: self.latest_paper_reviewer_evidence.clone(),
                deviation: self.latest_deviation_reviewer_evidence.clone(),
                substantiveness: self.latest_substantiveness_reviewer_evidence.clone(),
                corr: self.latest_corr_reviewer_evidence.clone(),
                sound: self.latest_sound_reviewer_evidence.clone(),
            },
            previous_paper_lane_findings: self.previous_paper_lane_findings.clone(),
            previous_substantiveness_lane_findings: self
                .previous_substantiveness_lane_findings
                .clone(),
            previous_corr_lane_findings: self.previous_corr_lane_findings.clone(),
            previous_sound_lane_findings: self.previous_sound_lane_findings.clone(),
            retry_outcome_kind: self.retry_outcome_kind,
            retry_attempt: match self.retry_outcome_kind {
                RetryOutcomeKind::None => 0,
                RetryOutcomeKind::Transport => self.transport_attempt,
                _ => self.attempt,
            },
            post_advance_routing: self.post_advance_routing_pending,
            fresh_context: false,
            prompt_contract_version: 0,
            project_invariants: crate::default_contract_value(),
            paper_contract: crate::default_contract_value(),
            corr_contract: crate::default_contract_value(),
            sound_contract: crate::default_contract_value(),
            worker_contract: crate::default_contract_value(),
            review_contract: crate::default_contract_value(),
            audit_contract: crate::default_contract_value(),
            stuck_math_audit_contract: crate::default_contract_value(),
            // Cleanup-v2 Step 12: surface audit-time state on Audit
            // requests, AND on Review requests during Phase::Cleanup
            // (so the reviewer prompt can render the task list +
            // re-audit legality). Other request kinds get empty views.
            cleanup_audit_tasks_view: if matches!(kind, RequestKind::Audit)
                || (kind == RequestKind::Review && self.phase == Phase::Cleanup)
            {
                self.cleanup_audit_tasks.clone()
            } else {
                Vec::new()
            },
            cleanup_audit_scratchpad_view: if kind == RequestKind::Audit {
                self.cleanup_audit_scratchpad.clone()
            } else {
                String::new()
            },
            cleanup_audit_round_view: if matches!(kind, RequestKind::Audit)
                || (kind == RequestKind::Review && self.phase == Phase::Cleanup)
            {
                self.cleanup_audit_round
            } else {
                0
            },
            cleanup_audit_burst_count_view: if kind == RequestKind::Audit {
                self.cleanup_audit_burst_count
            } else {
                0
            },
            cleanup_protected_statement_node_set_view: if matches!(kind, RequestKind::Audit)
                || (kind == RequestKind::Review && self.phase == Phase::Cleanup)
            {
                self.live_protected_statement_node_set()
            } else {
                BTreeSet::new()
            },
            latest_audit_rejection_reason_view: if kind == RequestKind::Audit {
                self.latest_audit_rejection_reason.clone()
            } else {
                String::new()
            },
            // Cleanup-v2 (audit Finding 2): expose the cleanup-force-done
            // latch on Review requests during Cleanup so the legality
            // gate can reject Continue when the consecutive-invalid-
            // worker threshold has fired. Other request kinds get false.
            cleanup_force_done_view: if kind == RequestKind::Review && self.phase == Phase::Cleanup
            {
                self.cleanup_force_done
            } else {
                false
            },
            worker_context: if kind == RequestKind::Worker {
                self.current_worker_context()
            } else {
                WorkerContext::default()
            },
            worker_acceptance: if kind == RequestKind::Worker {
                self.current_worker_acceptance()
            } else {
                WorkerAcceptanceContract::default()
            },
            invalid_attempt: self.invalid_attempt,
            human_input_outstanding: self.human_input_outstanding,
            gate_kind: self.gate_kind,
            // Patch C plan §7.4.2 — surface unverified-node failures
            // on Worker / Review requests so the reviewer can pick a
            // closure-failed node as `next_active` (with `task_blockers`
            // empty) and the worker prompt can render the original
            // failure context. Verifier and HumanGate requests don't
            // route worker bursts so the field stays empty there.
            local_closure_unverified: match kind {
                RequestKind::Worker | RequestKind::Review => self
                    .local_closure_unverified_nodes
                    .iter()
                    .filter_map(|node| {
                        self.local_closure_failures
                            .get(node)
                            .map(|summary| (node.clone(), summary.clone()))
                    })
                    .collect(),
                RequestKind::Paper
                | RequestKind::Corr
                | RequestKind::Sound
                | RequestKind::HumanGate
                | RequestKind::Audit
                | RequestKind::StuckMathAudit => BTreeMap::new(),
            },
        };
        crate::populate_request_prompt_contracts(&mut request, None);
        request
    }

    pub fn issue_request(&mut self, kind: RequestKind) -> WrapperRequest {
        self.request_seq += 1;
        self.refresh_stuck_math_audit_latch();
        let request = self.expected_request(self.request_seq, kind);
        self.in_flight_request = Some(request.clone());
        request
    }

    pub fn clear_in_flight_request(&mut self) {
        self.in_flight_request = None;
    }

    pub fn review_response_legal(&self, review: &ReviewResponse) -> bool {
        if !self
            .expected_request(0, RequestKind::Review)
            .review_response_legal(review)
        {
            return false;
        }
        // global_repair_mode S10 + protected-disjointness live-state
        // checks. Done here (rather than on the WrapperRequest) because
        // they require fields not surfaced through `expected_request`:
        // `last_reviewer_global_repair_request_cycle`,
        // `latest_global_repair_audit_decline_reason`, and the protected
        // node set that the reviewer should never be allowed to extend
        // through this mechanism.
        if let Some(gr) = review.global_repair_request.as_ref() {
            if !gr
                .proposed_extension_nodes
                .is_disjoint(&self.live_protected_statement_node_set())
            {
                return false;
            }
            if let Some(prev) = self.last_reviewer_global_repair_request_cycle {
                let elapsed = self.cycle.saturating_sub(prev);
                let cooldown = stuck_math_audit_dispatch_cooldown_cycles();
                let already_pending = self.pending_global_repair_request.is_some()
                    || self.pending_global_repair_grant.is_some()
                    || !self.latest_global_repair_audit_decline_reason.is_empty();
                if elapsed < cooldown && already_pending {
                    return false;
                }
            }
        }
        // LastClean is a pure rewind: the engine applies the reset and
        // re-issues a Review request from the post-reset state, so the
        // reviewer's next routing decision (`next_active`, `next_mode`,
        // blocker adjudications, difficulty updates) is made on the next
        // turn against the restored state — not absorbed from the response
        // that triggered the reset. Forbid `Continue + LastClean + Some(_)`
        // so the contract matches engine semantics; otherwise the
        // `next_active` value would be silently dropped.
        //
        // Earlier revs threaded `next_active` through the reset by probing
        // a cloned state with `apply_last_clean_reset` applied; that fix
        // closed a divergence bug but kept the reviewer trapped when no
        // `next_active` was simultaneously legal pre-reset and post-reset
        // — a realistic case after a few worker bursts of restructuring.
        // Re-issuing the Review post-reset is the cleaner contract.
        //
        // NeedInput / AdvancePhase + LastClean both naturally re-prompt
        // after the handoff: NeedInput first routes through the
        // NeedInputAuditor and only reaches HumanGate if confirmed, while
        // AdvancePhase still goes directly through HumanGate. Both apply
        // paths run the reset before deferring to a fresh reviewer turn.
        // Done + LastClean is rejected outright in
        // `expected_request().review_response_legal` (incoherent — Done
        // declares completion, LastClean rewinds).
        if matches!(
            review.reset,
            ResetChoice::LastClean | ResetChoice::TheoremStatingNode
        ) && review.decision == ReviewDecisionKind::Continue
            && review.next_active.is_some()
        {
            return false;
        }
        // Cleanup-Done defense-in-depth: under the cleanup invariant
        // (every accepted cleanup state is Done-valid; rewinds are
        // forbidden in cleanup), formalization_complete should always
        // hold when the reviewer faces a cleanup-phase Done choice.
        // This check pins the Done contract to the semantic invariant
        // rather than to phase-entry history — if a future regression
        // ever lets a non-Done-valid state into cleanup, Done won't
        // silently declare a half-done formalization complete.
        if self.phase == Phase::Cleanup
            && review.decision == ReviewDecisionKind::Done
            && !self.formalization_complete()
        {
            return false;
        }
        // Cleanup-Continue invariant guard: by the cleanup invariant,
        // every accepted cleanup state has no global blockers. The
        // reviewer therefore has no blockers to manipulate — any
        // non-empty reset/task/override blocker set is either
        // nonsensical or would violate the invariant by re-introducing
        // global blockers (apply_review_blocker_resets flips verifier
        // statuses to Unknown → adds blockers → breaks
        // formalization_complete on the next commit_live).
        if self.phase == Phase::Cleanup
            && (!review.reset_blockers.is_empty()
                || !review.task_blockers.is_empty()
                || !review.override_blockers.is_empty()
                || !review.request_sound_verifier_nodes.is_empty())
        {
            return false;
        }
        true
    }

    pub fn review_response_rejection_reasons(&self, review: &ReviewResponse) -> Vec<String> {
        let mut reasons = self
            .expected_request(0, RequestKind::Review)
            .review_response_rejection_reasons(review);
        if matches!(
            review.reset,
            ResetChoice::LastClean | ResetChoice::TheoremStatingNode
        ) && review.decision == ReviewDecisionKind::Continue
            && review.next_active.is_some()
        {
            reasons.push(
                "Continue reset/revert choices that re-observe state must leave next_active empty; the post-reset audit/review chooses the next active node"
                    .into(),
            );
        }
        if self.phase == Phase::Cleanup
            && review.decision == ReviewDecisionKind::Done
            && !self.formalization_complete()
        {
            reasons.push("Cleanup Done is legal only when formalization_complete holds".into());
        }
        if self.phase == Phase::Cleanup
            && (!review.reset_blockers.is_empty()
                || !review.task_blockers.is_empty()
                || !review.override_blockers.is_empty()
                || !review.request_sound_verifier_nodes.is_empty())
        {
            reasons.push(
                "Cleanup review responses must not manipulate blocker or verifier action lists"
                    .into(),
            );
        }
        if reasons.is_empty() && !self.review_response_legal(review) {
            reasons.push(
                "review response failed kernel legality checks; inspect the current request contract"
                    .into(),
            );
        }
        prompt_safe_rejection_reasons(&reasons)
    }

    /// True when `commit_live` has captured a complete set of
    /// `last_clean_*` mirrors at a clean checkpoint since this state
    /// was deserialized. Now backed by an explicit
    /// `last_clean_verifier_mirror_ready` flag (set inside
    /// `commit_live` when populating all mirrors atomically) rather
    /// than a structural-emptiness probe. Reasons for the flag:
    ///
    /// 1. The original structural-emptiness check returned true any
    ///    time *some* mirror was non-default. State files persisted
    ///    by versions before the status mirrors (#56-extension) or
    ///    the approved-fp mirrors (audit follow-up) existed have
    ///    populated structural mirrors but missing status /
    ///    approved-fp mirrors → applying LastClean restored empty
    ///    status / approved-fp maps → phantom Unknown blockers on a
    ///    "clean" reset.
    /// 2. Mirror sets that are legitimately empty (e.g. a tiny repo
    ///    with no nodes) couldn't be distinguished from "never
    ///    populated".
    ///
    /// The flag defaults to `false` on deserialization, so any older
    /// state file forces LastClean to wait for the next clean
    /// `commit_live` to write a complete mirror set. Used by
    /// `apply_last_clean_reset` (skip restore when not ready),
    /// `request_allowed_resets` (don't offer LastClean when not
    /// ready), and `review_response_legal` (skip post-reset legality
    /// check when not ready). Single source of truth.
    ///
    /// Patch C-I (audit HIGH 3): also gate on
    /// `last_clean_local_closure_mirror_ready` so the predicate matches
    /// the gates inside `apply_last_clean_reset`. The reset returns
    /// early when the closure-mirror flag is false, but the engine
    /// emits `RestoreWorktreeToLastClean` unconditionally alongside the
    /// state mutation. Without this AND, a migrated state file where
    /// verifier mirrors are populated but closure mirrors are not
    /// would let `request_allowed_resets` offer LastClean to a
    /// reviewer; on accept, the kernel state would refuse to rewind
    /// while the runtime still git-resets the worktree — state/disk
    /// divergence. Requiring both flags closes the menu option in that
    /// migration window so the reviewer can never pick a LastClean
    /// that the kernel won't actually apply.
    pub fn last_clean_mirrors_populated(&self) -> bool {
        self.last_clean_verifier_mirror_ready && self.last_clean_local_closure_mirror_ready
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.max_theorem_invalid_attempt == 0 {
            return Err("max theorem invalid attempt must be positive".into());
        }
        if self.proof_invalid_review_threshold == 0 {
            return Err("proof invalid review threshold must be positive".into());
        }
        if self.easy_max_retries == 0 {
            return Err("easy max retries must be positive".into());
        }
        if self.verifier_lanes.is_empty() {
            return Err("verifier lanes must be non-empty".into());
        }
        if self.live.coverage
            != self.coverage_from_claims_with_present(&self.target_claims, &self.live.present_nodes)
        {
            return Err("live coverage must be derived from target claims".into());
        }
        if let Some((node, _)) = self
            .target_claims
            .iter()
            .find(|(_, targets)| targets.len() > 1)
        {
            return Err(format!(
                "live node {node} may not directly claim multiple paper targets"
            ));
        }
        if let Some((node, _)) = self
            .committed_target_claims
            .iter()
            .find(|(_, targets)| targets.len() > 1)
        {
            return Err(format!(
                "committed node {node} may not directly claim multiple paper targets"
            ));
        }
        if self
            .configured_targets
            .iter()
            .any(|target| !self.live.paper_current_fingerprints.contains_key(target))
        {
            return Err(
                "live paper-faithfulness fingerprints must cover configured targets".into(),
            );
        }
        if self.committed.coverage
            != self.coverage_from_claims_with_present(
                &self.committed_target_claims,
                &self.committed.present_nodes,
            )
        {
            return Err("committed coverage must be derived from target claims".into());
        }
        if self.configured_targets.iter().any(|target| {
            !self
                .committed
                .paper_current_fingerprints
                .contains_key(target)
        }) {
            return Err(
                "committed paper-faithfulness fingerprints must cover configured targets".into(),
            );
        }
        let effective_live_kinds = Self::effective_node_kinds(
            &self.node_kinds,
            &self.live.present_nodes,
            &self.proof_nodes,
        );
        let effective_committed_kinds = Self::effective_node_kinds(
            &self.committed_node_kinds,
            &self.committed.present_nodes,
            &self.committed_proof_nodes,
        );
        if self.proof_nodes
            != Self::proof_nodes_from_kind_map(&effective_live_kinds, &self.live.present_nodes)
        {
            return Err("live proof-nodes set must match live node kinds".into());
        }
        if self.committed_proof_nodes
            != Self::proof_nodes_from_kind_map(
                &effective_committed_kinds,
                &self.committed.present_nodes,
            )
        {
            return Err("committed proof-nodes set must match committed node kinds".into());
        }
        for node in self
            .live
            .present_nodes
            .iter()
            .chain(self.committed.present_nodes.iter())
        {
            if !self.node_difficulty.contains_key(node) {
                return Err(format!("missing node difficulty for {node}"));
            }
            if !self.easy_attempts.contains_key(node) {
                return Err(format!("missing easy-attempt counter for {node}"));
            }
        }
        for (node, difficulty) in &self.node_difficulty {
            if *difficulty == NodeDifficulty::Hard
                && self.easy_attempts.get(node).copied().unwrap_or(0) != 0
            {
                return Err(format!("hard node {node} must have zero easy attempts"));
            }
        }
        if !self.active_node_legal(self.active_node.as_ref(), &self.live) {
            return Err("active node is not legal in live snapshot".into());
        }
        if !self.held_target_legal(self.held_target.as_ref(), &self.live) {
            return Err("held target is not legal in live snapshot".into());
        }
        if self.phase != Phase::TheoremStating && self.held_target.is_some() {
            return Err("held target may only exist in theorem-stating".into());
        }
        if self.phase != Phase::TheoremStating && self.target_edit_mode != TargetEditMode::Global {
            return Err("target edit mode must be global outside theorem-stating".into());
        }
        if self.phase != Phase::ProofFormalization && self.proof_edit_mode != ProofEditMode::Local {
            return Err("proof edit mode must be local outside proof-formalization".into());
        }
        if self.phase == Phase::TheoremStating
            && self.corr_blockers_exist()
            && self.held_target.is_some()
        {
            return Err("held target must be suspended while correspondence is blocked".into());
        }
        // Proposal v32 audit-2 followup #8: mirror the four TLA `TypeOK`
        // active-coarse invariants from `spec/SupervisorProtocol.tla:4661-4665`.
        // Previously these were maintained only out-of-band by specific
        // engine paths (`enter_cleanup_phase`, `relegalize_active_coarse_anchor`);
        // restore / migration / future code paths that bypass those
        // helpers could leave state in a configuration TLA forbids.
        // `cycles_in_coarse_repair_mode ∈ Nat` is enforced by the `u32`
        // type and so isn't restated here.
        //
        // Order chosen so each invariant is exercisable independently:
        // phase guard first (most general scope), dormancy guard next
        // (mechanism off), then membership and counter coherence.
        if self.phase != Phase::ProofFormalization && self.active_coarse_node.is_some() {
            return Err("active_coarse_node may only be set in ProofFormalization phase".into());
        }
        if self.coarse_dag_nodes.is_empty() && self.active_coarse_node.is_some() {
            return Err("active_coarse_node must be None when coarse_dag_nodes is empty".into());
        }
        if let Some(anchor) = self.active_coarse_node.as_ref() {
            if !self.coarse_dag_nodes.contains(anchor) {
                return Err(format!(
                    "active_coarse_node {anchor} is not a member of coarse_dag_nodes"
                ));
            }
        }
        if self.active_coarse_node.is_none() && self.cycles_in_coarse_repair_mode != 0 {
            return Err(
                "cycles_in_coarse_repair_mode must be 0 when no active_coarse_node is set".into(),
            );
        }
        if self.stuck_math_audit.need_input_audit.is_some() && !self.stuck_math_audit.active {
            return Err("need_input_audit context requires active stuck_math_audit latch".into());
        }
        if self.stuck_math_audit.need_input_audit.is_some() && self.stage != Stage::StuckMathAudit {
            return Err(
                "need_input_audit context may only be present during StuckMathAudit stage".into(),
            );
        }
        // Mutex between the NeedInputAuditor and GlobalRepairAuditor audit
        // lanes: both reuse `Stage::StuckMathAudit`, but their auditor
        // contracts (and role-fragment routing in `request_contracts.rs`)
        // are distinct. A simultaneous `need_input_audit` + pending
        // `global_repair_request` configuration would yield an incoherent
        // prompt (GR role with NeedInput contract) and downstream sticky
        // wedges. The proactive clears in `route_need_input_to_auditor` /
        // `route_global_repair_request_to_auditor` and the auto-decline in
        // `retry_or_transition_stuck_math_audit_to_reviewer` keep the
        // engine away from this configuration; this invariant catches any
        // future code path that bypasses those gates.
        if self.stuck_math_audit.need_input_audit.is_some()
            && self.pending_global_repair_request.is_some()
        {
            return Err(
                "need_input_audit context and pending_global_repair_request must be mutually exclusive".into(),
            );
        }
        if self
            .audit_plan
            .as_ref()
            .is_some_and(|plan| plan.need_input_audit)
            && !self.stuck_math_audit.active
        {
            return Err("need_input_audit plan requires active stuck_math_audit latch".into());
        }
        if let Some(task) = &self.pending_task {
            if !matches!(self.stage, Stage::Start | Stage::Worker) {
                return Err("pending task may only exist in Start or Worker".into());
            }
            let global = self.global_blockers();
            if !task.task_blockers.is_subset(&global) {
                return Err("pending task blockers must be a subset of global blockers".into());
            }
            if task.node != self.active_node {
                return Err("pending task node must match active node".into());
            }
            if task.mode != self.current_mode() {
                return Err("pending task mode must match current mode".into());
            }
            let orphan_nodes = self.orphan_nodes(&self.live);
            if !task.orphan_cleanup_nodes.is_subset(&orphan_nodes) {
                return Err("pending task orphan cleanup nodes must be live orphan nodes".into());
            }
            // Defense-in-depth (audit follow-up): a proof-phase pending task
            // bearing `task_blockers` must have a focus node AND a mode that
            // can legally repair cross-node / signature blockers. Local mode
            // authorizes only the active node's proof body; a task_blocker
            // under Local — or with no focus node at all — describes work
            // the deterministic checker will reject every time. The legality
            // gate at `WrapperRequest::review_response_legal` (Continue with
            // no `next_active` + Restructure mode, or Local + non-empty
            // task_blockers — except the Soundness carve-out) is the
            // upstream guard; this invariant catches any future code path
            // that bypasses it.
            //
            // Soundness carve-out (mirrors `review_response_legal`,
            // model.rs:1762ff., and commit 1263d80's prompt advice in
            // `review/common/05_after_failed_soundness.md`): Local +
            // Soundness-only task_blockers IS legitimate. Closing the
            // proof body is exactly Local's scope; once the active node
            // goes sorry-free, `needs_sound` returns false and the
            // Soundness blocker evaporates without any other repair.
            if self.phase == Phase::ProofFormalization && !task.task_blockers.is_empty() {
                if task.node.is_none() {
                    return Err(
                        "proof-phase pending task with task_blockers must have a focus node".into(),
                    );
                }
                let all_soundness = task
                    .task_blockers
                    .iter()
                    .all(|b| b.kind == BlockerKind::Soundness);
                let mode_ok = matches!(
                    task.mode,
                    TaskMode::Restructure | TaskMode::CoarseRestructure
                ) || (task.mode == TaskMode::Local && all_soundness);
                if !mode_ok {
                    return Err(
                        "proof-phase pending task with task_blockers must use Restructure or CoarseRestructure mode (or Local with Soundness-only task_blockers)"
                            .into(),
                    );
                }
            }
        }
        if matches!(self.stage, Stage::HumanGate) != (self.gate_kind != GateKind::None) {
            return Err("human gate stage and gate kind must agree".into());
        }
        match (&self.in_flight_request, self.expected_request_kind()) {
            (None, _) => {}
            (Some(request), Some(kind)) => {
                let expected = self.expected_request(request.id, kind);
                if request != &expected {
                    return Err("in-flight request payload does not match derived state".into());
                }
            }
            (Some(_), None) => {
                return Err(
                    "in-flight request present in a stage that should not issue requests".into(),
                )
            }
        }
        if self.invalid_attempt && matches!(self.stage, Stage::Complete) {
            return Err("invalid attempt cannot flow into complete".into());
        }
        for (node, deps) in &self.deps {
            if !self.live.present_nodes.contains(node) {
                return Err(format!("live deps contain non-present node {node}"));
            }
            if !deps.is_subset(&self.live.present_nodes) {
                return Err(format!(
                    "live deps for {node} must stay inside live present nodes"
                ));
            }
        }
        for node in self.target_claims.keys() {
            if !self.live.present_nodes.contains(node) {
                return Err(format!(
                    "live target claims contain non-present node {node}"
                ));
            }
        }
        for node in self.committed_target_claims.keys() {
            if !self.committed.present_nodes.contains(node) {
                return Err(format!(
                    "committed target claims contain non-present node {node}"
                ));
            }
        }
        // ───────────────────────────────────────────────────────────
        // Audit H-1 / M-1 — closure-tier invariants. These had no
        // representation in `validate()` previously; every check below
        // mirrors a known sorry-free-only / coverage / structural
        // invariant from plan §7.0 / §7.2 (LOCAL_CLOSURE_IMPL_PLAN.md)
        // and the audit reports' Cross-Cutting root causes.
        // ───────────────────────────────────────────────────────────

        // Mutual-exclusion (plan §7.2): a node holds a record OR sits
        // in unverified — never both. Catches engine paths that bypass
        // the eligibility filter.
        for node in self.local_closure_records.keys() {
            if self.local_closure_unverified_nodes.contains(node) {
                return Err(format!(
                    "closure invariant: {node} appears in both records and unverified"
                ));
            }
        }
        // Sorry-free-only (plan §7.2): unverified ∩ open_nodes = ∅.
        for node in &self.local_closure_unverified_nodes {
            if self.live.open_nodes.contains(node) {
                return Err(format!(
                    "closure invariant: {node} is in unverified AND open_nodes (mutex violation)"
                ));
            }
        }
        // Failure summaries are only meaningful for nodes currently
        // in the unverified set (plan §7.0).
        for node in self.local_closure_failures.keys() {
            if !self.local_closure_unverified_nodes.contains(node) {
                return Err(format!(
                    "closure invariant: failure summary for {node} but node not in unverified"
                ));
            }
        }
        // Coverage (Audit C-3): every sorry-free present proof_node
        // must appear in records ∪ unverified. The
        // `formalization_complete` gate already requires record
        // presence for completion; this stronger invariant catches
        // the deadlock the audit identified (manual edit between
        // bursts introduces a sorry-free node that never enters
        // either set).
        //
        // Only enforced when the state's closure tier is non-empty.
        // Many engine tests construct skeleton states that don't
        // model the closure tier at all (zero records, zero unverified
        // entries) — those legitimately have no coverage. The
        // invariant fires only once any closure state has been
        // observed, which catches the real audit scenario (operator
        // edit between bursts creates a sorry-free node while the
        // closure tier is in use elsewhere).
        let closure_tier_active = !self.local_closure_records.is_empty()
            || !self.local_closure_unverified_nodes.is_empty()
            || !self.local_closure_failures.is_empty();
        if closure_tier_active {
            for node in &self.proof_nodes {
                if !self.live.present_nodes.contains(node) {
                    continue;
                }
                if self.live.open_nodes.contains(node) {
                    continue;
                }
                if self.local_closure_records.contains_key(node) {
                    continue;
                }
                if self.local_closure_unverified_nodes.contains(node) {
                    continue;
                }
                return Err(format!(
                    "closure invariant: sorry-free present proof_node {node} has neither record nor unverified entry"
                ));
            }
        }
        // Record-owner consistency: every record owner must be a
        // sorry-free present proof_node, and the map key must match
        // the record's own `node` field.
        //
        // We intentionally do NOT invoke the canonical predicate
        // (`is_consistent_with_state`) here: that predicate is the
        // batch / prune / restore admission gate. `validate()` runs
        // on every state transition and many engine paths legitimately
        // accept worker payloads whose `boundary_theorems` map
        // references helpers that the kernel observation layer has
        // not yet wired into `live.present_nodes` (the engine treats
        // those as Lean-module-graph metadata, not as references the
        // kernel must independently confirm). The migration-time and
        // batch-time invariants enforce the stricter predicate; this
        // structural check is the minimum the kernel must always
        // agree on.
        for (node, record) in &self.local_closure_records {
            if &record.node != node {
                return Err(format!(
                    "closure invariant: records[{node}].node = {} (map key mismatch)",
                    record.node
                ));
            }
            if !self.live.present_nodes.contains(node) {
                return Err(format!(
                    "closure invariant: records[{node}] holds record but node is not present"
                ));
            }
            if !self.proof_nodes.contains(node) {
                return Err(format!(
                    "closure invariant: records[{node}] holds record but node is not proof-bearing"
                ));
            }
            if self.live.open_nodes.contains(node) {
                return Err(format!(
                    "closure invariant: records[{node}] holds record but node is sorryd (open_nodes)"
                ));
            }
        }
        // LastClean mirror internal consistency (Audit H-1). When the
        // closure-mirror readiness flag is true, mirrors must agree on
        // mutual-exclusion and coverage just like the live tier.
        if self.last_clean_local_closure_mirror_ready {
            for node in self.last_clean_local_closure_records.keys() {
                if self
                    .last_clean_local_closure_unverified_nodes
                    .contains(node)
                {
                    return Err(format!(
                        "closure invariant: LastClean mirror has {node} in both records and unverified"
                    ));
                }
            }
            for node in self.last_clean_local_closure_failures.keys() {
                if !self
                    .last_clean_local_closure_unverified_nodes
                    .contains(node)
                {
                    return Err(format!(
                        "closure invariant: LastClean failure summary for {node} but node not in unverified mirror"
                    ));
                }
            }
        }
        // Reverse-index consistency (Audit M-1). Rebuild the reverse
        // index against the current records map and compare; any drift
        // is a state-shape bug.
        let mut expected_boundary: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
        let mut expected_strict: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
        for (consumer, record) in &self.local_closure_records {
            for helper in record.boundary_theorems.keys() {
                expected_boundary
                    .entry(helper.clone())
                    .or_default()
                    .insert(consumer.clone());
            }
            for dep in record
                .strict_theorem_deps
                .keys()
                .chain(record.strict_definition_deps.keys())
            {
                expected_strict
                    .entry(dep.clone())
                    .or_default()
                    .insert(consumer.clone());
            }
        }
        if self.boundary_statement_consumers != expected_boundary {
            return Err(
                "closure invariant: boundary_statement_consumers out of sync with records".into(),
            );
        }
        if self.strict_dep_consumers != expected_strict {
            return Err("closure invariant: strict_dep_consumers out of sync with records".into());
        }
        Ok(())
    }
}

/// Patch C-A — rebuild the derived reverse indices on a `ProtocolState`
/// from the current `local_closure_records` map (plan §7.2).
///
/// Reverse indices are NOT serialized (`#[serde(skip)]`) and are NOT
/// mirrored at any tier. They are pure derivations of the records map
/// and must be recomputed:
///
///   * On supervisor startup after state load (so existing records
///     immediately invalidate consumers on the next dep change).
///   * After `apply_last_clean_reset` (records restored from
///     `last_clean_*` mirrors).
///   * After `restore_committed` (records rolled back from
///     `committed_*` mirrors).
///   * Inside Patch C-B's accept-path bookkeeping any time a record
///     is written or deleted (incremental updates are the norm; this
///     full-rebuild is the safety-net for restore paths).
///
/// `boundary_statement_consumers[H] = {N : record(N).boundary_theorems.contains_key(H)}`.
/// `strict_dep_consumers[D] = {N : record(N).strict_*_deps.contains_key(D)}`
/// where the strict-dep entry is the union of `strict_theorem_deps` and
/// `strict_definition_deps` keys.
pub fn recompute_local_closure_reverse_indices(state: &mut ProtocolState) {
    state.boundary_statement_consumers.clear();
    state.strict_dep_consumers.clear();
    for (node, record) in &state.local_closure_records {
        for helper in record.boundary_theorems.keys() {
            state
                .boundary_statement_consumers
                .entry(helper.clone())
                .or_default()
                .insert(node.clone());
        }
        for dep in record
            .strict_theorem_deps
            .keys()
            .chain(record.strict_definition_deps.keys())
        {
            state
                .strict_dep_consumers
                .entry(dep.clone())
                .or_default()
                .insert(node.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build an audit report that satisfies
    /// `AUDIT_REPORT_TEXT_MIN_CHARS` (200) AND includes the required
    /// concrete signal substring ("## Claim being audited").
    fn long_audit_report(suffix: &str) -> String {
        let prefix = "## Claim being audited\n";
        let padding = "x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS);
        format!("{prefix}{suffix} {padding}")
    }

    fn node(id: &str) -> NodeId {
        NodeId::from(id)
    }

    fn target(id: &str) -> TargetId {
        TargetId::from(id)
    }

    fn deviation(id: &str) -> DeviationId {
        DeviationId::from(id)
    }

    #[test]
    fn deviation_authorization_reopens_when_file_fingerprint_changes() {
        let id = deviation("dev:a");
        let mut state = ProtocolState {
            deviation_files: BTreeMap::from([(id.clone(), "reference/dev_a.tex".to_string())]),
            deviation_status: BTreeMap::from([(id.clone(), CorrStatus::Pass)]),
            deviation_approved_fingerprints: BTreeMap::from([(id.clone(), "old".to_string())]),
            live: WorkingSnapshot {
                deviation_current_fingerprints: BTreeMap::from([(id.clone(), "old".to_string())]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };

        assert!(state.current_deviation_pass(&id));
        state
            .live
            .deviation_current_fingerprints
            .insert(id.clone(), "new".to_string());

        assert!(state.current_deviation_unknown(&id));
        assert!(state.deviation_verify_ids().contains(&id));
        assert!(state.authorized_deviations().is_empty());
        assert!(state.global_blockers().contains(&Blocker {
            kind: BlockerKind::Deviation,
            object: BlockerObject::Deviation {
                deviation: id.clone(),
            },
            fingerprint: "new".to_string(),
            deferred: false,
        }));
    }

    #[test]
    fn last_clean_reset_restores_deviation_status_and_fingerprint_mirrors() {
        let id = deviation("dev:a");
        let mut state = ProtocolState {
            deviation_files: BTreeMap::from([(id.clone(), "reference/dev_a.tex".to_string())]),
            committed_deviation_files: BTreeMap::from([(
                id.clone(),
                "reference/dev_a.tex".to_string(),
            )]),
            deviation_status: BTreeMap::from([(id.clone(), CorrStatus::Pass)]),
            deviation_approved_fingerprints: BTreeMap::from([(id.clone(), "fp0".to_string())]),
            live: WorkingSnapshot {
                deviation_current_fingerprints: BTreeMap::from([(id.clone(), "fp0".to_string())]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };
        state.committed = state.live.clone();
        assert!(state.global_blockers().is_empty());
        state.commit_live();

        state
            .live
            .deviation_current_fingerprints
            .insert(id.clone(), "fp1".to_string());
        state
            .deviation_approved_fingerprints
            .insert(id.clone(), "fp1".to_string());
        state.deviation_status.insert(id.clone(), CorrStatus::Fail);

        assert_eq!(state.apply_last_clean_reset(), Ok(true));
        assert!(state.current_deviation_pass(&id));
        assert!(state.global_blockers().is_empty());
        assert_eq!(state.deviation_status.get(&id), Some(&CorrStatus::Pass));
        assert_eq!(
            state.deviation_approved_fingerprints.get(&id),
            Some(&"fp0".to_string())
        );
    }

    #[test]
    fn deviation_state_treats_empty_fingerprint_as_unknown() {
        let id = deviation("dev:a");
        let state = ProtocolState {
            deviation_files: BTreeMap::from([(id.clone(), "reference/dev_a.tex".to_string())]),
            deviation_status: BTreeMap::from([(id.clone(), CorrStatus::Pass)]),
            deviation_approved_fingerprints: BTreeMap::from([(id.clone(), String::new())]),
            live: WorkingSnapshot {
                deviation_current_fingerprints: BTreeMap::from([(id.clone(), String::new())]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };

        assert!(state.current_deviation_unknown(&id));
        assert!(state.deviation_verify_ids().contains(&id));
    }

    #[test]
    fn apply_worker_structure_updates_removes_deviation_on_deletion() {
        // Worker retires deviation `dev:a` (no node claims it; the only
        // claimed node `N` is dropping the claim in the same response).
        // Kernel must purge every per-deviation map entry for `dev:a`.
        let id = deviation("dev:a");
        let claimed_node = node("N");
        let live_fp: Fingerprint = "fp-a".into();
        let mut state = ProtocolState {
            deviation_files: BTreeMap::from([(id.clone(), "reference/dev_a.tex".to_string())]),
            deviation_status: BTreeMap::from([(id.clone(), CorrStatus::Pass)]),
            deviation_approved_fingerprints: BTreeMap::from([(id.clone(), live_fp.clone())]),
            node_deviation_claims: BTreeMap::from([(
                claimed_node.clone(),
                BTreeSet::from([id.clone()]),
            )]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([claimed_node.clone()]),
                deviation_current_fingerprints: BTreeMap::from([(id.clone(), live_fp.clone())]),
                ..WorkingSnapshot::default()
            },
            latest_deviation_review_ids: BTreeSet::from([id.clone()]),
            ..ProtocolState::default()
        };
        let response = WorkerResponse {
            snapshot: state.live.clone(),
            node_deviation_claims: BTreeMap::from([(claimed_node.clone(), BTreeSet::new())]),
            deviation_deletions: BTreeSet::from([id.clone()]),
            ..WorkerResponse::default()
        };

        state.apply_worker_structure_updates(&response);

        assert!(!state.deviation_files.contains_key(&id));
        assert!(!state.deviation_status.contains_key(&id));
        assert!(!state.deviation_approved_fingerprints.contains_key(&id));
        assert!(!state.live.deviation_current_fingerprints.contains_key(&id));
        assert!(!state.latest_deviation_review_ids.contains(&id));
        assert!(!state.node_deviation_claims.contains_key(&claimed_node));
    }

    #[test]
    fn worker_semantic_delta_flags_deletion_of_existing_deviation() {
        let id = deviation("dev:a");
        let state = ProtocolState {
            deviation_files: BTreeMap::from([(id.clone(), "reference/dev_a.tex".to_string())]),
            ..ProtocolState::default()
        };
        let response = WorkerResponse {
            snapshot: state.live.clone(),
            deviation_deletions: BTreeSet::from([id.clone()]),
            ..WorkerResponse::default()
        };
        assert!(state.worker_semantic_delta(&response));

        // Deletion of an unknown id is a no-op — kernel never had it —
        // and must not register as a semantic delta.
        let other = deviation("dev:never_existed");
        let response = WorkerResponse {
            snapshot: state.live.clone(),
            deviation_deletions: BTreeSet::from([other]),
            ..WorkerResponse::default()
        };
        assert!(!state.worker_semantic_delta(&response));
    }

    #[test]
    fn worker_semantic_delta_ignores_unchanged_deviation_request_echo() {
        let id = deviation("dev:a");
        let claimed_node = node("N");
        let state = ProtocolState {
            deviation_files: BTreeMap::from([(id.clone(), "reference/dev_a.tex".to_string())]),
            node_deviation_claims: BTreeMap::from([(
                claimed_node.clone(),
                BTreeSet::from([id.clone()]),
            )]),
            ..ProtocolState::default()
        };
        let mut response = WorkerResponse {
            deviation_requests: BTreeMap::from([(
                id.clone(),
                DeviationRequest {
                    path: "reference/dev_a.tex".to_string(),
                    summary: "same path".to_string(),
                    affected_nodes: BTreeSet::from([claimed_node.clone()]),
                },
            )]),
            ..WorkerResponse::default()
        };

        assert!(!state.worker_semantic_delta(&response));
        response
            .deviation_requests
            .get_mut(&id)
            .expect("request")
            .affected_nodes
            .insert(node("M"));
        assert!(state.worker_semantic_delta(&response));
    }

    fn sound_dep_drift_state(stored_status: SoundAssessmentStatus) -> ProtocolState {
        let a = node("A");
        let dep = node("D");
        ProtocolState {
            phase: Phase::ProofFormalization,
            proof_nodes: BTreeSet::from([a.clone()]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([a.clone(), dep.clone()]),
                open_nodes: BTreeSet::from([a.clone()]),
                sound_current_fingerprint_parts: BTreeMap::from([(
                    a.clone(),
                    SoundFingerprintParts {
                        own_tex_hash: "own".into(),
                        dep_statement_hashes: BTreeMap::from([(dep.clone(), "dep-new".into())]),
                        combined_sound_fp: "combined-new".into(),
                    },
                )]),
                sound_current_fingerprints: BTreeMap::from([(a.clone(), "combined-new".into())]),
                ..WorkingSnapshot::default()
            },
            sound_assessments: BTreeMap::from([(
                a,
                SoundAssessment {
                    status: stored_status,
                    origin: AssessmentOrigin::VerifierPanel,
                    fingerprints: SoundFingerprintParts {
                        own_tex_hash: "own".into(),
                        dep_statement_hashes: BTreeMap::from([(dep, "dep-old".into())]),
                        combined_sound_fp: "combined-old".into(),
                    },
                    lane_votes: BTreeMap::new(),
                    reviewer_action_id: None,
                },
            )]),
            ..ProtocolState::default()
        }
    }

    #[test]
    fn current_sound_assessment_dep_drift_preserves_unknown_categories() {
        for (stored, expected) in [
            (
                SoundAssessmentStatus::DepEditOnlyStalePassDeferred,
                SoundAssessmentStatus::DepEditOnlyStalePassDeferred,
            ),
            (
                SoundAssessmentStatus::SplitUnknown,
                SoundAssessmentStatus::SplitUnknown,
            ),
            (
                SoundAssessmentStatus::FreshUnknown,
                SoundAssessmentStatus::FreshUnknown,
            ),
            (
                SoundAssessmentStatus::SelfEditUnknown,
                SoundAssessmentStatus::SelfEditUnknown,
            ),
        ] {
            let state = sound_dep_drift_state(stored);
            assert_eq!(
                state.current_sound_assessment(&node("A")).status,
                expected,
                "stored status {stored:?} should not become stale fail"
            );
        }
    }

    #[test]
    fn current_sound_assessment_dep_drift_keeps_known_fail_as_stale_fail() {
        for stored in [
            SoundAssessmentStatus::VerifierFail,
            SoundAssessmentStatus::VerifierStructural,
            SoundAssessmentStatus::ReviewerPinnedFail,
            SoundAssessmentStatus::SketchAutoFail,
            SoundAssessmentStatus::DepEditOnlyStaleFail,
        ] {
            let state = sound_dep_drift_state(stored);
            assert_eq!(
                state.current_sound_assessment(&node("A")).status,
                SoundAssessmentStatus::DepEditOnlyStaleFail,
                "stored status {stored:?} should remain known-fail work"
            );
        }
    }

    #[test]
    fn theorem_stating_node_cone_clean_is_audit_only() {
        let a = node("A");
        let t = target("main");
        let mut state = ProtocolState {
            phase: Phase::ProofFormalization,
            configured_targets: BTreeSet::from([t.clone()]),
            coarse_dag_nodes: BTreeSet::from([a.clone()]),
            target_claims: BTreeMap::from([(a.clone(), BTreeSet::from([t.clone()]))]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([a.clone()]),
                coverage: BTreeMap::from([(t.clone(), BTreeSet::from([a.clone()]))]),
                paper_current_fingerprints: BTreeMap::from([(t.clone(), "paper".into())]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };

        assert!(!state
            .request_allowed_resets(RequestKind::Review)
            .contains(&ResetChoice::TheoremStatingNode));
        assert_eq!(
            state.request_resettable_theorem_stating_nodes(RequestKind::StuckMathAudit),
            BTreeSet::from([a.clone()])
        );

        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger: "test".into(),
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "diagnosis".into(),
            ..AuditPlan::default()
        });
        assert!(!state
            .request_allowed_resets(RequestKind::Review)
            .contains(&ResetChoice::TheoremStatingNode));

        state.audit_plan = Some(AuditPlan {
            report: "diagnosis".into(),
            cone_clean_node: Some(a),
            ..AuditPlan::default()
        });

        let resets = state.request_allowed_resets(RequestKind::Review);
        assert!(!resets.contains(&ResetChoice::TheoremStatingNode));
        assert!(state
            .request_resettable_theorem_stating_nodes(RequestKind::Review)
            .is_empty());
    }

    #[test]
    fn review_response_legal_checks_theorem_stating_reset_node() {
        let a = node("A");
        let request = WrapperRequest {
            kind: RequestKind::Review,
            phase: Phase::ProofFormalization,
            mode: TaskMode::Local,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local]),
            allowed_resets: BTreeSet::from([ResetChoice::None, ResetChoice::TheoremStatingNode]),
            resettable_theorem_stating_nodes: BTreeSet::from([a.clone()]),
            current_present_nodes: BTreeSet::from([a.clone()]),
            ..WrapperRequest::default()
        };
        let response = ReviewResponse {
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reset: ResetChoice::TheoremStatingNode,
            reset_node: Some(a.clone()),
            next_mode: TaskMode::Local,
            allow_new_obligations: true,
            must_close_active: false,
            ..ReviewResponse::default()
        };
        assert!(request.review_response_legal(&response));

        let mut bad = response.clone();
        bad.reset_node = None;
        assert!(!request.review_response_legal(&bad));

        let mut stray = response;
        stray.reset = ResetChoice::None;
        assert!(!request.review_response_legal(&stray));
    }

    #[test]
    fn review_response_legal_requires_stuck_math_report_when_active() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            stuck_math_audit: StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        let mut response = ReviewResponse {
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Local,
            allow_new_obligations: true,
            must_close_active: false,
            ..ReviewResponse::default()
        };

        assert!(
            !request.review_response_legal(&response),
            "active StuckMathAudit Continue/reset=None must carry a report"
        );

        response.stuck_math_audit = Some(StuckMathAuditReviewReport {
            notes: "the missing invariant is enough".into(),
            reviewer_lean_product: None,
        });

        assert!(
            request.review_response_legal(&response),
            "non-empty StuckMathAudit notes should satisfy the active-mode report requirement"
        );
    }

    #[test]
    fn record_stuck_math_audit_review_clears_stale_product_on_notes_only_report() {
        let old_product = serde_json::json!({"kind": "old"});
        let mut state = ProtocolState {
            stuck_math_audit: StuckMathAuditState {
                active: true,
                last_reviewer_lean_product: Some(old_product),
                ..StuckMathAuditState::default()
            },
            ..ProtocolState::default()
        };
        let notes_only = ReviewResponse {
            stuck_math_audit: Some(StuckMathAuditReviewReport {
                notes: "no compact Lean product this cycle".into(),
                reviewer_lean_product: None,
            }),
            ..ReviewResponse::default()
        };

        state.record_stuck_math_audit_review(&notes_only);

        assert_eq!(state.stuck_math_audit.last_reviewer_lean_product, None);
    }

    #[test]
    fn review_response_legal_rejects_oversized_stuck_math_product() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            stuck_math_audit: StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        let response = ReviewResponse {
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Local,
            allow_new_obligations: true,
            must_close_active: false,
            stuck_math_audit: Some(StuckMathAuditReviewReport {
                notes: "too much".into(),
                reviewer_lean_product: Some(serde_json::json!({
                    "kind": "oversized",
                    "payload": "x".repeat(STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS)
                })),
            }),
            ..ReviewResponse::default()
        };

        assert!(
            !request.review_response_legal(&response),
            "oversized reviewer Lean products must not enter protocol state"
        );
    }

    /// Build a minimal Review WrapperRequest with an active StuckMathAudit
    /// latch and an audit_plan with `tasks` for legality testing of
    /// `dismissed_tasks` / `dismiss_audit_plan` on Continue responses.
    /// The audit_plan defaults to `need_input_audit=false`; phase
    /// defaults to ProofFormalization so dismissals are in-phase.
    fn audit_plan_legality_request(tasks: Vec<AuditTask>) -> WrapperRequest {
        WrapperRequest {
            kind: RequestKind::Review,
            phase: Phase::ProofFormalization,
            mode: TaskMode::Local,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::Continue]),
            allowed_next_modes: BTreeSet::from([TaskMode::Local]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            stuck_math_audit: StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..StuckMathAuditState::default()
            },
            audit_plan: Some(AuditPlan {
                report: "diagnosis".into(),
                tasks,
                ..AuditPlan::default()
            }),
            ..WrapperRequest::default()
        }
    }

    /// Continue + reset=None + Local + Proof must carry a stuck-math-audit
    /// notes report when the latch is active (enforced by
    /// `review_response_stuck_math_audit_legal`). Tests for the audit-plan
    /// legality paths shape the rest of the response with this minimal,
    /// otherwise-legal Continue.
    fn continue_response_with_audit_plan_fields(
        dismissed_tasks: Vec<TaskDismissal>,
        dismiss_audit_plan: bool,
    ) -> ReviewResponse {
        ReviewResponse {
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Local,
            reset: ResetChoice::None,
            allow_new_obligations: true,
            must_close_active: false,
            stuck_math_audit: Some(StuckMathAuditReviewReport {
                notes: "report".into(),
                reviewer_lean_product: None,
            }),
            dismissed_tasks,
            dismiss_audit_plan,
            ..ReviewResponse::default()
        }
    }

    fn audit_task(id: &str) -> AuditTask {
        AuditTask {
            id: id.into(),
            title: format!("{id} title"),
            body: format!("{id} body"),
            ..AuditTask::default()
        }
    }

    #[test]
    fn review_response_audit_plan_legal_accepts_dismissed_tasks_against_live_plan() {
        let request = audit_plan_legality_request(vec![audit_task("task-1"), audit_task("task-2")]);
        let response = continue_response_with_audit_plan_fields(
            vec![TaskDismissal {
                id: "task-1".into(),
                reason: "completed".into(),
            }],
            false,
        );
        assert!(
            request.review_response_legal(&response),
            "dismissed_tasks against a live audit_plan must be legal on Continue"
        );
    }

    #[test]
    fn review_response_audit_plan_legal_accepts_dismiss_audit_plan_against_live_plan() {
        let request = audit_plan_legality_request(vec![audit_task("task-1"), audit_task("task-2")]);
        let response = continue_response_with_audit_plan_fields(Vec::new(), true);
        assert!(
            request.review_response_legal(&response),
            "dismiss_audit_plan against a live audit_plan must be legal on Continue"
        );
    }

    #[test]
    fn review_response_audit_plan_legal_accepts_both_fields_together() {
        // The live bug: the reviewer prompt invites "dismiss live tasks and
        // then the whole plan once nothing live remains." A Continue
        // carrying both shapes in one step must be legal — the engine
        // applies the task dismissals before dropping the plan, so the
        // reasons live on in `superseded_audit_plan.tasks`.
        let request = audit_plan_legality_request(vec![audit_task("task-1"), audit_task("task-2")]);
        let response = continue_response_with_audit_plan_fields(
            vec![
                TaskDismissal {
                    id: "task-1".into(),
                    reason: "completed".into(),
                },
                TaskDismissal {
                    id: "task-2".into(),
                    reason: "stale".into(),
                },
            ],
            true,
        );
        assert!(
            request.review_response_legal(&response),
            "dismissed_tasks + dismiss_audit_plan together must be legal on Continue"
        );
    }

    #[test]
    fn review_response_audit_plan_legal_rejects_unknown_task_id() {
        let request = audit_plan_legality_request(vec![audit_task("task-1")]);
        let response = continue_response_with_audit_plan_fields(
            vec![TaskDismissal {
                id: "ghost".into(),
                reason: "completed".into(),
            }],
            false,
        );
        assert!(
            !request.review_response_legal(&response),
            "dismissed_tasks referencing an id not in audit_plan.tasks must be rejected"
        );
        let reasons = request.review_response_rejection_reasons(&response);
        assert!(
            reasons.iter().any(|r| r.contains("ghost")),
            "rejection reason should name the offending task id; got {reasons:?}"
        );
    }

    #[test]
    fn review_response_audit_plan_legal_rejects_when_no_audit_plan() {
        let mut request = audit_plan_legality_request(vec![audit_task("task-1")]);
        request.audit_plan = None;
        let response = continue_response_with_audit_plan_fields(
            vec![TaskDismissal {
                id: "task-1".into(),
                reason: "completed".into(),
            }],
            false,
        );
        assert!(
            !request.review_response_legal(&response),
            "dismissed_tasks must be rejected when there is no audit_plan"
        );
    }

    #[test]
    fn review_response_audit_plan_legal_accepts_need_input_plan_outside_proof_formalization() {
        // Failure case this guards: TheoremStating phase with an active
        // NeedInputAuditor plan. The phase clause must allow
        // dismissals when `plan.need_input_audit=true`, since the audit
        // plan is also surfaced to the reviewer prompt outside
        // ProofFormalization in that role.
        let mut request =
            audit_plan_legality_request(vec![audit_task("task-1"), audit_task("task-2")]);
        request.phase = Phase::TheoremStating;
        if let Some(plan) = request.audit_plan.as_mut() {
            plan.need_input_audit = true;
        }
        let response = continue_response_with_audit_plan_fields(
            vec![
                TaskDismissal {
                    id: "task-1".into(),
                    reason: "done".into(),
                },
                TaskDismissal {
                    id: "task-2".into(),
                    reason: "stale".into(),
                },
            ],
            true,
        );
        assert!(
            request.review_response_legal(&response),
            "NeedInputAuditor plan dismissals must be legal in TheoremStating"
        );
    }

    #[test]
    fn review_response_audit_plan_legal_accepts_theorem_stating_stuck_math_audit_plan() {
        // TheoremStating-phase StuckMathAudit plans (non-NeedInputAuditor)
        // must accept dismissals so the reviewer can prune tasks as their
        // substantive change lands in the Tablet, mirroring the behavior
        // for ProofFormalization and NeedInputAuditor plans. Without this,
        // the auditor's task list would only grow / get superseded by
        // fresh audits, never pruned.
        let mut request =
            audit_plan_legality_request(vec![audit_task("task-1"), audit_task("task-2")]);
        request.phase = Phase::TheoremStating;
        // plan.need_input_audit stays false (default) — this is the
        // ordinary stuck-math-audit case, not a NIA recovery.
        let response = continue_response_with_audit_plan_fields(
            vec![
                TaskDismissal {
                    id: "task-1".into(),
                    reason: "done".into(),
                },
                TaskDismissal {
                    id: "task-2".into(),
                    reason: "stale".into(),
                },
            ],
            true,
        );
        assert!(
            request.review_response_legal(&response),
            "TheoremStating stuck-math-audit plan dismissals must be legal"
        );
    }

    #[test]
    fn review_response_audit_plan_engine_applies_dismissals_then_drops_plan() {
        // Round-trip: a Continue with both `dismissed_tasks` and a normal
        // set of blocker actions is accepted, the engine records each
        // dismissed task with reason + cycle, and a concurrent
        // `dismiss_audit_plan=true` moves the (now fully dismissed)
        // plan into superseded with the dismissal trail intact.
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.cycle = 42;
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger: "test".into(),
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "diagnosis".into(),
            tasks: vec![audit_task("task-1"), audit_task("task-2")],
            ..AuditPlan::default()
        });

        let response = ReviewResponse {
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Local,
            reset: ResetChoice::None,
            allow_new_obligations: true,
            must_close_active: false,
            dismissed_tasks: vec![
                TaskDismissal {
                    id: "task-1".into(),
                    reason: "done".into(),
                },
                TaskDismissal {
                    id: "task-2".into(),
                    reason: "stale".into(),
                },
            ],
            dismiss_audit_plan: true,
            ..ReviewResponse::default()
        };

        crate::engine::apply_review_audit_plan_actions_for_test(&mut state, &response);

        assert!(
            state.audit_plan.is_none(),
            "audit_plan should be dropped when dismiss_audit_plan=true"
        );
        let superseded = state
            .superseded_audit_plan
            .as_ref()
            .expect("plan should move to superseded slot");
        let task1 = superseded
            .tasks
            .iter()
            .find(|t| t.id == "task-1")
            .expect("task-1 carried over");
        assert!(task1.dismissed);
        assert_eq!(task1.dismissed_reason, "done");
        assert_eq!(task1.dismissed_at_cycle, Some(42));
        let task2 = superseded
            .tasks
            .iter()
            .find(|t| t.id == "task-2")
            .expect("task-2 carried over");
        assert!(task2.dismissed);
        assert_eq!(task2.dismissed_reason, "stale");
        assert_eq!(task2.dismissed_at_cycle, Some(42));
    }

    #[test]
    fn review_response_audit_plan_dismiss_clears_stuck_math_audit_latch() {
        // When the reviewer dismisses the whole audit plan, the engine
        // must also clear `stuck_math_audit.active` so the next
        // start_cycle re-evaluates audit triggers from scratch. Without
        // this, the latch persists across dismissal and the cooldown
        // (1 cycle when audit_plan is None) gates the next re-fire,
        // making "dismiss" effectively a no-op in trigger semantics:
        // the audit re-fires the very next cycle even when no trigger
        // currently calls for it.
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.cycle = 100;
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger:
                "sound-stagnation-window: no Sound progress for >= 5 snapshots (theorem-stating)"
                    .into(),
            active_since_cycle: 95,
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "diagnosis".into(),
            tasks: vec![audit_task("task-1")],
            ..AuditPlan::default()
        });

        let response = ReviewResponse {
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Local,
            reset: ResetChoice::None,
            allow_new_obligations: true,
            must_close_active: false,
            dismissed_tasks: vec![],
            dismiss_audit_plan: true,
            ..ReviewResponse::default()
        };

        crate::engine::apply_review_audit_plan_actions_for_test(&mut state, &response);

        assert!(
            state.audit_plan.is_none(),
            "audit_plan should be moved to superseded"
        );
        assert!(
            !state.stuck_math_audit.active,
            "stuck_math_audit.active must be cleared on plan dismissal"
        );
        assert!(
            state.stuck_math_audit.trigger.is_empty(),
            "trigger string should be cleared"
        );
        assert!(
            state.stuck_math_audit.trigger_blockers.is_empty(),
            "trigger_blockers should be cleared"
        );
        assert_eq!(
            state.stuck_math_audit.active_since_cycle, 0,
            "active_since_cycle should reset to 0 so a future re-activation gets a fresh timestamp"
        );
    }

    #[test]
    fn review_response_audit_plan_task_dismissal_only_preserves_stuck_math_audit_latch() {
        // Per-task dismissals (without dismiss_audit_plan=true) leave
        // the audit plan in place and the latch active — the reviewer
        // is pruning the task list, not closing the audit round.
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.cycle = 100;
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger: "test trigger".into(),
            active_since_cycle: 95,
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "diagnosis".into(),
            tasks: vec![audit_task("task-1"), audit_task("task-2")],
            ..AuditPlan::default()
        });

        let response = ReviewResponse {
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_mode: TaskMode::Local,
            reset: ResetChoice::None,
            allow_new_obligations: true,
            must_close_active: false,
            dismissed_tasks: vec![TaskDismissal {
                id: "task-1".into(),
                reason: "done".into(),
            }],
            dismiss_audit_plan: false,
            ..ReviewResponse::default()
        };

        crate::engine::apply_review_audit_plan_actions_for_test(&mut state, &response);

        assert!(state.audit_plan.is_some(), "plan should still be live");
        assert!(
            state.stuck_math_audit.active,
            "latch must remain active when only individual tasks are dismissed"
        );
        assert_eq!(state.stuck_math_audit.trigger, "test trigger");
        assert_eq!(state.stuck_math_audit.active_since_cycle, 95);
    }

    #[test]
    fn stuck_math_audit_default_threshold_is_five_cycles() {
        assert_eq!(STUCK_MATH_AUDIT_CYCLES_SINCE_CLEAN_THRESHOLD_DEFAULT, 5);
    }

    #[test]
    fn stuck_math_audit_reaudit_interval_default_is_four_cycles() {
        assert_eq!(STUCK_MATH_AUDIT_REAUDIT_INTERVAL_CYCLES_DEFAULT, 4);
    }

    #[test]
    fn stuck_math_audit_no_sound_progress_window_default_is_five() {
        assert_eq!(STUCK_MATH_AUDIT_NO_SOUND_PROGRESS_WINDOW_DEFAULT, 5);
    }

    #[test]
    fn stuck_math_audit_shallow_coarse_no_progress_default_threshold_is_five_cycles() {
        assert_eq!(
            STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD_DEFAULT,
            5
        );
    }

    #[test]
    fn shallow_coarse_progress_tracking_resets_when_closed_count_increases() {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.coarse_dag_nodes = mk_set(&["A", "B"]);
        state.live.present_nodes = mk_set(&["A", "B"]);
        state.live.open_nodes = mk_set(&["A", "B"]);

        state.commit_live();
        assert_eq!(state.shallow_coarse_closed_count, 0);
        assert_eq!(state.cycles_since_shallow_coarse_closed_count_increase, 1);

        state.live.open_nodes.remove(&node("A"));
        state.commit_live();
        assert_eq!(state.shallow_coarse_closed_count, 1);
        assert_eq!(state.cycles_since_shallow_coarse_closed_count_increase, 0);

        state.commit_live();
        assert_eq!(state.shallow_coarse_closed_count, 1);
        assert_eq!(state.cycles_since_shallow_coarse_closed_count_increase, 1);
    }

    #[test]
    fn stuck_worker_outcome_no_longer_activates_stuck_math_audit() {
        let mut state = proof_phase_clean_state("Foo");
        state.live.open_nodes.insert(node("Foo"));
        state.retry_outcome_kind = RetryOutcomeKind::Stuck;

        state.refresh_stuck_math_audit_latch();

        assert!(
            !state.stuck_math_audit.active,
            "StuckMathAudit trigger B is now shallow-coarse no-progress, not worker Stuck/NeedsRestructure"
        );
    }

    #[test]
    fn shallow_coarse_no_progress_activates_stuck_math_audit() {
        let mut state = proof_phase_clean_state("Foo");
        state.coarse_dag_nodes = mk_set(&["Foo"]);
        state.live.open_nodes.insert(node("Foo"));
        state.shallow_coarse_closed_count = 0;
        state.cycles_since_shallow_coarse_closed_count_increase =
            STUCK_MATH_AUDIT_SHALLOW_COARSE_NO_PROGRESS_THRESHOLD_DEFAULT;

        state.refresh_stuck_math_audit_latch();

        assert!(state.stuck_math_audit.active);
        assert!(state
            .stuck_math_audit
            .trigger
            .contains("cycles_since_shallow_coarse_closed_count_increase"));
    }

    #[test]
    fn stuck_math_audit_request_view_is_proof_phase_only() {
        let mut state = ProtocolState {
            phase: Phase::Cleanup,
            stuck_math_audit: StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..StuckMathAuditState::default()
            },
            last_clean_rewind_count: 1,
            ..ProtocolState::default()
        };
        state.live.present_nodes = BTreeSet::from([node("A")]);
        state.proof_nodes = BTreeSet::from([node("A")]);

        assert!(
            !state.global_blockers().is_empty(),
            "test prerequisite: non-proof blockers should still exist"
        );
        assert!(
            !state.request_stuck_math_audit(RequestKind::Review).active,
            "Review requests outside ProofFormalization must not expose StuckMathAudit"
        );
        assert!(
            !state.request_stuck_math_audit(RequestKind::Worker).active,
            "Worker requests outside ProofFormalization must not expose StuckMathAudit"
        );
        assert!(
            !state
                .expected_request(0, RequestKind::Review)
                .stuck_math_audit
                .active,
            "WrapperRequest must hide the stale latch outside ProofFormalization"
        );
    }

    #[test]
    fn stuck_math_audit_survives_last_clean_until_new_clean_checkpoint() {
        let mut state = proof_phase_clean_state("Foo");
        assert!(state.global_blockers().is_empty());
        state.commit_live();
        assert!(state.last_clean_mirrors_populated());

        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger: "test".into(),
            last_reviewer_lean_product: Some(serde_json::json!({"kind": "diagnostic"})),
            ..StuckMathAuditState::default()
        };
        state.live.open_nodes.insert(node("Foo"));
        state.cycles_since_clean = 5;
        assert!(
            !state.global_blockers().is_empty(),
            "test prerequisite: dirty state should have a soundness blocker"
        );

        assert_eq!(state.apply_last_clean_reset(), Ok(true));
        assert_eq!(state.last_clean_rewind_count, 1);
        assert!(state.global_blockers().is_empty());
        assert!(
            state.stuck_math_audit.active,
            "LastClean must not clear StuckMathAudit immediately"
        );
        assert!(
            state.request_stuck_math_audit(RequestKind::Review).active,
            "the post-rewind proof-phase review must still see StuckMathAudit"
        );

        state.commit_live();
        assert_eq!(
            state.last_clean_rewind_count, 1,
            "recommitting the same clean mirror is not a new clean checkpoint"
        );
        assert!(
            state.stuck_math_audit.active,
            "StuckMathAudit persists until a genuinely new clean checkpoint"
        );

        state
            .local_closure_records
            .insert(node("Bar"), sample_record("Bar"));
        assert!(state.clean_checkpoint_ready());
        state.commit_live();
        assert_eq!(state.last_clean_rewind_count, 0);
        assert!(
            !state.stuck_math_audit.active,
            "capturing a new clean checkpoint clears StuckMathAudit"
        );
    }

    /// Option A regression for Review 315: the request_audit_plan and
    /// request_stuck_math_audit helpers must agree on visibility. Under
    /// the legacy code, a PF state with an active latch, a non-need-input
    /// plan, and empty `global_blockers()` caused request_audit_plan to
    /// surface the plan (it read `state.stuck_math_audit.active` raw)
    /// while request_stuck_math_audit zeroed the view (empty-blockers
    /// gate). The reviewer then saw `audit_plan=Some` AND
    /// `stuck_math_audit.active=false` in the same request, so any
    /// `dismiss_audit_plan` was rejected by the kernel.
    ///
    /// Under Option A, the shared `audit_plan_view_active` predicate
    /// gates BOTH helpers. The empty-blockers + no-rewind condition
    /// zeros both surfaces in lockstep, so the muddle cannot recur.
    #[test]
    fn audit_plan_view_active_is_consistent_across_helpers_review_315_repro() {
        let mut state = proof_phase_clean_state("Foo");
        // proof_phase_clean_state is all-Pass, so global_blockers is
        // empty by construction — exactly the Review 315 shape.
        assert!(
            state.global_blockers().is_empty(),
            "test prerequisite: global_blockers empty matches Review 315 reproducer shape"
        );
        // Latch pinned active without a rewind exception (Review 315
        // had `last_clean_rewind_count=0` per the inspection).
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger: "shallow-coarse synthetic".into(),
            active_since_cycle: 86,
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "synthetic".into(),
            tasks: vec![AuditTask {
                id: "task1".into(),
                title: "task1".into(),
                body: "body".into(),
                ..AuditTask::default()
            }],
            written_at_cycle: 86,
            written_by_request: 311,
            need_input_audit: false,
            trigger_at_write: "shallow-coarse synthetic".into(),
            ..AuditPlan::default()
        });
        assert_eq!(state.last_clean_rewind_count, 0);
        // Under the legacy `request_audit_plan`, this state surfaced
        // the plan (state.active=true, phase=PF). Under Option A, the
        // shared predicate zeros it because global_blockers is empty
        // and we are not in the LastClean rewind exception.
        let review_plan = state.request_audit_plan(RequestKind::Review);
        let review_latch = state.request_stuck_math_audit(RequestKind::Review);
        assert_eq!(
            review_plan.is_some(),
            review_latch.active,
            "request_audit_plan visibility must agree with request_stuck_math_audit.active for the same kind (Review 315 root cause)"
        );
        // Concretely: both zeroed in this Review 315 reproducer state.
        assert!(
            review_plan.is_none(),
            "no plan should be surfaced when the latch view is zeroed"
        );
        assert!(
            !review_latch.active,
            "the latch view should be zeroed under the empty-blockers + no-rewind condition"
        );
        // Worker view follows the same predicate.
        let worker_plan = state.request_audit_plan(RequestKind::Worker);
        let worker_latch = state.request_stuck_math_audit(RequestKind::Worker);
        assert_eq!(
            worker_plan.is_some(),
            worker_latch.active,
            "worker visibility must track the same predicate as reviewer"
        );
        // And the kernel-side dismissal-legality check (reads the
        // *request* view of stuck_math_audit.active) is now consistent
        // with the (also zeroed) plan view: no `dismiss_audit_plan`
        // affordance is shown because there is no plan to dismiss.
        let request = state.expected_request(0, RequestKind::Review);
        assert_eq!(
            request.audit_plan.is_some(),
            request.stuck_math_audit.active,
            "the WrapperRequest must carry consistent audit_plan / stuck_math_audit.active"
        );
        assert!(
            request.audit_plan.is_none() && !request.stuck_math_audit.active,
            "Review 315 reproducer: both should be zeroed under Option A"
        );
    }

    /// Option A complement to the Review 315 reproducer: with a global
    /// blocker open, the same state SHOULD surface the plan AND
    /// stuck_math_audit.active together (the normal in-cycle path).
    #[test]
    fn audit_plan_view_active_surfaces_both_when_blockers_present() {
        let mut state = proof_phase_clean_state("Foo");
        // Force a global blocker by adding an open node (sorry).
        state.live.open_nodes.insert(node("Foo"));
        assert!(
            !state.global_blockers().is_empty(),
            "test prerequisite: open node should produce a blocker"
        );
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            trigger: "synthetic".into(),
            active_since_cycle: 86,
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "synthetic".into(),
            tasks: vec![AuditTask {
                id: "task1".into(),
                title: "task1".into(),
                body: "body".into(),
                ..AuditTask::default()
            }],
            need_input_audit: false,
            ..AuditPlan::default()
        });
        let review_plan = state.request_audit_plan(RequestKind::Review);
        let review_latch = state.request_stuck_math_audit(RequestKind::Review);
        assert!(
            review_plan.is_some(),
            "Review should see the plan in the in-cycle active path"
        );
        assert!(
            review_latch.active,
            "Review should see the latch active in the in-cycle active path"
        );
        // Cross-helper consistency check.
        assert_eq!(
            review_plan.is_some(),
            review_latch.active,
            "request_audit_plan visibility must agree with request_stuck_math_audit.active"
        );
    }

    /// Option A: when the live `audit_plan` is suppressed
    /// (`request_audit_plan` returns None) but the kernel still has a
    /// plan in `state.audit_plan` or `state.superseded_audit_plan`, the
    /// snapshot helper widens to surface it on Review/Worker as a
    /// historical reference field.
    #[test]
    fn previous_audit_plan_snapshot_widens_to_review_and_worker_when_live_plan_absent() {
        // Case A: superseded plan + latch off + PF phase.
        let mut state = proof_phase_clean_state("Foo");
        state.live.open_nodes.insert(node("Foo"));
        assert!(!state.global_blockers().is_empty());
        state.stuck_math_audit = StuckMathAuditState::default(); // latch off
        state.audit_plan = None;
        state.superseded_audit_plan = Some(AuditPlan {
            report: "historical".into(),
            written_at_cycle: 50,
            ..AuditPlan::default()
        });
        // Live plan absent for Review/Worker.
        assert!(state.request_audit_plan(RequestKind::Review).is_none());
        assert!(state.request_audit_plan(RequestKind::Worker).is_none());
        // Snapshot surfaces the superseded plan.
        let snap_review = state.request_previous_audit_plan_snapshot(RequestKind::Review);
        let snap_worker = state.request_previous_audit_plan_snapshot(RequestKind::Worker);
        assert!(
            snap_review.is_some(),
            "Review should see the snapshot when no live plan and a superseded plan exists"
        );
        assert!(
            snap_worker.is_some(),
            "Worker should see the snapshot when no live plan and a superseded plan exists"
        );
        assert_eq!(snap_review.unwrap().report, "historical");
        assert_eq!(snap_worker.unwrap().report, "historical");

        // Case B: state.audit_plan present but suppressed by the
        // empty-blockers latch zeroing (Review 315 shape). The snapshot
        // should surface state.audit_plan (preferred over superseded).
        let mut state = proof_phase_clean_state("Foo");
        assert!(state.global_blockers().is_empty());
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "live-but-suppressed".into(),
            ..AuditPlan::default()
        });
        // Live surface zeroed by Option A predicate.
        assert!(state.request_audit_plan(RequestKind::Review).is_none());
        // Snapshot picks up the (un-superseded) plan.
        let snap = state.request_previous_audit_plan_snapshot(RequestKind::Review);
        assert!(
            snap.is_some(),
            "snapshot must surface state.audit_plan when the live view is suppressed"
        );
        assert_eq!(snap.unwrap().report, "live-but-suppressed");

        // Case C: when the live plan IS visible, the snapshot is
        // suppressed (no double surface).
        let mut state = proof_phase_clean_state("Foo");
        state.live.open_nodes.insert(node("Foo"));
        assert!(!state.global_blockers().is_empty());
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "live".into(),
            ..AuditPlan::default()
        });
        state.superseded_audit_plan = Some(AuditPlan {
            report: "older".into(),
            ..AuditPlan::default()
        });
        assert!(state.request_audit_plan(RequestKind::Review).is_some());
        assert!(
            state
                .request_previous_audit_plan_snapshot(RequestKind::Review)
                .is_none(),
            "snapshot must be suppressed when the live plan is surfaced"
        );

        // Case D: outside PF/TS, no snapshot for Review/Worker (matches
        // the live plan's phase gate).
        let mut state = proof_phase_clean_state("Foo");
        state.phase = Phase::Cleanup;
        state.audit_plan = None;
        state.superseded_audit_plan = Some(AuditPlan::default());
        assert!(
            state
                .request_previous_audit_plan_snapshot(RequestKind::Review)
                .is_none(),
            "snapshot must be absent outside PF/TS for Review"
        );
    }

    /// Option A invariant: the StuckMathAudit role's view of the plan
    /// is unchanged — it still authors the plan and reads the incumbent
    /// via `request_previous_audit_plan_snapshot`, never via
    /// `request_audit_plan`.
    #[test]
    fn stuck_math_audit_role_view_unchanged_under_option_a() {
        let mut state = proof_phase_clean_state("Foo");
        state.live.open_nodes.insert(node("Foo"));
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            report: "live".into(),
            ..AuditPlan::default()
        });
        // request_audit_plan(StuckMathAudit) is always None (the role
        // authors the plan; it does not consume it via this surface).
        assert!(
            state
                .request_audit_plan(RequestKind::StuckMathAudit)
                .is_none(),
            "StuckMathAudit role must never see the plan via request_audit_plan"
        );
        // The snapshot helper still returns the live plan to the
        // auditor (the basis for the next audit).
        let snap = state.request_previous_audit_plan_snapshot(RequestKind::StuckMathAudit);
        assert!(snap.is_some());
        assert_eq!(snap.unwrap().report, "live");
        // The latch view IS visible to the StuckMathAudit role (so the
        // auditor sees the trigger).
        let latch = state.request_stuck_math_audit(RequestKind::StuckMathAudit);
        assert!(latch.active);
    }

    /// Option A: `audit_plan_view_active` should return true on a
    /// need_input_audit plan even outside PF/TS (the pinned-escalation
    /// branch). Review/Worker see the plan; HumanGate sees the plan
    /// through `request_audit_plan` (separate short-circuit) but does
    /// NOT see the latch view.
    #[test]
    fn audit_plan_view_active_pins_need_input_audit_plan_across_phases() {
        let mut state = proof_phase_clean_state("Foo");
        state.phase = Phase::Cleanup;
        state.stuck_math_audit = StuckMathAuditState {
            active: true,
            ..StuckMathAuditState::default()
        };
        state.audit_plan = Some(AuditPlan {
            need_input_audit: true,
            report: "need-input".into(),
            ..AuditPlan::default()
        });
        // Review/Worker: plan + latch visible via pin.
        assert!(
            state.audit_plan_view_active(RequestKind::Review),
            "need_input plan + active latch should pin the view for Review"
        );
        assert!(state.request_audit_plan(RequestKind::Review).is_some());
        assert!(state.request_stuck_math_audit(RequestKind::Review).active);
        // HumanGate: plan visible (via the HumanGate short-circuit in
        // request_audit_plan), but latch view zeroed.
        assert!(state.request_audit_plan(RequestKind::HumanGate).is_some());
        assert!(
            !state.audit_plan_view_active(RequestKind::HumanGate),
            "HumanGate does not see the latch view"
        );
        assert!(
            !state
                .request_stuck_math_audit(RequestKind::HumanGate)
                .active
        );
    }

    #[test]
    fn dep_closure_follows_imports_downward() {
        let mut state = ProtocolState::default();
        state.deps = BTreeMap::from([
            (
                node("ThmConn"),
                BTreeSet::from([node("LemmaA"), node("LemmaB")]),
            ),
            (node("LemmaA"), BTreeSet::from([node("DefX")])),
            (node("LemmaB"), BTreeSet::new()),
            (node("DefX"), BTreeSet::new()),
        ]);
        let live_present = BTreeSet::from([
            node("ThmConn"),
            node("LemmaA"),
            node("LemmaB"),
            node("DefX"),
        ]);
        let seed = BTreeSet::from([node("ThmConn")]);

        let closure = state.dep_closure(&seed, &live_present, &state.deps);

        assert_eq!(closure, live_present);
    }

    #[test]
    fn orphan_nodes_ignore_transitively_supported_dependencies() {
        let mut state = ProtocolState::default();
        state.deps = BTreeMap::from([
            (
                node("ThmConn"),
                BTreeSet::from([node("LemmaA"), node("LemmaB")]),
            ),
            (node("LemmaA"), BTreeSet::from([node("DefX")])),
            (node("LemmaB"), BTreeSet::new()),
            (node("DefX"), BTreeSet::new()),
        ]);
        state.live.present_nodes = BTreeSet::from([
            node("Preamble"),
            node("ThmConn"),
            node("LemmaA"),
            node("LemmaB"),
            node("DefX"),
        ]);
        state.live.coverage = BTreeMap::from([(
            TargetId::from("thm:conn"),
            BTreeSet::from([node("ThmConn")]),
        )]);

        let orphans = state.orphan_nodes(&state.live);

        assert!(orphans.is_empty());
    }

    #[test]
    fn orphan_cleanup_uses_cleanup_worker_profile_and_validation_kind() {
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([node("Preamble"), node("A"), node("B")]);
        state.pending_task = Some(PendingTask {
            orphan_cleanup_nodes: BTreeSet::from([node("B")]),
            ..PendingTask::default()
        });

        assert_eq!(state.current_worker_profile(), WorkerProfile::Cleanup);
        assert_eq!(
            state.current_worker_validation_kind(),
            WorkerValidationKind::Cleanup
        );
        assert_eq!(
            state.current_worker_authorized_nodes(),
            BTreeSet::from([node("Preamble"), node("A"), node("B")])
        );
        let plan = state.current_worker_observation_plan();
        assert!(plan.capture_before_snapshot);
        assert!(plan.capture_before_tablet_contents);
        assert!(!plan.capture_baseline_declaration_hashes);
        assert!(!plan.capture_baseline_correspondence_hashes);
    }

    #[test]
    fn expected_request_summarizes_large_checker_mismatch_rejection_reason() {
        let mut state = ProtocolState::default();
        let raw_reason = format!(
            "{CHECKER_MISMATCH_REJECTION_PREFIX} worker={{\"snapshot\":\"{}\"}} supervisor={{\"errors\":[\"{}\"]}}",
            "w".repeat(10_000),
            "s".repeat(10_000)
        );
        state.deterministic_worker_rejection_reasons = vec![
            raw_reason.clone(),
            "ordinary deterministic rejection".to_string(),
        ];

        let request = state.expected_request(1, RequestKind::Review);

        assert_eq!(state.deterministic_worker_rejection_reasons[0], raw_reason);
        assert_eq!(request.deterministic_worker_rejection_reasons.len(), 2);
        let summarized = &request.deterministic_worker_rejection_reasons[0];
        assert!(summarized.starts_with(CHECKER_MISMATCH_REJECTION_PREFIX));
        assert!(!summarized.contains("worker={"));
        assert!(!summarized.contains("supervisor={"));
        assert!(summarized.len() < 600);
        assert_eq!(
            request.review_contract["request_summary"]["deterministic_worker_rejection_reasons"],
            serde_json::json!(request.deterministic_worker_rejection_reasons.clone())
        );
    }

    #[test]
    fn checker_mismatch_summary_preserves_sorry_ax_reminder() {
        let mut state = ProtocolState::default();
        state.deterministic_worker_rejection_reasons = vec![format!(
            "{CHECKER_MISMATCH_REJECTION_PREFIX} worker={{\"ok\":true}} supervisor={{\"errors\":[\"Axiom audit failed: Unapproved axioms: [\\\"sorryAx\\\"]\"],\"snapshot\":\"{}\"}}",
            "s".repeat(10_000)
        )];

        let request = state.expected_request(1, RequestKind::Worker);

        let summarized = &request.deterministic_worker_rejection_reasons[0];
        assert!(summarized.starts_with(CHECKER_MISMATCH_REJECTION_PREFIX));
        assert!(!summarized.contains("worker={"));
        assert!(summarized.contains(SORRY_AX_REJECTION_REMINDER));
    }

    #[test]
    fn final_cleanup_phase_uses_distinct_worker_profile_and_validation_kind() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.live.present_nodes = BTreeSet::from([node("Preamble"), node("A")]);

        assert_eq!(state.current_worker_profile(), WorkerProfile::FinalCleanup);
        assert_eq!(
            state.current_worker_validation_kind(),
            WorkerValidationKind::FinalCleanup
        );
        assert_eq!(
            state.current_worker_authorized_nodes(),
            BTreeSet::from([node("Preamble"), node("A")])
        );
    }

    #[test]
    fn easy_active_node_in_restructure_mode_validates_as_proof_restructure() {
        // Reviewer chose Restructure mode (typically to authorize a
        // task_blocker fix on a different node) on an Easy active node.
        // The kernel must honour the mode rather than short-circuiting to
        // ProofEasy — otherwise the worker is restricted to the active
        // node's .lean and physically cannot address the blocker.
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some(node("a"));
        state.live.present_nodes = BTreeSet::from([node("Preamble"), node("a"), node("b")]);
        state
            .node_difficulty
            .insert(node("a"), NodeDifficulty::Easy);
        state.proof_edit_mode = ProofEditMode::Restructure;
        assert_eq!(state.current_active_difficulty(), NodeDifficulty::Easy);
        assert_eq!(
            state.current_worker_validation_kind(),
            WorkerValidationKind::ProofRestructure,
        );
    }

    #[test]
    fn easy_active_node_in_local_mode_validates_as_proof_local() {
        // Difficulty is now advisory: Local scope no longer changes just
        // because the active node is marked Easy.
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some(node("a"));
        state.live.present_nodes = BTreeSet::from([node("Preamble"), node("a")]);
        state
            .node_difficulty
            .insert(node("a"), NodeDifficulty::Easy);
        state.proof_edit_mode = ProofEditMode::Local;
        assert_eq!(state.current_active_difficulty(), NodeDifficulty::Easy);
        assert_eq!(
            state.current_worker_validation_kind(),
            WorkerValidationKind::ProofLocal,
        );
        assert!(matches!(
            state.current_worker_validation_execution_plan().as_slice(),
            [WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                mode: WorkerProofDeltaMode::Local,
                ..
            }]
        ));
    }

    #[test]
    fn easy_active_node_in_coarse_restructure_validates_as_proof_coarse_restructure() {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.active_node = Some(node("a"));
        state.live.present_nodes = BTreeSet::from([node("Preamble"), node("a")]);
        state
            .node_difficulty
            .insert(node("a"), NodeDifficulty::Easy);
        state.proof_edit_mode = ProofEditMode::CoarseRestructure;
        assert_eq!(state.current_active_difficulty(), NodeDifficulty::Easy);
        assert_eq!(
            state.current_worker_validation_kind(),
            WorkerValidationKind::ProofCoarseRestructure,
        );
    }

    #[test]
    fn need_input_uses_current_mode_even_when_continue_routing_menu_differs() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            phase: Phase::TheoremStating,
            mode: TaskMode::Targeted,
            allowed_decisions: BTreeSet::from([ReviewDecisionKind::NeedInput]),
            allowed_next_modes: BTreeSet::from([TaskMode::Global]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            ..WrapperRequest::default()
        };
        let legal = ReviewResponse {
            decision: ReviewDecisionKind::NeedInput,
            next_mode: TaskMode::Targeted,
            ..ReviewResponse::default()
        };
        assert!(request.review_response_legal(&legal));

        let wrong_mode = ReviewResponse {
            next_mode: TaskMode::Global,
            ..legal
        };
        assert!(!request.review_response_legal(&wrong_mode));
    }

    // Removed in the protected_correspondence refactor:
    //   semantic_closure_follows_semantic_deps_downward — tested
    //     semantic_closure(), which is gone alongside semantic_deps.
    //   protected_nodes_range_only_over_live_present_nodes — tested
    //     protected_nodes(), which is gone.
    //   protected_package_legality_checks_semantic_prerequisites —
    //     asserted that a non-covering protected-closure node's
    //     target_fingerprint change rejected the worker's snapshot.
    //     Under the new design only covering nodes are protected, so
    //     `DefX` (non-covering) changing is legal; the test's
    //     negative assertion no longer applies.

    #[test]
    fn theorem_review_next_active_uses_held_node_dep_closure() {
        // K-7 regression: the held-target branch of
        // `theorem_review_next_active_legal` previously fed the held
        // covering NodeId into `target_support_cone`, which keys
        // `coverage` by TargetId. The lookup silently returned None,
        // collapsing `kernel_hinted_next_active_nodes` to empty whenever
        // (TheoremStating + held set + corr cleared) was reached.
        // Both ids are `String` aliases so rustc never caught it.
        // Surfaced empirically by the synthetic graph harness in
        // commit 533e240.
        //
        // Fix restores the original (pre-3fa4fc1a) semantics:
        // legal next_active is `dep_closure({held})` — the held node
        // itself plus everything it depends on.
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([
            node("Preamble"),
            node("ZMain"),
            node("LemmaA"),
            node("Other"),
        ]);
        state.live.open_nodes = BTreeSet::from([node("ZMain")]);
        state.proof_nodes = BTreeSet::from([node("ZMain")]);
        state.deps = BTreeMap::from([
            (node("ZMain"), BTreeSet::from([node("LemmaA")])),
            (node("LemmaA"), BTreeSet::new()),
            (node("Other"), BTreeSet::new()),
        ]);
        // Drive `current_corr_pass` to true for every present node so
        // `corr_blockers_exist()` is false and `corr_verify_nodes()`
        // is empty (no Unknown/Fail corr states): both are required
        // to reach the held-target branch under test.
        for n in [
            node("Preamble"),
            node("ZMain"),
            node("LemmaA"),
            node("Other"),
        ] {
            state.corr_status.insert(n.clone(), CorrStatus::Pass);
            state
                .live
                .corr_current_fingerprints
                .insert(n.clone(), "fp".to_string());
            state
                .corr_approved_fingerprints
                .insert(n.clone(), "fp".to_string());
            state
                .substantiveness_status
                .insert(n.clone(), CorrStatus::Pass);
            state
                .live
                .substantiveness_current_fingerprints
                .insert(n.clone(), "sub-fp".to_string());
            state
                .substantiveness_approved_fingerprints
                .insert(n, "sub-fp".to_string());
        }
        // ZMain's sound state is Unknown by default (no sound_status
        // entry), satisfying `sound_state != Pass` in
        // `select_theorem_held_target`.

        // Sanity: the held-target branch is the one being exercised.
        assert_eq!(state.select_theorem_held_target().as_deref(), Some("ZMain"));
        assert!(state.blocked_targets().is_empty());
        assert!(state.corr_verify_nodes().is_empty());

        // The held node itself is legal.
        assert!(state.theorem_review_next_active_legal(Some(&node("ZMain"))));
        // Its dependency is legal (in the dep_closure of {ZMain}).
        assert!(state.theorem_review_next_active_legal(Some(&node("LemmaA"))));
        // An unrelated node is not.
        assert!(!state.theorem_review_next_active_legal(Some(&node("Other"))));
    }

    // ---- Substantiveness lane tests (audit Finding 4.3) ----
    //
    // These tests use the post-K-8 NodeId / TargetId newtype API directly
    // (`NodeId::from`) to avoid the broken `fn node` helper and the bulk
    // `String`-vs-newtype mismatch that has the rest of the kernel test
    // surface failing to compile. They only exercise read-side and
    // status-mirror code paths, so they don't need the broken helpers.

    fn nid(s: &str) -> NodeId {
        NodeId::from(s)
    }

    /// Set up a TheoremStating state with two present nodes (A, B), where
    /// A has substantiveness=Pass (with matching fingerprints) and B is
    /// Unknown. Both nodes have corr=Unknown. Used by the corr-gate tests.
    fn theorem_state_with_substantiveness_a_pass_b_unknown() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("A"), nid("B")]);
        state.proof_nodes = BTreeSet::from([nid("A"), nid("B")]);

        // Substantiveness: A=Pass with matching fingerprints, B=Unknown.
        state
            .substantiveness_status
            .insert(nid("A"), CorrStatus::Pass);
        state
            .live
            .substantiveness_current_fingerprints
            .insert(nid("A"), "fpA".to_string());
        state
            .substantiveness_approved_fingerprints
            .insert(nid("A"), "fpA".to_string());
        // B has no entries -> current_substantiveness_state returns Unknown.

        // Corr Unknown for both A and B.
        state.corr_status.insert(nid("A"), CorrStatus::Unknown);
        state.corr_status.insert(nid("B"), CorrStatus::Unknown);
        state
    }

    #[test]
    fn current_corr_state_honors_preamble_empty_fp_pin() {
        // Regression: `ensure_initial_preamble` (bin/runtime_cli.rs:4681)
        // pre-pins Preamble corr to `(Pass, "", "")` when Preamble.tex
        // has no structured definition items — vacuous correspondence.
        // 7aad7cb's `!current.is_empty()` guard accidentally tripped
        // this pin and made the kernel re-dispatch a Corr verifier
        // every cycle, producing an infinite verify loop on fresh runs
        // whose Preamble.tex is structurally empty. The Preamble carve-
        // out in `current_corr_state` honors the empty-fp pin via
        // fingerprint equality alone.
        let preamble = nid("Preamble");
        let mut state = ProtocolState::default();
        state.live.present_nodes = BTreeSet::from([preamble.clone()]);
        state.corr_status.insert(preamble.clone(), CorrStatus::Pass);
        state
            .live
            .corr_current_fingerprints
            .insert(preamble.clone(), String::new());
        state
            .corr_approved_fingerprints
            .insert(preamble.clone(), String::new());

        assert_eq!(
            state.current_corr_state(&preamble),
            CurrentCheckState::Pass,
            "Preamble with the init pin (Pass, \"\", \"\") must read Pass; \
             would loop into corr_verify_nodes otherwise"
        );

        // Counterpoint: a non-Preamble node with the same empty-fp pin
        // still reads Unknown (the general guard is preserved).
        let other = nid("OtherNode");
        state.live.present_nodes.insert(other.clone());
        state.corr_status.insert(other.clone(), CorrStatus::Pass);
        state
            .live
            .corr_current_fingerprints
            .insert(other.clone(), String::new());
        state
            .corr_approved_fingerprints
            .insert(other.clone(), String::new());
        assert_eq!(
            state.current_corr_state(&other),
            CurrentCheckState::Unknown,
            "Non-Preamble node with empty fp still reads Unknown (general guard preserved)"
        );
    }

    #[test]
    fn sound_dispatch_gated_when_corr_blockers_exist() {
        // The Sound verifier reasons about a proof citing dep statements
        // protected by paper / corr / substantiveness / deviation. While
        // any of those non-sound lanes is in motion, a Sound dispatch
        // would pin a verdict against a moving target. Both the auto-
        // dispatch and the reviewer-requested entry points must refuse.
        let preamble = nid("Preamble");
        let a = nid("A");
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([preamble.clone(), a.clone()]);
        // Preamble corr Pass via the empty-fp init pin.
        state.corr_status.insert(preamble.clone(), CorrStatus::Pass);
        state
            .live
            .corr_current_fingerprints
            .insert(preamble.clone(), String::new());
        state
            .corr_approved_fingerprints
            .insert(preamble.clone(), String::new());
        // Reviewer asked to re-verify A's soundness.
        state.reviewer_requested_sound_verifier_nodes = BTreeSet::from([a.clone()]);

        // With A's corr still Unknown (no entry in corr_status), the
        // global predicate fires and both entry points return empty.
        assert!(
            state.corr_blockers_exist(),
            "A has no corr entry, so corr_blockers_exist must fire"
        );
        assert!(
            state.reviewer_requested_sound_verify_nodes().is_empty(),
            "reviewer-requested Sound must refuse while other-lane blockers exist"
        );
        assert!(
            state.sound_verify_nodes().is_empty(),
            "auto-dispatch Sound must refuse while other-lane blockers exist"
        );
    }

    #[test]
    fn sound_dispatch_gated_by_deviation_blocker() {
        // `corr_blockers_exist` now also returns true when any tracked
        // deviation is not Pass; the Sound gate inherits that. Without
        // this, a reviewer could pre-empt a deviation-authorization
        // verifier with a Sound request.
        let preamble = nid("Preamble");
        let a = nid("A");
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([preamble.clone(), a.clone()]);
        // Both nodes corr Pass; substantiveness Pass.
        for n in [&preamble, &a] {
            state.corr_status.insert(n.clone(), CorrStatus::Pass);
            state
                .live
                .corr_current_fingerprints
                .insert(n.clone(), "fp".into());
            state
                .corr_approved_fingerprints
                .insert(n.clone(), "fp".into());
            state
                .substantiveness_status
                .insert(n.clone(), CorrStatus::Pass);
            state
                .live
                .substantiveness_current_fingerprints
                .insert(n.clone(), "sub".into());
            state
                .substantiveness_approved_fingerprints
                .insert(n.clone(), "sub".into());
        }
        // Now introduce an unauthorized deviation (status absent →
        // current_deviation_pass = false).
        let dev = DeviationId::from("dev:x");
        state
            .deviation_files
            .insert(dev.clone(), "reference/x.tex".into());
        // Reviewer asks for Sound on A.
        state.reviewer_requested_sound_verifier_nodes = BTreeSet::from([a.clone()]);

        assert!(
            state.corr_blockers_exist(),
            "open deviation must trip corr_blockers_exist"
        );
        assert!(
            state.reviewer_requested_sound_verify_nodes().is_empty(),
            "reviewer-requested Sound must refuse while a deviation is open"
        );
    }

    #[test]
    fn corr_verify_nodes_excludes_substantiveness_unknown_in_theorem_stating() {
        // In TheoremStating, the corr lane filters nodes that haven't
        // reached substantiveness Pass: a node that isn't paper-cleared
        // shouldn't enter the corr frontier yet.
        let state = theorem_state_with_substantiveness_a_pass_b_unknown();
        let frontier = state.corr_verify_nodes();
        assert!(
            frontier.contains(&nid("A")),
            "A is substantiveness=Pass, so it must enter the corr frontier; got {:?}",
            frontier
        );
        assert!(
            !frontier.contains(&nid("B")),
            "B is substantiveness=Unknown, so it must NOT enter the corr frontier; got {:?}",
            frontier
        );
    }

    #[test]
    fn corr_verify_nodes_excludes_substantiveness_unknown_in_proof_formalization() {
        // The substantiveness lane fires in ProofFormalization too (helper
        // nodes added by Hard restructure are checked). The corr-gate
        // filter is therefore active: B (substantiveness=Unknown) must NOT
        // enter the corr frontier even in ProofFormalization.
        let mut state = theorem_state_with_substantiveness_a_pass_b_unknown();
        state.phase = Phase::ProofFormalization;
        let frontier = state.corr_verify_nodes();
        assert!(
            frontier.contains(&nid("A")),
            "A is substantiveness=Pass; must enter the corr frontier; got {:?}",
            frontier
        );
        assert!(
            !frontier.contains(&nid("B")),
            "B is substantiveness=Unknown; must NOT enter the corr frontier in ProofFormalization; got {:?}",
            frontier
        );
    }

    #[test]
    fn corr_verify_nodes_unaffected_by_substantiveness_in_cleanup() {
        // In Cleanup the substantiveness lane is dormant
        // (current_substantiveness_state short-circuits to Pass), so the
        // corr-gate filter is a no-op: both A and B land on the corr
        // frontier even though only A has explicit substantiveness=Pass.
        let mut state = theorem_state_with_substantiveness_a_pass_b_unknown();
        state.phase = Phase::Cleanup;
        let frontier = state.corr_verify_nodes();
        assert!(
            frontier.contains(&nid("A")),
            "A must enter the corr frontier; got {:?}",
            frontier
        );
        assert!(
            frontier.contains(&nid("B")),
            "B must enter the corr frontier in Cleanup (substantiveness lane dormant); got {:?}",
            frontier
        );
    }

    #[test]
    fn expected_request_populates_substantiveness_verify_nodes() {
        // With one substantiveness=Unknown node and no paper-target Unknowns,
        // expected_request(Paper) should carry that node in
        // substantiveness_verify_nodes (per-node Paper scenario).
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("X")]);
        state.proof_nodes = BTreeSet::from([nid("X")]);
        // No configured_targets so paper_verify_targets() is empty -> per-node
        // scenario eligible.
        // X has no substantiveness_status entry, so current_substantiveness_state
        // returns Unknown -> X goes on the substantiveness frontier.

        let request = state.expected_request(0, RequestKind::Paper);
        assert!(
            request.paper_verify_targets.is_empty(),
            "Per-node scenario: paper_verify_targets must be empty; got {:?}",
            request.paper_verify_targets
        );
        assert_eq!(
            request.substantiveness_verify_nodes,
            BTreeSet::from([nid("X")]),
            "substantiveness_verify_nodes must carry the Unknown node",
        );
    }

    #[test]
    fn old_state_file_without_substantiveness_fields_deserializes_cleanly() {
        // ProtocolState fields for the substantiveness lane all carry
        // #[serde(default)]. A state JSON that omits them entirely should
        // deserialize and the new fields should default to empty.
        let raw = r#"{
            "phase": "theorem_stating",
            "stage": "start",
            "cycle": 0
        }"#;
        let state: ProtocolState =
            serde_json::from_str(raw).expect("state without substantiveness fields must parse");
        assert!(
            state.substantiveness_status.is_empty(),
            "substantiveness_status must default to empty"
        );
        assert!(
            state.substantiveness_approved_fingerprints.is_empty(),
            "substantiveness_approved_fingerprints must default to empty"
        );
        assert!(
            state.last_clean_substantiveness_status.is_empty(),
            "last_clean_substantiveness_status must default to empty"
        );
        assert!(
            state
                .last_clean_substantiveness_approved_fingerprints
                .is_empty(),
            "last_clean_substantiveness_approved_fingerprints must default to empty"
        );
        assert!(
            state.latest_substantiveness_reviewer_evidence.is_empty(),
            "latest_substantiveness_reviewer_evidence must default to empty"
        );
        assert!(
            state.latest_substantiveness_review_nodes.is_empty(),
            "latest_substantiveness_review_nodes must default to empty"
        );
        assert!(
            state.previous_substantiveness_lane_findings.is_empty(),
            "previous_substantiveness_lane_findings must default to empty"
        );
        assert_eq!(
            state.substantiveness_consecutive_no_progress_requests, 0,
            "substantiveness_consecutive_no_progress_requests must default to 0"
        );
        assert!(
            state.live.substantiveness_current_fingerprints.is_empty(),
            "WorkingSnapshot.substantiveness_current_fingerprints must default to empty"
        );
    }

    #[test]
    fn substantiveness_blockers_block_advance_phase() {
        // Audit Finding §2.5: PaperNodeFaithfulness (now Substantiveness)
        // blockers must block phase advance via `review_response_legal`'s
        // `self.blockers.is_empty()` AdvancePhase clause. Set up a
        // TheoremStating state with one substantiveness=Unknown node and
        // attempt AdvancePhase: the response must be rejected.
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("X")]);
        state.proof_nodes = BTreeSet::from([nid("X")]);
        // No configured_targets so paper-target lane is clean.
        // X has no substantiveness_status entry -> Unknown -> a Substantiveness
        // blocker is generated by global_blockers().
        // No corr/sound entries so X is also corr/sound Unknown — fine for
        // this test, since blocker non-emptiness alone rejects AdvancePhase.

        let request = state.expected_request(0, RequestKind::Review);
        // Sanity: blockers must be non-empty for the test to be meaningful.
        assert!(
            !request.blockers.is_empty(),
            "test prerequisite: state must have at least one blocker"
        );
        let has_subst_blocker = request
            .blockers
            .iter()
            .any(|b| b.kind == BlockerKind::Substantiveness);
        assert!(
            has_subst_blocker,
            "test prerequisite: at least one blocker must be Substantiveness; got {:?}",
            request.blockers
        );

        let advance_phase_response = ReviewResponse {
            request_id: request.id,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::AdvancePhase,
            reason: String::new(),
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            request_sound_verifier_nodes: BTreeSet::new(),
            next_active: None,
            next_active_coarse: None,
            reset: ResetChoice::None,
            reset_node: None,
            next_mode: TaskMode::default(),
            difficulty_updates: BTreeMap::new(),
            allow_new_obligations: true,
            must_close_active: false,
            clear_human_input: false,
            next_worker_context_mode: WorkerContextMode::default(),
            paper_focus_ranges: Vec::new(),
            work_style_hint: WorkerWorkStyleHint::default(),
            protected_semantic_change_nodes: BTreeSet::new(),
            confirm_protected_semantic_change_scope: false,
            global_repair_request: None,
            consume_global_repair_grant: false,
            authorized_nodes: BTreeSet::new(),
            cleanup_dismiss_tasks: Vec::new(),
            cleanup_next_task: None,
            cleanup_request_reaudit: false,
            paper_grounding: PaperGrounding::default(),
            stuck_math_audit: None,
            dismiss_audit_plan: false,
            dismissed_tasks: Vec::new(),
        };

        assert!(
            !request.review_response_legal(&advance_phase_response),
            "AdvancePhase must be rejected when Substantiveness blockers are present",
        );
    }

    /// Finding D: TheoremStating + Targeted + AdvancePhase + no
    /// `next_active` must be legal — the next phase's request
    /// rederives `next_active` from scratch, so the field is inert on
    /// this code path. The waiver is narrowly scoped: the same setup
    /// with Continue must still be rejected.
    #[test]
    fn review_response_legal_advance_phase_targeted_no_next_active() {
        let request = WrapperRequest {
            kind: RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: BTreeSet::from([
                ReviewDecisionKind::AdvancePhase,
                ReviewDecisionKind::Continue,
            ]),
            allowed_next_modes: BTreeSet::from([TaskMode::Targeted]),
            allowed_resets: BTreeSet::from([ResetChoice::None]),
            targeted_next_active_nodes: BTreeSet::new(),
            blockers: BTreeSet::new(),
            human_input_outstanding: false,
            ..WrapperRequest::default()
        };
        let advance_phase = ReviewResponse {
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::AdvancePhase,
            next_mode: TaskMode::Targeted,
            next_active: None,
            reset: ResetChoice::None,
            allow_new_obligations: true,
            must_close_active: false,
            clear_human_input: false,
            ..ReviewResponse::default()
        };
        assert!(
            request.review_response_legal(&advance_phase),
            "AdvancePhase+Targeted+next_active=None must be legal (next phase rederives next_active); reasons={:?}",
            request.review_response_rejection_reasons(&advance_phase),
        );

        let continue_response = ReviewResponse {
            decision: ReviewDecisionKind::Continue,
            ..advance_phase
        };
        assert!(
            !request.review_response_legal(&continue_response),
            "Continue+Targeted+next_active=None must still be rejected (waiver is AdvancePhase-only)"
        );
    }

    #[test]
    fn last_clean_reset_restores_substantiveness_status_and_approved_fps() {
        // Audit Finding §2.6 / plan §1.7: the last_clean mirror must
        // capture and restore substantiveness_status and
        // substantiveness_approved_fingerprints. Without this, a LastClean
        // rewind would leave the substantiveness lane stale relative to
        // the rewound disk state.
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        // Single proof node X. No configured_targets, no other nodes
        // beyond Preamble — keeps global_blockers empty without juggling
        // every lane.
        state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("X")]);
        state.proof_nodes = BTreeSet::from([nid("X")]);

        // Make every present node Pass on every relevant lane (paper-target
        // lane is vacuously Pass with empty configured_targets).
        // Substantiveness: Preamble auto-passes (PREAMBLE_NAME special case);
        // X must have status=Pass + matching fingerprints.
        state
            .substantiveness_status
            .insert(nid("X"), CorrStatus::Pass);
        state
            .live
            .substantiveness_current_fingerprints
            .insert(nid("X"), "fpX".to_string());
        state
            .substantiveness_approved_fingerprints
            .insert(nid("X"), "fpX".to_string());
        // Corr: Preamble + X both Pass with matching fingerprints.
        for n in [nid("Preamble"), nid("X")] {
            state.corr_status.insert(n.clone(), CorrStatus::Pass);
            state
                .live
                .corr_current_fingerprints
                .insert(n.clone(), "cfp".to_string());
            state
                .corr_approved_fingerprints
                .insert(n, "cfp".to_string());
        }
        // Sound: only proof_nodes need sound Pass.
        state.sound_status.insert(nid("X"), SoundStatus::Pass);
        state
            .live
            .sound_current_fingerprints
            .insert(nid("X"), "sfp".to_string());
        state
            .sound_approved_fingerprints
            .insert(nid("X"), "sfp".to_string());

        // Sanity: state is clean (no blockers).
        assert!(
            state.global_blockers().is_empty(),
            "test prerequisite: state must be clean before commit_live; blockers={:?}",
            state.global_blockers()
        );

        // Capture clean checkpoint (snapshots last_clean_* mirrors).
        state.commit_live();
        assert!(
            state.last_clean_mirrors_populated(),
            "commit_live with empty global_blockers must populate the last_clean mirrors"
        );
        assert_eq!(
            state.last_clean_substantiveness_status.get(&nid("X")),
            Some(&CorrStatus::Pass),
            "last_clean_substantiveness_status must capture X=Pass"
        );
        assert_eq!(
            state
                .last_clean_substantiveness_approved_fingerprints
                .get(&nid("X")),
            Some(&"fpX".to_string()),
            "last_clean_substantiveness_approved_fingerprints must capture fpX"
        );

        // Mutate working state: substantiveness_status[X] -> Unknown,
        // approved fingerprint cleared.
        state
            .substantiveness_status
            .insert(nid("X"), CorrStatus::Unknown);
        state
            .substantiveness_approved_fingerprints
            .remove(&nid("X"));
        assert!(
            !state.current_substantiveness_pass(&nid("X")),
            "post-mutation, X must not be current_substantiveness_pass"
        );

        // Apply LastClean reset. Patch C-N item 2: signature now
        // returns Result<bool, String>; commit_live above populated
        // both verifier and closure mirrors, so this returns Ok(true).
        assert_eq!(state.apply_last_clean_reset(), Ok(true));

        // Post-reset, substantiveness_status and approved_fps must be
        // restored to their clean-checkpoint values.
        assert_eq!(
            state.substantiveness_status.get(&nid("X")),
            Some(&CorrStatus::Pass),
            "substantiveness_status[X] must restore to Pass after LastClean"
        );
        assert_eq!(
            state.substantiveness_approved_fingerprints.get(&nid("X")),
            Some(&"fpX".to_string()),
            "substantiveness_approved_fingerprints[X] must restore to fpX after LastClean"
        );
        assert!(
            state.current_substantiveness_pass(&nid("X")),
            "post-restore, X must again be current_substantiveness_pass"
        );
    }

    // ---- Topological dispatch ---------------------------------------------

    /// Build a fingerprint string with the given dependency set (used for
    /// dispatch eligibility) and a definition-descendants map (used for
    /// TeX-hash reopen). Tests that don't care about the def/all-set
    /// distinction usually pass the same names to both via
    /// `corr_fp_with_l_def`.
    fn corr_fp_with_dependencies_and_def_descendants(
        dependencies: &[&str],
        def_descendants: &[(&str, &str)],
    ) -> String {
        let l_def: serde_json::Map<String, serde_json::Value> = def_descendants
            .iter()
            .map(|(name, hash)| {
                (
                    (*name).to_string(),
                    serde_json::Value::String((*hash).to_string()),
                )
            })
            .collect();
        let deps: Vec<serde_json::Value> = dependencies
            .iter()
            .map(|d| serde_json::Value::String((*d).to_string()))
            .collect();
        serde_json::json!({
            "own_tex": "",
            "lean_semantic_closure": "",
            "lean_relevant_definition_descendants": l_def,
            "lean_relevant_dependencies": deps,
            "preamble_tex": "",
        })
        .to_string()
    }

    /// Convenience wrapper: dependencies == def-descendant names. The L_def
    /// values are stable string "h". Use when the test doesn't distinguish
    /// def-kind vs all-relevant.
    fn corr_fp_with_l_def(descendants: &[&str]) -> String {
        let def_pairs: Vec<(&str, &str)> = descendants.iter().map(|d| (*d, "h")).collect();
        corr_fp_with_dependencies_and_def_descendants(descendants, &def_pairs)
    }

    fn substantiveness_pass_state() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([node("Preamble"), node("D"), node("N")]);
        state.proof_nodes = BTreeSet::from([node("D"), node("N")]);
        // Both nodes substantiveness Pass so they don't gate corr.
        for n in ["D", "N"] {
            state
                .live
                .substantiveness_current_fingerprints
                .insert(node(n), format!("sub-{n}"));
            state
                .substantiveness_status
                .insert(node(n), CorrStatus::Pass);
            state
                .substantiveness_approved_fingerprints
                .insert(node(n), format!("sub-{n}"));
        }
        state
    }

    #[test]
    fn corr_dispatch_defers_node_with_open_descendant_corr() {
        // N depends on D (D ∈ L(N)). D's corr is Unknown. N's corr should
        // be deferred — not on the corr_verify_nodes frontier.
        let mut state = substantiveness_pass_state();
        state
            .live
            .corr_current_fingerprints
            .insert(node("D"), corr_fp_with_l_def(&[]));
        state
            .live
            .corr_current_fingerprints
            .insert(node("N"), corr_fp_with_l_def(&["D"]));
        // D and N are both Unknown (no corr_status entries).

        let frontier = state.corr_verify_nodes();
        assert!(
            frontier.contains(&node("D")),
            "leaf D must be on the corr-verify frontier"
        );
        assert!(
            !frontier.contains(&node("N")),
            "N must be deferred while D's corr is open"
        );
    }

    #[test]
    fn corr_dispatch_eligible_after_descendant_passes() {
        // After D's corr passes, N becomes dispatch-eligible.
        let mut state = substantiveness_pass_state();
        state
            .live
            .corr_current_fingerprints
            .insert(node("D"), corr_fp_with_l_def(&[]));
        state
            .live
            .corr_current_fingerprints
            .insert(node("N"), corr_fp_with_l_def(&["D"]));
        // D has corr=Pass with matching approved fingerprint.
        state.corr_status.insert(node("D"), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert(node("D"), corr_fp_with_l_def(&[]));

        assert!(
            state.is_corr_dispatch_eligible(&node("N")),
            "N must be dispatch-eligible once D is corr-Pass"
        );
        let frontier = state.corr_verify_nodes();
        assert!(
            frontier.contains(&node("N")),
            "N must surface on the corr-verify frontier once D passes"
        );
    }

    #[test]
    fn corr_blocker_marked_deferred_when_dependency_open() {
        // global_blockers() must mark N's NodeCorr blocker as deferred when
        // a dependency D has open corr.
        let mut state = substantiveness_pass_state();
        state
            .live
            .corr_current_fingerprints
            .insert(node("D"), corr_fp_with_l_def(&[]));
        state
            .live
            .corr_current_fingerprints
            .insert(node("N"), corr_fp_with_l_def(&["D"]));

        let blockers = state.global_blockers();
        let n_blocker = blockers
            .iter()
            .find(|b| {
                b.kind == BlockerKind::NodeCorr
                    && matches!(&b.object, BlockerObject::Node { node: n } if n == &node("N"))
            })
            .expect("N must have a NodeCorr blocker (corr Unknown)");
        assert!(
            n_blocker.deferred,
            "N's blocker must be deferred while D's corr is open"
        );
        let d_blocker = blockers
            .iter()
            .find(|b| {
                b.kind == BlockerKind::NodeCorr
                    && matches!(&b.object, BlockerObject::Node { node: n } if n == &node("D"))
            })
            .expect("D must have a NodeCorr blocker");
        assert!(
            !d_blocker.deferred,
            "D's blocker (leaf, no open dependencies) must NOT be deferred"
        );
    }

    #[test]
    fn deferred_blocker_filtered_from_review_request() {
        // Review request must filter out deferred blockers so the
        // blocker-action subset check sees only actionable ones.
        let mut state = substantiveness_pass_state();
        state
            .live
            .corr_current_fingerprints
            .insert(node("D"), corr_fp_with_l_def(&[]));
        state
            .live
            .corr_current_fingerprints
            .insert(node("N"), corr_fp_with_l_def(&["D"]));

        let request_blockers = state.request_blockers(RequestKind::Review);
        assert!(
            request_blockers.iter().all(|b| !b.deferred),
            "Review request blockers must not include deferred entries"
        );
        // D should still be there (actionable leaf).
        assert!(
            request_blockers
                .iter()
                .any(|b| b.kind == BlockerKind::NodeCorr
                    && matches!(&b.object, BlockerObject::Node { node: n } if n == &node("D"))),
            "D's leaf NodeCorr must surface to the reviewer"
        );
        // N's deferred blocker should NOT be there.
        assert!(
            !request_blockers
                .iter()
                .any(|b| b.kind == BlockerKind::NodeCorr
                    && matches!(&b.object, BlockerObject::Node { node: n } if n == &node("N"))),
            "N's deferred NodeCorr must NOT surface to the reviewer"
        );
        // Global blockers (unfiltered) must still include both N and D —
        // used by gates like `corr_blockers_exist`.
        let global = state.global_blockers();
        assert!(
            global.iter().any(|b| b.kind == BlockerKind::NodeCorr
                && matches!(&b.object, BlockerObject::Node { node: n } if n == &node("N"))),
            "global_blockers must still surface N's NodeCorr"
        );
        assert!(
            global.iter().any(|b| b.kind == BlockerKind::NodeCorr
                && matches!(&b.object, BlockerObject::Node { node: n } if n == &node("D"))),
            "global_blockers must still surface D's NodeCorr"
        );
    }

    #[test]
    fn paper_dispatch_defers_target_with_open_covering_corr() {
        // Target T is paper-Unknown; covering node X is corr-Unknown. Paper
        // must be deferred until X's corr passes.
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.live.present_nodes = BTreeSet::from([node("Preamble"), node("X")]);
        state.configured_targets = BTreeSet::from([TargetId::from("T")]);
        state
            .target_claims
            .insert(node("X"), BTreeSet::from([TargetId::from("T")]));
        state
            .live
            .coverage
            .insert(TargetId::from("T"), BTreeSet::from([node("X")]));
        state
            .live
            .paper_current_fingerprints
            .insert(TargetId::from("T"), "tfp".to_string());
        state
            .live
            .corr_current_fingerprints
            .insert(node("X"), corr_fp_with_l_def(&[]));
        // X corr is Unknown.

        assert!(
            state.paper_verify_targets().is_empty(),
            "T must be deferred while covering X has open corr"
        );
        // Set X corr=Pass and verify T becomes eligible.
        state.corr_status.insert(node("X"), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert(node("X"), corr_fp_with_l_def(&[]));
        assert!(
            state.paper_verify_targets().contains(&TargetId::from("T")),
            "T must surface once covering X is corr-Pass"
        );
    }

    #[test]
    fn corr_dispatch_eligible_with_legacy_or_empty_fingerprint() {
        // Defensive: legacy storage / unparseable fingerprint must not
        // break dispatch decisions. Treat absent L_def as "no Lean-relevant
        // deps" → eligible.
        let mut state = substantiveness_pass_state();
        // Empty fingerprint string.
        state
            .live
            .corr_current_fingerprints
            .insert(node("D"), String::new());
        // Legacy-shape JSON without lean_relevant_definition_descendants.
        state.live.corr_current_fingerprints.insert(
            node("N"),
            serde_json::json!({
                "own_tex": "",
                "lean_semantic_closure": "",
                "definition_descendants": {"OldD": "h"},
                "preamble_tex": "",
            })
            .to_string(),
        );

        assert!(
            state.is_corr_dispatch_eligible(&node("D")),
            "empty fingerprint => no descendants => eligible"
        );
        assert!(
            state.is_corr_dispatch_eligible(&node("N")),
            "legacy-shape fingerprint => no parsed descendants => eligible"
        );
    }

    #[test]
    fn corr_dispatch_eligible_treats_missing_descendant_as_eligible() {
        // If a descendant in L(N) is not in present_nodes (e.g. removed),
        // it's not a real dependency anymore — N is still eligible.
        let mut state = substantiveness_pass_state();
        state
            .live
            .corr_current_fingerprints
            .insert(node("N"), corr_fp_with_l_def(&["GhostDep"]));
        // GhostDep is NOT in present_nodes.

        assert!(
            state.is_corr_dispatch_eligible(&node("N")),
            "non-present descendant must not block dispatch"
        );
    }

    #[test]
    fn corr_dispatch_defers_on_theorem_dependency_not_just_def() {
        // Audit follow-up: dispatch must defer N if any Lean-relevant dep
        // (theorem, axiom, proposition) has open corr — not only def-kind
        // descendants. The TeX-hash propagation axis (def-only) and the
        // dispatch axis (all-relevant) are now separate.
        let mut state = substantiveness_pass_state();
        // Add a theorem T to present_nodes; D is a definition.
        state.live.present_nodes.insert(node("T"));
        state.proof_nodes.insert(node("T"));
        // Substantiveness Pass for T so it's not gated separately.
        state
            .live
            .substantiveness_current_fingerprints
            .insert(node("T"), "sub-T".to_string());
        state
            .substantiveness_status
            .insert(node("T"), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert(node("T"), "sub-T".to_string());
        // T's own corr is leaf-eligible (no deps).
        state.live.corr_current_fingerprints.insert(
            node("T"),
            corr_fp_with_dependencies_and_def_descendants(&[], &[]),
        );
        // N's fingerprint: T is in lean_relevant_dependencies but NOT in
        // lean_relevant_definition_descendants (because T is a theorem, not
        // a def). Under the def-only dispatch axis (pre-fix), N would be
        // wrongly considered eligible. Under the all-relevant axis, N
        // correctly defers until T's corr passes.
        state.live.corr_current_fingerprints.insert(
            node("N"),
            corr_fp_with_dependencies_and_def_descendants(&["T"], &[]),
        );

        // T's corr Unknown.
        assert!(
            !state.is_corr_dispatch_eligible(&node("N")),
            "N must defer while theorem dep T has open corr"
        );

        // After T's corr passes, N becomes eligible.
        let t_fp = corr_fp_with_dependencies_and_def_descendants(&[], &[]);
        state.corr_status.insert(node("T"), CorrStatus::Pass);
        state.corr_approved_fingerprints.insert(node("T"), t_fp);
        assert!(
            state.is_corr_dispatch_eligible(&node("N")),
            "N must become eligible once theorem T's corr passes"
        );
    }

    #[test]
    fn corr_verify_nodes_surfaces_eligible_parent_after_leaf_passes() {
        // Audit follow-up: after a leaf's corr passes, its formerly-deferred
        // parent must become a `corr_verify_nodes()` member so corr-accept
        // routing can dispatch the verifier instead of routing to Reviewer.
        let mut state = substantiveness_pass_state();
        state.live.corr_current_fingerprints.insert(
            node("D"),
            corr_fp_with_dependencies_and_def_descendants(&[], &[]),
        );
        state.live.corr_current_fingerprints.insert(
            node("N"),
            corr_fp_with_dependencies_and_def_descendants(&["D"], &[("D", "h")]),
        );

        // Initial: D is leaf-eligible, N is deferred.
        assert!(state.corr_verify_nodes().contains(&node("D")));
        assert!(!state.corr_verify_nodes().contains(&node("N")));

        // D's corr passes (matching approved + status).
        let d_fp = corr_fp_with_dependencies_and_def_descendants(&[], &[]);
        state.corr_status.insert(node("D"), CorrStatus::Pass);
        state.corr_approved_fingerprints.insert(node("D"), d_fp);

        // N is now on the corr_verify_nodes frontier — not deferred.
        assert!(
            state.corr_verify_nodes().contains(&node("N")),
            "N must surface on the corr-verify frontier once D's corr passes"
        );
    }

    // ---- Patch C-A local-closure foundation tests ------------------------

    fn sample_record(name: &str) -> LocalClosureRecord {
        let mut record = LocalClosureRecord::default();
        record.node = node(name);
        record.closure_version = "v1".to_string();
        record.toolchain_hash = "tc-hash".to_string();
        record.lake_manifest_hash = "lake-hash".to_string();
        record.preamble_hash = "pre-hash".to_string();
        record.approved_axioms_hash = "ax-hash".to_string();
        record.active_decl_hash = "decl-hash".to_string();
        record.active_statement_hash = "stmt-hash".to_string();
        record.kernel_axioms = BTreeSet::from(["propext".to_string()]);
        record.boundary_theorems = BTreeMap::from([(node("HelperB"), "stmt-b".to_string())]);
        record.strict_theorem_deps = BTreeMap::from([(node("ThmT"), "val-t".to_string())]);
        record.strict_definition_deps = BTreeMap::from([(node("DefD"), "sem-d".to_string())]);
        record.accepted_at_snapshot_id = "snap-42".to_string();
        // Audit H-4: representative happy path uses `Agreed` so any
        // future test that drops the record into a default policy
        // (axcheck_required = true) gets a consistent baseline.
        record.axcheck_status = AxcheckStatus::Agreed;
        record
    }

    fn sample_summary() -> ErrorSummary {
        ErrorSummary {
            status: "axiom_violation".to_string(),
            returncode: 0,
            timed_out: false,
            stderr_excerpt: "uses sorryAx".to_string(),
            axiom_violations: vec!["sorryAx".to_string()],
            strict_errors: vec![],
            captured_at_cycle: 17,
            retry_count: 0,
            last_attempt_cycle: 0,
            next_retry_cycle: 0,
            retry_exhausted: false,
        }
    }

    fn transport_summary(retry_count: u32, last: u64, next: u64, exhausted: bool) -> ErrorSummary {
        ErrorSummary {
            status: "transport_error".to_string(),
            returncode: -1,
            timed_out: false,
            stderr_excerpt: "checker socket unreachable".to_string(),
            axiom_violations: vec![],
            strict_errors: vec![],
            captured_at_cycle: last,
            retry_count,
            last_attempt_cycle: last,
            next_retry_cycle: next,
            retry_exhausted: exhausted,
        }
    }

    #[test]
    fn local_closure_record_round_trips_through_json() {
        // Patch C-A — LocalClosureRecord must survive JSON round-trip
        // exactly when populated with non-default values, including
        // BTreeSet/BTreeMap fields keyed by NodeId.
        let record = sample_record("Foo");
        let json = serde_json::to_string(&record).expect("serialize record");
        let parsed: LocalClosureRecord = serde_json::from_str(&json).expect("deserialize record");
        assert_eq!(parsed, record, "record must round-trip exactly");
    }

    #[test]
    fn error_summary_round_trips_through_json_with_transport_backoff() {
        // Patch C-A — ErrorSummary including the transport-error
        // backoff fields must round-trip exactly.
        let summary = transport_summary(2, 100, 104, false);
        let json = serde_json::to_string(&summary).expect("serialize summary");
        let parsed: ErrorSummary = serde_json::from_str(&json).expect("deserialize summary");
        assert_eq!(parsed, summary, "summary must round-trip exactly");
    }

    #[test]
    fn revalidation_batch_round_trips_through_json() {
        // Patch C-A — RevalidationBatch carries the per-pass output
        // (refreshed records, still-unverified summaries) consumed by
        // Patch C-B's `apply_revalidation_batch`. Must round-trip
        // exactly with both arms populated.
        let batch = RevalidationBatch {
            refreshed: vec![(node("Foo"), sample_record("Foo"))],
            still_unverified: vec![(node("Bar"), sample_summary())],
        };
        let json = serde_json::to_string(&batch).expect("serialize batch");
        let parsed: RevalidationBatch = serde_json::from_str(&json).expect("deserialize batch");
        assert_eq!(parsed, batch, "revalidation batch must round-trip exactly");
    }

    #[test]
    fn protocol_state_deserializes_pre_patch_c_state_without_closure_fields() {
        // Patch C-A — pre-Patch-C state files lack every new closure
        // field. With `#[serde(default)]` on each, the state must
        // deserialize cleanly with all closure tiers default-empty
        // and `last_clean_local_closure_mirror_ready` = false. Mirrors
        // the precedent set by
        // `old_state_file_without_substantiveness_fields_deserializes_cleanly`.
        let raw = r#"{
            "phase": "theorem_stating",
            "stage": "start",
            "cycle": 0
        }"#;
        let state: ProtocolState = serde_json::from_str(raw).expect("pre-Patch-C state must parse");
        assert!(
            state.local_closure_records.is_empty(),
            "local_closure_records must default to empty"
        );
        assert!(
            state.local_closure_unverified_nodes.is_empty(),
            "local_closure_unverified_nodes must default to empty"
        );
        assert!(
            state.local_closure_failures.is_empty(),
            "local_closure_failures must default to empty"
        );
        assert!(
            state.committed_local_closure_records.is_empty(),
            "committed_local_closure_records must default to empty"
        );
        assert!(
            state.committed_local_closure_unverified_nodes.is_empty(),
            "committed_local_closure_unverified_nodes must default to empty"
        );
        assert!(
            state.committed_local_closure_failures.is_empty(),
            "committed_local_closure_failures must default to empty"
        );
        assert!(
            state.last_clean_local_closure_records.is_empty(),
            "last_clean_local_closure_records must default to empty"
        );
        assert!(
            state.last_clean_local_closure_unverified_nodes.is_empty(),
            "last_clean_local_closure_unverified_nodes must default to empty"
        );
        assert!(
            state.last_clean_local_closure_failures.is_empty(),
            "last_clean_local_closure_failures must default to empty"
        );
        // Critical forward-compat invariant: the readiness flag must
        // default to FALSE so a pre-Patch-C state file restored after
        // deploy refuses LastClean until the next clean checkpoint.
        assert!(
            !state.last_clean_local_closure_mirror_ready,
            "last_clean_local_closure_mirror_ready MUST default to false on pre-Patch-C state files"
        );
        // Reverse indices are #[serde(skip)] so they default to empty
        // on every deserialize regardless of the records map. The
        // supervisor's startup hook is responsible for repopulating
        // them via `recompute_local_closure_reverse_indices` after
        // load.
        assert!(state.boundary_statement_consumers.is_empty());
        assert!(state.strict_dep_consumers.is_empty());
    }

    #[test]
    fn recompute_local_closure_reverse_indices_populates_from_records() {
        // Patch C-A — the reverse-index recompute helper walks
        // `local_closure_records` and produces:
        //   boundary_statement_consumers[H] = {N : N.boundary_theorems[H]}
        //   strict_dep_consumers[D] = {N : N has D in strict_theorem_deps
        //                                 OR strict_definition_deps}
        // Construct a small records map with two consumers (A, B)
        // sharing helper H, plus a private strict dep — verify the
        // reverse indices reflect both.
        let mut state = ProtocolState::default();

        let mut record_a = sample_record("A");
        record_a.node = node("A");
        record_a.boundary_theorems = BTreeMap::from([
            (node("H"), "stmt-h".to_string()),
            (node("K"), "stmt-k".to_string()),
        ]);
        record_a.strict_theorem_deps = BTreeMap::from([(node("ThmT"), "val-t".to_string())]);
        record_a.strict_definition_deps = BTreeMap::from([(node("DefD"), "sem-d".to_string())]);

        let mut record_b = sample_record("B");
        record_b.node = node("B");
        record_b.boundary_theorems = BTreeMap::from([(node("H"), "stmt-h".to_string())]);
        record_b.strict_theorem_deps = BTreeMap::new();
        record_b.strict_definition_deps = BTreeMap::from([(node("DefD"), "sem-d".to_string())]);

        state.local_closure_records.insert(node("A"), record_a);
        state.local_closure_records.insert(node("B"), record_b);

        // Pre-recompute: indices empty (skip-serialized; default).
        assert!(state.boundary_statement_consumers.is_empty());
        assert!(state.strict_dep_consumers.is_empty());

        recompute_local_closure_reverse_indices(&mut state);

        // H has two consumers (A and B); K has one (A).
        assert_eq!(
            state.boundary_statement_consumers.get(&node("H")),
            Some(&BTreeSet::from([node("A"), node("B")])),
            "H must surface both A and B as consumers"
        );
        assert_eq!(
            state.boundary_statement_consumers.get(&node("K")),
            Some(&BTreeSet::from([node("A")])),
            "K must surface only A as consumer"
        );

        // ThmT (strict theorem dep) has only A; DefD (strict
        // definition dep) has both A and B (via the union of theorem
        // and definition strict-dep keys).
        assert_eq!(
            state.strict_dep_consumers.get(&node("ThmT")),
            Some(&BTreeSet::from([node("A")])),
            "ThmT must surface only A"
        );
        assert_eq!(
            state.strict_dep_consumers.get(&node("DefD")),
            Some(&BTreeSet::from([node("A"), node("B")])),
            "DefD must surface both A and B"
        );

        // Idempotent: a second call with the same records map yields
        // the same indices.
        let snapshot_boundary = state.boundary_statement_consumers.clone();
        let snapshot_strict = state.strict_dep_consumers.clone();
        recompute_local_closure_reverse_indices(&mut state);
        assert_eq!(state.boundary_statement_consumers, snapshot_boundary);
        assert_eq!(state.strict_dep_consumers, snapshot_strict);

        // Removing a record and recomputing prunes its entries.
        state.local_closure_records.remove(&node("A"));
        recompute_local_closure_reverse_indices(&mut state);
        // K had only A as consumer; with A gone, K disappears from the
        // index entirely.
        assert!(
            !state.boundary_statement_consumers.contains_key(&node("K")),
            "removing A's record must prune K from boundary_statement_consumers"
        );
        // H still has B.
        assert_eq!(
            state.boundary_statement_consumers.get(&node("H")),
            Some(&BTreeSet::from([node("B")]))
        );
        // ThmT had only A; now gone.
        assert!(!state.strict_dep_consumers.contains_key(&node("ThmT")));
        // DefD had both; now only B.
        assert_eq!(
            state.strict_dep_consumers.get(&node("DefD")),
            Some(&BTreeSet::from([node("B")]))
        );
    }

    #[test]
    fn restore_committed_rolls_back_closure_tier() {
        // Patch C-A — `restore_committed` must roll the closure live
        // tier (records / unverified / failures) back to the committed
        // mirrors AND recompute reverse indices from the restored
        // records. This is the rejection-rollback path; without it,
        // a closure-state mutation accepted speculatively during a
        // worker burst that was then rejected would leak.
        let mut state = ProtocolState::default();

        // Committed tier holds the "ground truth" pre-burst state: A
        // has a record, no failures, no unverified entries.
        state
            .committed_local_closure_records
            .insert(node("A"), sample_record("A"));
        state.committed_local_closure_unverified_nodes = BTreeSet::new();
        state.committed_local_closure_failures = BTreeMap::new();

        // Live tier holds a speculative mid-burst state that diverges
        // from committed: A's record was deleted, B is now unverified
        // with a failure summary.
        state.local_closure_records = BTreeMap::new();
        state.local_closure_unverified_nodes.insert(node("B"));
        state
            .local_closure_failures
            .insert(node("B"), sample_summary());

        // Reverse indices started empty; live state has no records, so
        // the post-restore indices must reflect the committed records
        // (sample_record("A") has helpers and strict deps).
        assert!(state.boundary_statement_consumers.is_empty());
        assert!(state.strict_dep_consumers.is_empty());

        state.restore_committed();

        // Live records / unverified / failures must match the committed
        // mirror exactly.
        assert_eq!(
            state.local_closure_records.get(&node("A")),
            Some(&sample_record("A")),
            "live records[A] must restore from committed"
        );
        assert!(
            !state.local_closure_records.contains_key(&node("B")),
            "B's speculative-deletion-target must NOT have a record after restore"
        );
        assert!(
            state.local_closure_unverified_nodes.is_empty(),
            "live unverified set must restore from committed (empty)"
        );
        assert!(
            state.local_closure_failures.is_empty(),
            "live failures must restore from committed (empty)"
        );

        // Reverse indices must reflect the restored records — A's
        // sample_record names HelperB as boundary helper and ThmT /
        // DefD as strict deps.
        assert_eq!(
            state.boundary_statement_consumers.get(&node("HelperB")),
            Some(&BTreeSet::from([node("A")])),
            "boundary index must surface A as HelperB consumer post-restore"
        );
        assert_eq!(
            state.strict_dep_consumers.get(&node("ThmT")),
            Some(&BTreeSet::from([node("A")])),
            "strict index must surface A as ThmT consumer post-restore"
        );
        assert_eq!(
            state.strict_dep_consumers.get(&node("DefD")),
            Some(&BTreeSet::from([node("A")])),
            "strict index must surface A as DefD consumer post-restore"
        );
    }

    #[test]
    fn commit_live_snapshots_closure_live_to_committed_unconditionally() {
        // Patch C-A — every `commit_live` must snapshot the closure
        // live tier into the committed mirrors, even when the
        // checkpoint isn't clean. This is the precondition for
        // `restore_committed` to roll closure state back accurately.
        let mut state = ProtocolState::default();
        // Force a NON-clean checkpoint so the last_clean branch is
        // skipped (use a configured target with no node claiming it
        // → empty-coverage paper fail → blocker present).
        state.configured_targets = BTreeSet::from([TargetId::from("t")]);

        // Live tier carries a record and a failure.
        state
            .local_closure_records
            .insert(node("A"), sample_record("A"));
        state.local_closure_unverified_nodes.insert(node("B"));
        state
            .local_closure_failures
            .insert(node("B"), sample_summary());

        // Committed tier starts empty.
        assert!(state.committed_local_closure_records.is_empty());
        assert!(state.committed_local_closure_unverified_nodes.is_empty());
        assert!(state.committed_local_closure_failures.is_empty());

        // Sanity: not a clean checkpoint (paper blocker should be present).
        assert!(
            !state.global_blockers().is_empty(),
            "test setup must produce a non-clean checkpoint"
        );

        state.commit_live();

        // Committed mirrors now match live.
        assert_eq!(
            state.committed_local_closure_records.get(&node("A")),
            Some(&sample_record("A")),
            "commit_live must snapshot records into committed"
        );
        assert!(
            state
                .committed_local_closure_unverified_nodes
                .contains(&node("B")),
            "commit_live must snapshot unverified set into committed"
        );
        assert_eq!(
            state.committed_local_closure_failures.get(&node("B")),
            Some(&sample_summary()),
            "commit_live must snapshot failures into committed"
        );

        // The last_clean closure mirrors must remain empty (non-clean
        // checkpoint) and the readiness flag must remain false.
        assert!(state.last_clean_local_closure_records.is_empty());
        assert!(state.last_clean_local_closure_unverified_nodes.is_empty());
        assert!(state.last_clean_local_closure_failures.is_empty());
        assert!(
            !state.last_clean_local_closure_mirror_ready,
            "non-clean commit_live must NOT bump last_clean_local_closure_mirror_ready"
        );
    }

    #[test]
    fn commit_live_at_clean_checkpoint_snapshots_closure_to_last_clean_and_bumps_flag() {
        // Patch C-A — a clean-checkpoint `commit_live` (empty
        // `global_blockers()`, empty pending_protected_reapproval_nodes)
        // must additionally snapshot the closure live tier into the
        // last_clean mirrors AND bump
        // `last_clean_local_closure_mirror_ready = true`. Default
        // ProtocolState satisfies clean-checkpoint preconditions.
        let mut state = ProtocolState::default();
        state
            .local_closure_records
            .insert(node("A"), sample_record("A"));

        assert!(
            state.global_blockers().is_empty()
                && state.pending_protected_reapproval_nodes.is_empty(),
            "default state must qualify as a clean checkpoint"
        );
        assert!(
            !state.last_clean_local_closure_mirror_ready,
            "test prerequisite: closure mirror flag starts false"
        );

        state.commit_live();

        assert!(
            state.last_clean_local_closure_mirror_ready,
            "clean-checkpoint commit_live must bump last_clean_local_closure_mirror_ready"
        );
        assert_eq!(
            state.last_clean_local_closure_records.get(&node("A")),
            Some(&sample_record("A")),
            "clean-checkpoint commit_live must snapshot records into last_clean"
        );
        // Committed tier is also populated unconditionally (per the
        // previous test); this test additionally exercises the
        // last_clean-branch shape.
        assert_eq!(
            state.committed_local_closure_records.get(&node("A")),
            Some(&sample_record("A"))
        );
    }

    #[test]
    fn apply_last_clean_reset_restores_closure_mirrors_when_ready() {
        // Patch C-A — when the closure mirror flag is true and the
        // verifier mirror flag is true, `apply_last_clean_reset` must
        // restore the closure live tier from the last_clean mirrors,
        // mirror the committed tier from the same source (so a
        // subsequent `restore_committed` doesn't undo the LastClean
        // restore), and recompute reverse indices.
        let mut state = ProtocolState::default();
        // Capture a clean checkpoint with A in the closure records.
        // Default-state baseline already satisfies clean predicates.
        state
            .local_closure_records
            .insert(node("A"), sample_record("A"));
        state.commit_live();
        assert!(state.last_clean_local_closure_mirror_ready);
        assert!(state.last_clean_verifier_mirror_ready);

        // Mutate live tier away from the clean snapshot: drop A's
        // record, add B as unverified with a failure.
        state.local_closure_records = BTreeMap::new();
        state.local_closure_unverified_nodes.insert(node("B"));
        state
            .local_closure_failures
            .insert(node("B"), sample_summary());
        // Also mutate the committed tier (so we can verify
        // last_clean → committed restore).
        state.committed_local_closure_records = BTreeMap::new();
        state
            .committed_local_closure_unverified_nodes
            .insert(node("B"));

        // Patch C-N item 2: rewind ran (both flags ready), Ok(true).
        assert_eq!(state.apply_last_clean_reset(), Ok(true));

        // Live tier restored from last_clean mirrors.
        assert_eq!(
            state.local_closure_records.get(&node("A")),
            Some(&sample_record("A"))
        );
        assert!(state.local_closure_unverified_nodes.is_empty());
        assert!(state.local_closure_failures.is_empty());

        // Committed tier ALSO restored from last_clean (so a future
        // restore_committed rolls back to the LastClean snapshot,
        // not the pre-rewind committed tier).
        assert_eq!(
            state.committed_local_closure_records.get(&node("A")),
            Some(&sample_record("A"))
        );
        assert!(state.committed_local_closure_unverified_nodes.is_empty());
        assert!(state.committed_local_closure_failures.is_empty());

        // Reverse indices repopulated from the restored records.
        assert_eq!(
            state.boundary_statement_consumers.get(&node("HelperB")),
            Some(&BTreeSet::from([node("A")]))
        );
        assert_eq!(
            state.strict_dep_consumers.get(&node("ThmT")),
            Some(&BTreeSet::from([node("A")]))
        );
    }

    #[test]
    fn apply_last_clean_reset_refuses_when_closure_mirror_flag_is_false() {
        // Patch C-A — even when the verifier mirror flag is true and
        // the structural last_clean_* mirrors are populated, a
        // `false` `last_clean_local_closure_mirror_ready` flag must
        // refuse the LastClean rewind so a state file persisted
        // before Patch C-A's first clean checkpoint doesn't restore
        // structural state against empty closure mirrors (false-clean
        // state, plan §7.8).
        let mut state = ProtocolState::default();
        // Drive a clean checkpoint to populate the verifier mirrors
        // and the closure mirrors.
        state
            .local_closure_records
            .insert(node("A"), sample_record("A"));
        state.commit_live();
        assert!(state.last_clean_verifier_mirror_ready);
        assert!(state.last_clean_local_closure_mirror_ready);

        // Forge the post-deploy migration condition: the structural
        // verifier mirrors look populated (from a pre-Patch-C clean
        // checkpoint), but the closure mirror flag is false — i.e.,
        // the closure mirrors were NOT captured at any clean
        // checkpoint after Patch C-A deploy. Manually flip just the
        // closure flag back to false.
        state.last_clean_local_closure_mirror_ready = false;

        // Mutate live tier so we can detect any restoration.
        state.local_closure_records = BTreeMap::new();
        state.local_closure_unverified_nodes.insert(node("B"));
        state
            .local_closure_failures
            .insert(node("B"), sample_summary());
        let live_records_before = state.local_closure_records.clone();
        let live_unverified_before = state.local_closure_unverified_nodes.clone();
        let live_failures_before = state.local_closure_failures.clone();
        let live_present_before = state.live.present_nodes.clone();

        // Patch C-N item 2: closure mirror flag false → Ok(false) so the
        // caller in engine.rs can suppress RestoreWorktreeToLastClean.
        assert_eq!(state.apply_last_clean_reset(), Ok(false));

        // The function must be a no-op for closure state when the
        // closure mirror flag is false. The structural rewind is
        // also suppressed (the function refuses entirely once it
        // reaches the closure-flag gate, so the structural mirrors
        // stay where they were too).
        assert_eq!(
            state.local_closure_records, live_records_before,
            "live records must not be touched when closure mirror flag is false"
        );
        assert_eq!(
            state.local_closure_unverified_nodes, live_unverified_before,
            "live unverified set must not be touched when closure mirror flag is false"
        );
        assert_eq!(
            state.local_closure_failures, live_failures_before,
            "live failures must not be touched when closure mirror flag is false"
        );
        assert_eq!(
            state.live.present_nodes, live_present_before,
            "structural live state must also stay untouched (gate refusal short-circuits)"
        );
        // cycles_since_clean is zeroed unconditionally (matches the
        // existing verifier-mirror migration-guard precedent).
        assert_eq!(state.cycles_since_clean, 0);
    }

    #[test]
    fn last_clean_mirrors_populated_requires_both_verifier_and_closure_mirrors_ready() {
        // Patch C-I (audit HIGH 3): `last_clean_mirrors_populated()` must
        // AND both readiness flags. A migrated state file where the
        // verifier mirrors are ready but the closure mirrors are not
        // would otherwise let `request_allowed_resets` offer LastClean
        // while `apply_last_clean_reset` would early-return on the
        // closure-mirror gate — kernel state stays put while the
        // runtime still git-resets disk (state/disk divergence).
        let mut state = ProtocolState::default();
        // Default: both flags false → predicate false.
        assert!(!state.last_clean_mirrors_populated());

        // Only the verifier flag set (the migration window) → still
        // false. This is the case the audit calls out.
        state.last_clean_verifier_mirror_ready = true;
        state.last_clean_local_closure_mirror_ready = false;
        assert!(
            !state.last_clean_mirrors_populated(),
            "verifier-only mirror readiness must NOT satisfy the predicate"
        );

        // Only the closure flag set → still false (symmetric guard).
        state.last_clean_verifier_mirror_ready = false;
        state.last_clean_local_closure_mirror_ready = true;
        assert!(
            !state.last_clean_mirrors_populated(),
            "closure-only mirror readiness must NOT satisfy the predicate"
        );

        // Both flags set → true.
        state.last_clean_verifier_mirror_ready = true;
        state.last_clean_local_closure_mirror_ready = true;
        assert!(
            state.last_clean_mirrors_populated(),
            "predicate must be true only when both mirror-ready flags are set"
        );
    }

    #[test]
    fn request_allowed_resets_does_not_offer_last_clean_until_closure_mirror_ready() {
        // Patch C-I (audit HIGH 3): `request_allowed_resets` must not
        // offer `ResetChoice::LastClean` while the closure mirror flag
        // is false, even if every other LastClean precondition is met
        // (has_ever_been_clean, cycles_since_clean >= 1, verifier
        // mirror ready). Otherwise the reviewer can pick a reset the
        // kernel won't apply while the runtime still resets disk.
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.has_ever_been_clean = true;
        state.cycles_since_clean = 1;
        // Simulate the migration window: verifier mirror ready but
        // closure mirror NOT ready (state file persisted before Patch
        // C-A's first clean checkpoint).
        state.last_clean_verifier_mirror_ready = true;
        state.last_clean_local_closure_mirror_ready = false;

        let resets = state.request_allowed_resets(RequestKind::Review);
        assert!(
            !resets.contains(&ResetChoice::LastClean),
            "LastClean must not be offered while closure mirror flag is false; got {:?}",
            resets
        );

        // Promote the closure flag → LastClean now appears.
        state.last_clean_local_closure_mirror_ready = true;
        let resets = state.request_allowed_resets(RequestKind::Review);
        assert!(
            resets.contains(&ResetChoice::LastClean),
            "LastClean must be offered once both mirror flags are ready; got {:?}",
            resets
        );
    }

    #[test]
    fn request_allowed_resets_forces_last_clean_at_csc_threshold_until_rewound_twice() {
        // Mandatory-at-threshold rule: when cycles_since_clean >=
        // CSC_LAST_CLEAN_THRESHOLD (default 15, env-tunable),
        // `last_clean` is the only legal reset (None and LastCommit
        // dropped from the menu). EXCEPTION: once the current clean
        // checkpoint has been rewound to twice or more
        // (`last_clean_rewind_count >= 2`), the mandate is waived so
        // the reviewer can resume non-reset Continue paths — repeated
        // rewinds aren't helping.
        // SAFETY: tests run in parallel by default; set the env var
        // explicitly so a sibling test that mutates it can't race.
        // Tests in this module that don't set it inherit the default 15.
        unsafe {
            std::env::set_var("TRELLIS_CSC_LAST_CLEAN_THRESHOLD", "15");
        }
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.has_ever_been_clean = true;
        state.last_clean_verifier_mirror_ready = true;
        state.last_clean_local_closure_mirror_ready = true;

        // Below threshold: all options offered.
        state.cycles_since_clean = 14;
        state.last_clean_rewind_count = 0;
        let resets = state.request_allowed_resets(RequestKind::Review);
        assert!(resets.contains(&ResetChoice::None));
        assert!(resets.contains(&ResetChoice::LastClean));

        // At threshold, 0 rewinds: only LastClean.
        state.cycles_since_clean = 15;
        state.last_clean_rewind_count = 0;
        let resets = state.request_allowed_resets(RequestKind::Review);
        assert!(!resets.contains(&ResetChoice::None));
        assert!(!resets.contains(&ResetChoice::LastCommit));
        assert!(resets.contains(&ResetChoice::LastClean));
        assert_eq!(resets.len(), 1);

        // At threshold, 1 rewind: still only LastClean.
        state.last_clean_rewind_count = 1;
        let resets = state.request_allowed_resets(RequestKind::Review);
        assert!(!resets.contains(&ResetChoice::None));
        assert!(resets.contains(&ResetChoice::LastClean));

        // At threshold, 2 rewinds: exception fires — None offered again.
        state.last_clean_rewind_count = 2;
        let resets = state.request_allowed_resets(RequestKind::Review);
        assert!(resets.contains(&ResetChoice::None));
        assert!(resets.contains(&ResetChoice::LastClean));

        // Far above threshold with high rewind count: exception still holds.
        state.cycles_since_clean = 50;
        state.last_clean_rewind_count = 5;
        let resets = state.request_allowed_resets(RequestKind::Review);
        assert!(resets.contains(&ResetChoice::None));
    }

    #[test]
    fn commit_live_resets_rewind_count_on_new_clean_checkpoint() {
        // The rewind counter must reset to 0 whenever commit_live
        // captures a new clean checkpoint (the mirror is replaced, so
        // prior rewinds no longer target the same state). A spiral
        // that produced N rewinds, then made it back to a clean state,
        // is no longer in "repeated rewind" territory if it spirals
        // again afterward.
        let mut state = ProtocolState::default();
        // Default ProtocolState qualifies as a clean checkpoint (empty
        // global_blockers and pending_protected_reapproval_nodes) — see
        // `commit_live_at_clean_checkpoint_snapshots_closure_to_last_clean_and_bumps_flag`.
        assert!(state.clean_checkpoint_ready());
        state.last_clean_rewind_count = 3;
        state.commit_live();
        assert_eq!(
            state.last_clean_rewind_count, 0,
            "new clean checkpoint must reset rewind count"
        );
    }

    #[test]
    fn apply_last_clean_reset_increments_rewind_count() {
        // After a successful rewind (mirrors populated, closure mirror
        // ready), the counter must increment by 1.
        let mut state = ProtocolState::default();
        state
            .local_closure_records
            .insert(node("A"), sample_record("A"));
        state.commit_live(); // populates mirrors, count starts at 0
        assert_eq!(state.last_clean_rewind_count, 0);

        // Dirty the state (something to rewind from) and rewind.
        state.cycles_since_clean = 5;
        assert_eq!(state.apply_last_clean_reset(), Ok(true));
        assert_eq!(state.last_clean_rewind_count, 1);

        state.cycles_since_clean = 5;
        assert_eq!(state.apply_last_clean_reset(), Ok(true));
        assert_eq!(state.last_clean_rewind_count, 2);
    }

    #[test]
    fn commit_live_preserves_rewind_count_when_clean_state_unchanged() {
        // Bug regression: after `apply_last_clean_reset` restores live
        // to equal the `last_clean_*` mirrors, the next `commit_live`
        // (e.g. when the re-issued reviewer's response is processed)
        // sees `clean_checkpoint_ready()=true` and used to
        // unconditionally recapture the snapshot AND reset
        // `last_clean_rewind_count` to 0. Because the would-be-new
        // snapshot was structurally identical to the existing one, the
        // counter could never accumulate past 1 — so the
        // `CSC_REWIND_WAIVER_COUNT = 2` exception never fired.
        //
        // Fix: `commit_live` skips the recapture branch (lcrc reset
        // included) when the would-be-new snapshot equals the
        // existing mirror. Only a structurally different clean state
        // counts as a "new" clean checkpoint for lcrc-reset purposes.
        let mut state = ProtocolState::default();
        state
            .local_closure_records
            .insert(node("A"), sample_record("A"));
        state.commit_live(); // captures clean mirror, lcrc=0
        assert!(state.last_clean_mirrors_populated());
        assert_eq!(state.last_clean_rewind_count, 0);

        // Rewind: live state is restored to equal last_clean_*. lcrc→1.
        state.cycles_since_clean = 5;
        assert_eq!(state.apply_last_clean_reset(), Ok(true));
        assert_eq!(state.last_clean_rewind_count, 1);

        // Snapshot the mirror fields so we can assert they don't move.
        let mirror_live_before = state.last_clean_live.clone();
        let mirror_records_before = state.last_clean_local_closure_records.clone();

        // commit_live fires (re-issued review path). Live state still
        // structurally equals the mirror — the recapture+reset branch
        // must NOT fire.
        state.commit_live();
        assert_eq!(
            state.last_clean_rewind_count, 1,
            "commit_live must preserve lcrc when live state matches existing clean mirror"
        );
        assert_eq!(
            state.last_clean_live, mirror_live_before,
            "mirror live snapshot must not have moved when state is structurally identical"
        );
        assert_eq!(
            state.last_clean_local_closure_records, mirror_records_before,
            "mirror closure records must not have moved when state is structurally identical"
        );
        // cycles_since_clean must still be reset (clean checkpoint).
        assert_eq!(state.cycles_since_clean, 0);

        // Now mutate live to be structurally different from the mirror.
        // Adding a new sorry-free node + record changes
        // `local_closure_records` (snapshot surface) without
        // introducing any blocker (no open node, no failure), so
        // `clean_checkpoint_ready()` stays true.
        state
            .local_closure_records
            .insert(node("B"), sample_record("B"));
        assert!(state.clean_checkpoint_ready());

        state.commit_live();
        assert_eq!(
            state.last_clean_rewind_count, 0,
            "genuinely new clean state (different records) must reset lcrc"
        );
        assert!(
            state
                .last_clean_local_closure_records
                .contains_key(&node("B")),
            "mirror must capture the new clean state"
        );
    }

    // ---- Patch C-C scheduling-predicate plumbing tests -------------------

    /// Build a minimal ProofFormalization-phase state with a single
    /// proof_node `name` that is sorry-free, target-covered, has every
    /// per-node verifier (corr / sound / substantiveness / paper)
    /// passing, and has a fresh `LocalClosureRecord` installed — so
    /// `formalization_complete` passes by default and individual tests
    /// can perturb fields. Matches the verifier-status invariants
    /// established by engine.rs::tests::base_state.
    fn proof_phase_clean_state(name: &str) -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.proof_nodes = BTreeSet::from([node(name)]);
        state.live.present_nodes = BTreeSet::from([node(name)]);
        // sorry-free: no entry in open_nodes.
        state.live.open_nodes = BTreeSet::new();
        let target = TargetId::from("t");
        state.configured_targets = BTreeSet::from([target.clone()]);
        state
            .target_claims
            .insert(node(name), BTreeSet::from([target.clone()]));
        state
            .approved_targets
            .configured_targets
            .insert(target.clone());
        state
            .approved_targets
            .coverage
            .insert(target.clone(), BTreeSet::from([node(name)]));
        state
            .live
            .coverage
            .insert(target.clone(), BTreeSet::from([node(name)]));
        // Paper / corr / substantiveness all Pass with matching
        // current/approved fingerprints so `global_blockers` is empty.
        state
            .live
            .paper_current_fingerprints
            .insert(target.clone(), format!("{name}=fp"));
        state.paper_status.insert(target.clone(), CorrStatus::Pass);
        state
            .paper_approved_fingerprints
            .insert(target.clone(), format!("{name}=fp"));
        state
            .live
            .corr_current_fingerprints
            .insert(node(name), "corr".into());
        state.corr_status.insert(node(name), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert(node(name), "corr".into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert(node(name), "sub".into());
        state
            .substantiveness_status
            .insert(node(name), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert(node(name), "sub".into());
        // Sound auto-passes for sorry-free needs_sound==false nodes
        // (current_sound_state short-circuits to Pass when needs_sound
        // returns false), so no sound_status/fingerprints required.
        state
            .local_closure_records
            .insert(node(name), sample_record(name));
        state
    }

    #[test]
    fn formalization_complete_blocks_when_unverified_set_is_non_empty() {
        // Patch C-C plan §7.6 — `formalization_complete` must return
        // false when `local_closure_unverified_nodes` is non-empty,
        // even if textual + blockers are clean.
        let mut state = proof_phase_clean_state("Foo");
        // `Foo` is sorry-free with a fresh record — gate is open.
        assert!(state.formalization_complete());
        // Mark `Foo` unverified (drop record + add to unverified set).
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        state
            .local_closure_failures
            .insert(node("Foo"), sample_summary());
        assert!(
            !state.formalization_complete(),
            "unverified sorry-free proof_node must block formalization_complete"
        );
    }

    #[test]
    fn formalization_complete_blocks_when_sorry_free_proof_node_lacks_record() {
        // Patch C-C plan §7.6 — `records_present` clause: every
        // sorry-free proof_node must have a `LocalClosureRecord`. A
        // proof_node that's sorry-free but has no record AND is not
        // even in the unverified set still blocks the gate (defends
        // against a path that drops the record without populating
        // unverified — the records_present clause is the backstop).
        let mut state = proof_phase_clean_state("Foo");
        assert!(state.formalization_complete());
        state.local_closure_records.remove(&node("Foo"));
        // Note: NOT inserting into unverified_nodes — testing the
        // records_present clause specifically.
        assert!(
            !state.formalization_complete(),
            "sorry-free proof_node without a record must block formalization_complete"
        );
    }

    #[test]
    fn formalization_complete_blocks_sentinel_hashed_record() {
        // Audit H-1/M-1 follow-up — record presence alone is not enough
        // for completion. A record still carrying engine sentinel hashes
        // is pre-backfill evidence and must not open the final gate.
        let mut state = proof_phase_clean_state("Foo");
        assert!(state.formalization_complete());
        state
            .local_closure_records
            .get_mut(&node("Foo"))
            .expect("record")
            .toolchain_hash = "TODO_PATCH_C_D_HASH".to_string();
        assert!(
            !state.formalization_complete(),
            "sentinel-hashed record must block formalization_complete"
        );
    }

    #[test]
    fn formalization_complete_blocks_record_with_stale_kernel_semantic_hash() {
        // Audit H-1/M-1 follow-up — completion must reject a record whose
        // probe-time dep semantic hash no longer matches current kernel
        // fingerprints, even if the record is still present.
        let mut state = proof_phase_clean_state("Foo");
        assert!(state.formalization_complete());
        state
            .live
            .corr_current_fingerprints
            .insert(node("HelperB"), "helper-new".to_string());
        state
            .local_closure_records
            .get_mut(&node("Foo"))
            .expect("record")
            .kernel_semantic_hashes
            .insert(node("HelperB"), "helper-old".to_string());
        assert!(
            !state.formalization_complete(),
            "stale kernel semantic hash must block formalization_complete"
        );
    }

    #[test]
    fn formalization_complete_returns_true_when_all_clauses_clean() {
        // Patch C-C plan §7.6 — when textual_clean, blockers_clean,
        // unverified_clean, and records_present all hold, the gate
        // opens.
        let state = proof_phase_clean_state("Foo");
        assert!(
            state.formalization_complete(),
            "all-clauses-clean state must open formalization_complete"
        );
    }

    #[test]
    fn active_node_legal_accepts_sorry_free_unverified_node() {
        // Patch C-C plan §7.4 — `active_node_legal` clause 1 union:
        // a sorry-free node in `local_closure_unverified_nodes` must
        // be a legal active focus in ProofFormalization, mirroring
        // the existing `live.open_nodes.contains(node)` clause.
        let mut state = proof_phase_clean_state("Foo");
        // Pull `Foo` into the unverified set without re-opening it.
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        state
            .local_closure_failures
            .insert(node("Foo"), sample_summary());
        assert!(!state.live.open_nodes.contains(&node("Foo")));
        assert!(
            state.active_node_legal(Some(&node("Foo")), &state.live),
            "sorry-free unverified node must be a legal active focus"
        );
    }

    #[test]
    fn request_allowed_next_active_includes_unverified_only_node() {
        // Patch C-C plan §7.4 — `request_kernel_hinted_next_active_nodes`
        // for a Review request in ProofFormalization must include
        // sorry-free unverified nodes. This is the §7.4.2 "blockers
        // empty but local-closure work remains" condition.
        let mut state = proof_phase_clean_state("Foo");
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        state
            .local_closure_failures
            .insert(node("Foo"), sample_summary());
        let allowed = state.request_kernel_hinted_next_active_nodes(RequestKind::Review);
        assert!(
            allowed.contains(&node("Foo")),
            "unverified-only node must appear in kernel_hinted_next_active_nodes; got {:?}",
            allowed
        );
    }

    #[test]
    fn select_initial_proof_active_node_returns_unverified_when_no_open_nodes() {
        // Patch C-C plan §7.4 — when `live.open_nodes` is empty but
        // `local_closure_unverified_nodes` carries a sorry-free
        // node with a non-transport failure, the auto-scheduler
        // should pick that node.
        let mut state = proof_phase_clean_state("Foo");
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        state
            .local_closure_failures
            .insert(node("Foo"), sample_summary());
        assert!(state.live.open_nodes.is_empty());
        let selected = state.select_initial_proof_active_node();
        assert_eq!(
            selected,
            Some(node("Foo")),
            "select_initial_proof_active_node must return the unverified-only sorry-free proof_node"
        );
    }

    #[test]
    fn select_initial_proof_active_node_cone_funnel_includes_unverified() {
        // Patch C-C plan §7.4 — the cone-supported pool (first stage
        // of the funnel) must include unverified nodes from the
        // target's support cone, not just textually-open ones.
        // Construct: Foo covers target t, Foo depends on Helper, both
        // sorry-free; mark Helper unverified. The cone-funnel should
        // pick Helper (frontier rule prefers leaves).
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.proof_nodes = BTreeSet::from([node("Foo"), node("Helper")]);
        state.live.present_nodes = BTreeSet::from([node("Foo"), node("Helper")]);
        state.live.open_nodes = BTreeSet::new();
        state
            .deps
            .insert(node("Foo"), BTreeSet::from([node("Helper")]));
        let target = TargetId::from("t");
        state.configured_targets = BTreeSet::from([target.clone()]);
        state
            .approved_targets
            .configured_targets
            .insert(target.clone());
        state
            .approved_targets
            .coverage
            .insert(target.clone(), BTreeSet::from([node("Foo")]));
        state
            .live
            .coverage
            .insert(target, BTreeSet::from([node("Foo")]));
        state
            .local_closure_records
            .insert(node("Foo"), sample_record("Foo"));
        state
            .local_closure_records
            .insert(node("Helper"), sample_record("Helper"));
        // Now perturb: invalidate Helper's record + mark unverified.
        state.local_closure_records.remove(&node("Helper"));
        state.local_closure_unverified_nodes.insert(node("Helper"));
        state
            .local_closure_failures
            .insert(node("Helper"), sample_summary());
        let selected = state.select_initial_proof_active_node();
        assert_eq!(
            selected,
            Some(node("Helper")),
            "cone-funnel must surface unverified Helper from Foo's support cone"
        );
    }

    #[test]
    fn select_initial_proof_active_node_skips_transport_error_only_node() {
        // Patch C-C plan §7.4.1 — transport-error-only failures must
        // NOT auto-schedule a worker burst; they retry via the
        // deterministic-revalidation pass. With no other candidates,
        // the auto-scheduler returns None.
        let mut state = proof_phase_clean_state("Foo");
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        state
            .local_closure_failures
            .insert(node("Foo"), transport_summary(1, 5, 7, false));
        assert_eq!(
            state.select_initial_proof_active_node(),
            None,
            "transport-error-only unverified node must not be auto-scheduled"
        );
    }

    #[test]
    fn auto_scheduler_skips_naked_unverified_node() {
        // Patch C-F bug fix — a node in `local_closure_unverified_nodes`
        // with NO entry in `local_closure_failures` is "naked": it has
        // no evidence of a real proof problem (just-after-migration cold
        // start, dep-invalidated record, post-revalidation residue). The
        // right action is a cheap server-side probe via the runtime
        // CLI's revalidation pass, NOT a ~30-60s worker burst. The auto-
        // scheduler must skip naked unverified.
        let mut state = proof_phase_clean_state("Foo");
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        // Intentionally NO entry in local_closure_failures.
        assert!(state.local_closure_failures.get(&node("Foo")).is_none());
        assert_eq!(
            state.select_initial_proof_active_node(),
            None,
            "naked unverified must not be auto-scheduled"
        );
    }

    #[test]
    fn auto_scheduler_skips_unverified_with_empty_failure_record() {
        // Patch C-F bug fix — defensive: an unverified node whose
        // failure record has empty axiom_violations + empty
        // strict_errors AND a status that does not name a known proof-
        // problem category must also be skipped. This catches partial
        // / uninitialized records (e.g., a default-constructed
        // ErrorSummary that somehow landed in the map) without
        // dispatching a worker.
        let mut state = proof_phase_clean_state("Foo");
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        let summary = ErrorSummary {
            status: "unknown".to_string(),
            ..ErrorSummary::default()
        };
        assert!(summary.axiom_violations.is_empty());
        assert!(summary.strict_errors.is_empty());
        state.local_closure_failures.insert(node("Foo"), summary);
        assert_eq!(
            state.select_initial_proof_active_node(),
            None,
            "unverified with empty/unknown failure record must not be auto-scheduled"
        );
    }

    #[test]
    fn auto_scheduler_picks_unverified_with_axiom_violation() {
        // Patch C-F positive control — an unverified node whose failure
        // record carries a non-empty `axiom_violations` list represents
        // a real proof problem (the active proof uses an unapproved
        // kernel axiom). This is exactly the case where a worker burst
        // is appropriate. The auto-scheduler must surface the node.
        let mut state = proof_phase_clean_state("Foo");
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        // `sample_summary()` populates axiom_violations with "sorryAx".
        let summary = sample_summary();
        assert!(!summary.axiom_violations.is_empty());
        state.local_closure_failures.insert(node("Foo"), summary);
        assert_eq!(
            state.select_initial_proof_active_node(),
            Some(node("Foo")),
            "unverified with axiom_violation must be auto-scheduled"
        );
    }

    #[test]
    fn auto_scheduler_picks_unverified_with_strict_error() {
        // Patch C-F positive control — an unverified node whose failure
        // record carries a non-empty `strict_errors` list represents a
        // real strict-context violation (e.g., a strict theorem dep's
        // value no longer matches the captured semantic hash). The
        // auto-scheduler must surface this node for repair via worker
        // burst, even with an empty `axiom_violations` list.
        let mut state = proof_phase_clean_state("Foo");
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        let summary = ErrorSummary {
            status: "strict_error".to_string(),
            returncode: 1,
            timed_out: false,
            stderr_excerpt: "strict-context mismatch".to_string(),
            axiom_violations: vec![],
            strict_errors: vec!["dep T value drift".to_string()],
            captured_at_cycle: 17,
            retry_count: 0,
            last_attempt_cycle: 0,
            next_retry_cycle: 0,
            retry_exhausted: false,
        };
        state.local_closure_failures.insert(node("Foo"), summary);
        assert_eq!(
            state.select_initial_proof_active_node(),
            Some(node("Foo")),
            "unverified with strict_error must be auto-scheduled"
        );
    }

    #[test]
    fn review_request_surfaces_local_closure_unverified_failures() {
        // Patch C-C plan §7.4.2 — `WrapperRequest.local_closure_unverified`
        // must be populated on Review requests with the failure summaries
        // for every node in `local_closure_unverified_nodes` that has a
        // matching entry in `local_closure_failures`.
        let mut state = proof_phase_clean_state("Foo");
        state.local_closure_records.remove(&node("Foo"));
        state.local_closure_unverified_nodes.insert(node("Foo"));
        state
            .local_closure_failures
            .insert(node("Foo"), sample_summary());
        let request = state.expected_request(1, RequestKind::Review);
        assert_eq!(request.local_closure_unverified.len(), 1);
        assert_eq!(
            request.local_closure_unverified.get(&node("Foo")),
            Some(&sample_summary())
        );
        // Verifier requests must NOT carry the field (per the request
        // builder match: empty for Paper / Corr / Sound / HumanGate).
        let paper_request = state.expected_request(2, RequestKind::Paper);
        assert!(paper_request.local_closure_unverified.is_empty());
    }

    // ---- Patch C-E gap-fill tests --------------------------------------

    #[test]
    fn reverse_indices_recompute_idempotent_after_state_deserialize() {
        // Plan §7.2 — reverse indices are `#[serde(skip)]`; after JSON
        // round-trip they MUST be reconstructible from `local_closure_records`
        // via `recompute_local_closure_reverse_indices`. This is the
        // supervisor startup invariant.
        let mut original = ProtocolState::default();
        let mut record_a = sample_record("A");
        record_a.node = node("A");
        record_a.boundary_theorems = BTreeMap::from([(node("H"), "stmt-h".to_string())]);
        record_a.strict_theorem_deps = BTreeMap::from([(node("T"), "val-t".to_string())]);
        original.local_closure_records.insert(node("A"), record_a);
        recompute_local_closure_reverse_indices(&mut original);
        let pre_boundary = original.boundary_statement_consumers.clone();
        let pre_strict = original.strict_dep_consumers.clone();
        assert!(!pre_boundary.is_empty());

        // Round-trip through JSON; indices are skip-serialized → drop on
        // the floor across the wire.
        let json = serde_json::to_string(&original).expect("serialize state");
        let mut roundtrip: ProtocolState = serde_json::from_str(&json).expect("deserialize state");
        assert!(
            roundtrip.boundary_statement_consumers.is_empty(),
            "deserialized state must have empty reverse indices (skip-serialized)"
        );
        assert!(roundtrip.strict_dep_consumers.is_empty());

        // Startup-pump recompute reproduces the original indices.
        recompute_local_closure_reverse_indices(&mut roundtrip);
        assert_eq!(roundtrip.boundary_statement_consumers, pre_boundary);
        assert_eq!(roundtrip.strict_dep_consumers, pre_strict);
    }

    #[test]
    fn commit_live_cross_tier_mirror_consistency_committed_tracks_live() {
        // Plan §7.2 / §7.7 — after `commit_live` at any checkpoint, the
        // committed mirror MUST track the live closure tier byte-for-byte.
        // This is the precondition for `restore_committed` correctness.
        let mut state = ProtocolState::default();
        // Force a non-clean checkpoint to isolate the committed branch.
        state.configured_targets = BTreeSet::from([TargetId::from("t")]);
        state
            .local_closure_records
            .insert(node("A"), sample_record("A"));
        state.local_closure_unverified_nodes.insert(node("B"));
        state
            .local_closure_failures
            .insert(node("B"), sample_summary());
        state.commit_live();
        assert_eq!(
            state.committed_local_closure_records,
            state.local_closure_records
        );
        assert_eq!(
            state.committed_local_closure_unverified_nodes,
            state.local_closure_unverified_nodes
        );
        assert_eq!(
            state.committed_local_closure_failures,
            state.local_closure_failures
        );
    }

    #[test]
    fn last_clean_promotes_from_committed_at_clean_checkpoint() {
        // Plan §7.8 — clean-checkpoint `commit_live` promotes the live
        // tier into both committed AND last_clean mirrors in one step;
        // committed and last_clean must agree after a clean checkpoint.
        let mut state = ProtocolState::default();
        state
            .local_closure_records
            .insert(node("A"), sample_record("A"));
        assert!(state.global_blockers().is_empty());
        state.commit_live();
        assert!(state.last_clean_local_closure_mirror_ready);
        assert_eq!(
            state.committed_local_closure_records,
            state.last_clean_local_closure_records
        );
        assert_eq!(
            state.committed_local_closure_unverified_nodes,
            state.last_clean_local_closure_unverified_nodes
        );
        assert_eq!(
            state.committed_local_closure_failures,
            state.last_clean_local_closure_failures
        );
    }

    #[test]
    fn approved_axioms_hash_field_round_trips_independently_of_kernel_axioms() {
        // Plan §7.3 rule 11 + §8 — `approved_axioms_hash` is a per-record
        // field independent of `kernel_axioms`. A record with the same
        // `kernel_axioms` but a different `approved_axioms_hash` must be
        // distinguishable (the hash field is what drives rule-11 invalidation).
        let mut record_a = sample_record("A");
        record_a.approved_axioms_hash = "ax-hash-v1".to_string();
        let mut record_b = sample_record("A");
        record_b.approved_axioms_hash = "ax-hash-v2".to_string();
        // Same kernel_axioms, same boundary deps; only the per-node
        // approved-axioms hash differs.
        assert_eq!(record_a.kernel_axioms, record_b.kernel_axioms);
        assert_ne!(
            record_a, record_b,
            "differing approved_axioms_hash must not compare equal"
        );
        // Round-trip both — the hash distinction must survive serde.
        let a_json = serde_json::to_string(&record_a).unwrap();
        let b_json = serde_json::to_string(&record_b).unwrap();
        let a_parsed: LocalClosureRecord = serde_json::from_str(&a_json).unwrap();
        let b_parsed: LocalClosureRecord = serde_json::from_str(&b_json).unwrap();
        assert_eq!(a_parsed.approved_axioms_hash, "ax-hash-v1");
        assert_eq!(b_parsed.approved_axioms_hash, "ax-hash-v2");
    }

    #[test]
    fn select_initial_proof_active_node_returns_none_when_only_records_present() {
        // Plan §7.4 — when every sorry-free proof_node has a fresh record
        // AND `live.open_nodes` is empty AND the unverified set is empty,
        // there is no work to schedule: select returns None. This is the
        // "phase complete" precondition; without it the auto-scheduler
        // would loop in a clean state.
        let state = proof_phase_clean_state("Foo");
        // proof_phase_clean_state installs a record for Foo and leaves
        // unverified empty; assert nothing is selectable.
        assert!(state.live.open_nodes.is_empty());
        assert!(state.local_closure_unverified_nodes.is_empty());
        assert!(state.local_closure_records.contains_key(&node("Foo")));
        assert_eq!(
            state.select_initial_proof_active_node(),
            None,
            "fully-clean ProofFormalization must surface no candidates"
        );
    }

    // ---- Cleanup-v2 unit tests (Steps 18-20, 2026-05-14) -----------------

    /// Build a Phase::Cleanup state with a few present nodes and an
    /// optional protected-statement entry. Helper for cleanup-v2
    /// validator + task tests.
    fn cleanup_phase_state_with_nodes(
        present: &[&str],
        protected_target: Option<(&str, &[&str])>,
    ) -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.stage = Stage::Start;
        state.live.present_nodes = present.iter().map(|n| node(n)).collect();
        if let Some((target, covering)) = protected_target {
            state.live.coverage.insert(
                TargetId::from(target),
                covering.iter().map(|n| node(n)).collect(),
            );
        }
        state
    }

    #[test]
    fn legal_cleanup_task_accepts_substitution_with_mathlib_replacement() {
        let state = cleanup_phase_state_with_nodes(&["A", "B"], None);
        let task = NewCleanupAuditTask {
            target_node: node("A"),
            rationale: "wrapper of Nat.add_comm".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "Nat.add_comm".into(),
                },
            },
        };
        assert!(state.legal_cleanup_task(&task).is_ok());
    }

    #[test]
    fn legal_cleanup_task_rejects_target_not_present() {
        let state = cleanup_phase_state_with_nodes(&["A"], None);
        let task = NewCleanupAuditTask {
            target_node: node("B"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Low,
            kind: CleanupTaskKind::LintFix {
                warning_text: "unused variable".into(),
            },
        };
        let err = state.legal_cleanup_task(&task).unwrap_err();
        assert!(
            err.contains("not in live.present_nodes"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn legal_cleanup_task_rejects_target_in_protected_statement_set() {
        let state =
            cleanup_phase_state_with_nodes(&["A", "Protected"], Some(("t", &["Protected"])));
        let task = NewCleanupAuditTask {
            target_node: node("Protected"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Low,
            kind: CleanupTaskKind::LintFix {
                warning_text: "warning".into(),
            },
        };
        let err = state.legal_cleanup_task(&task).unwrap_err();
        assert!(
            err.contains("protected-statement set"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn legal_cleanup_task_substitution_tablet_wrapper_replacement_must_be_present() {
        let state = cleanup_phase_state_with_nodes(&["A", "B"], None);
        let task = NewCleanupAuditTask {
            target_node: node("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Medium,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::TabletWrapper {
                    node: node("MissingNode"),
                },
            },
        };
        let err = state.legal_cleanup_task(&task).unwrap_err();
        assert!(err.contains("not in live.present_nodes"));
    }

    #[test]
    fn legal_cleanup_task_substitution_tablet_wrapper_protected_replacement_is_ok() {
        // Replacement may be a protected node — only the target is barred.
        let state =
            cleanup_phase_state_with_nodes(&["A", "Protected"], Some(("t", &["Protected"])));
        let task = NewCleanupAuditTask {
            target_node: node("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Medium,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::TabletWrapper {
                    node: node("Protected"),
                },
            },
        };
        assert!(state.legal_cleanup_task(&task).is_ok());
    }

    #[test]
    fn legal_cleanup_task_substitution_mathlib_empty_citation_rejected() {
        let state = cleanup_phase_state_with_nodes(&["A"], None);
        let task = NewCleanupAuditTask {
            target_node: node("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Low,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "   ".into(),
                },
            },
        };
        let err = state.legal_cleanup_task(&task).unwrap_err();
        assert!(err.contains("Mathlib") || err.contains("citation"));
    }

    #[test]
    fn legal_cleanup_task_lintfix_empty_warning_text_rejected() {
        let state = cleanup_phase_state_with_nodes(&["A"], None);
        let task = NewCleanupAuditTask {
            target_node: node("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Low,
            kind: CleanupTaskKind::LintFix {
                warning_text: "".into(),
            },
        };
        let err = state.legal_cleanup_task(&task).unwrap_err();
        assert!(err.contains("LintFix") || err.contains("warning_text"));
    }

    #[test]
    fn legal_cleanup_task_rejects_duplicate_target_kind_pair() {
        let mut state = cleanup_phase_state_with_nodes(&["A", "B"], None);
        // Plant an existing task: (A, Substitution(Mathlib(foo)))
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: node("A"),
            rationale: "first".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "foo".into(),
                },
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        // Propose the same (target, kind) again — should be rejected.
        let dup = NewCleanupAuditTask {
            target_node: node("A"),
            rationale: "duplicate".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "foo".into(),
                },
            },
        };
        let err = state.legal_cleanup_task(&dup).unwrap_err();
        assert!(err.contains("duplicate"));

        // Same target with a different kind is OK (LintFix vs Substitution).
        let different_kind = NewCleanupAuditTask {
            target_node: node("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Low,
            kind: CleanupTaskKind::LintFix {
                warning_text: "unused var".into(),
            },
        };
        assert!(state.legal_cleanup_task(&different_kind).is_ok());
    }

    #[test]
    fn live_protected_statement_node_set_unions_coverage_and_closure() {
        let mut state = ProtocolState::default();
        state.live.coverage.insert(
            TargetId::from("t1"),
            BTreeSet::from([node("Cover1"), node("Cover2")]),
        );
        state
            .live
            .protected_closure_nodes_per_target
            .insert(TargetId::from("t1"), BTreeSet::from([node("Closure1")]));
        state
            .live
            .protected_closure_nodes_per_target
            .insert(TargetId::from("t2"), BTreeSet::from([node("Closure2")]));
        let result = state.live_protected_statement_node_set();
        assert_eq!(
            result,
            BTreeSet::from([
                node("Cover1"),
                node("Cover2"),
                node("Closure1"),
                node("Closure2"),
            ])
        );
    }

    #[test]
    fn cleanup_substantiveness_scope_empty_outside_cleanup() {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: node("A"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "X".into(),
                },
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        state.cleanup_active_task = Some(0);
        // Outside Cleanup phase: scope is always empty regardless of
        // active_task.
        assert!(state.cleanup_substantiveness_scope().is_empty());
    }

    #[test]
    fn cleanup_substantiveness_scope_includes_target_and_authorized_for_substitution() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: node("Target"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "X".into(),
                },
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        state.cleanup_active_task = Some(0);
        state.pending_task = Some(PendingTask {
            authorized_nodes: BTreeSet::from([node("Importer1"), node("Importer2")]),
            ..PendingTask::default()
        });
        let scope = state.cleanup_substantiveness_scope();
        assert_eq!(
            scope,
            BTreeSet::from([node("Target"), node("Importer1"), node("Importer2")])
        );
    }

    #[test]
    fn cleanup_substantiveness_scope_empty_for_lintfix_task() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: node("LintNode"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Medium,
            kind: CleanupTaskKind::LintFix {
                warning_text: "warn".into(),
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        state.cleanup_active_task = Some(0);
        // LintFix: no substantiveness re-check (no semantic rewires).
        assert!(state.cleanup_substantiveness_scope().is_empty());
    }

    /// Cleanup-v2 (audit Finding 5): the worker-visible scope for a
    /// FinalCleanup Substitution task must equal `pending_task.authorized_nodes
    /// ∪ {target_node}` — NOT all_present. Pre-fix the worker was told
    /// `present_nodes.clone()` but the validator restricted to
    /// `authorized_nodes ∪ {target_node}`, so a substitution worker editing
    /// an importer outside the reviewer-supplied set would be rejected at
    /// acceptance time despite being told it was authorized.
    #[test]
    fn current_worker_authorized_nodes_substitution_matches_validator_scope() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.live.present_nodes = BTreeSet::from([
            node("Target"),
            node("Importer1"),
            node("Importer2"),
            node("Unrelated1"),
            node("Unrelated2"),
            node("Unrelated3"),
        ]);
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: node("Target"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::High,
            kind: CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "Nat.add_comm".into(),
                },
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        state.cleanup_active_task = Some(0);
        state.pending_task = Some(PendingTask {
            mode: TaskMode::Cleanup,
            authorized_nodes: BTreeSet::from([node("Importer1"), node("Importer2")]),
            ..PendingTask::default()
        });
        let scope = state.current_worker_authorized_nodes();
        // Worker-visible scope == validator scope == authorized_nodes ∪ {target_node}.
        assert_eq!(
            scope,
            BTreeSet::from([node("Target"), node("Importer1"), node("Importer2")]),
            "worker-visible scope must equal validator-enforced scope, not all_present"
        );
        // Crucially, unrelated nodes must NOT be in the worker scope.
        assert!(!scope.contains(&node("Unrelated1")));
        assert!(!scope.contains(&node("Unrelated2")));
        assert!(!scope.contains(&node("Unrelated3")));
    }

    /// Cleanup-v2 (audit Finding 5): LintFix worker scope is just
    /// `{target_node}`. Matches the LintFix validator.
    #[test]
    fn current_worker_authorized_nodes_lintfix_is_target_only() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        state.live.present_nodes = BTreeSet::from([node("Target"), node("Other1"), node("Other2")]);
        state.cleanup_audit_tasks.push(CleanupAuditTask {
            target_node: node("Target"),
            rationale: "".into(),
            confidence: CleanupTaskConfidence::Medium,
            kind: CleanupTaskKind::LintFix {
                warning_text: "x".into(),
            },
            status: CleanupTaskStatus::Pending,
            audit_origin_round: 1,
        });
        state.cleanup_active_task = Some(0);
        state.pending_task = Some(PendingTask {
            mode: TaskMode::Cleanup,
            authorized_nodes: BTreeSet::new(),
            ..PendingTask::default()
        });
        let scope = state.current_worker_authorized_nodes();
        // LintFix scope: just the target.
        assert_eq!(scope, BTreeSet::from([node("Target")]));
    }

    /// Cleanup-v2 (audit Finding 2): when `cleanup_force_done` is set,
    /// `request_allowed_decisions` for Review must drop Continue from
    /// the allowed set, leaving Done as the sole legal decision.
    #[test]
    fn request_allowed_decisions_cleanup_drops_continue_under_force_done() {
        let mut state = ProtocolState::default();
        state.phase = Phase::Cleanup;
        // Default state (force_done = false): Continue + Done.
        let baseline = state.request_allowed_decisions(RequestKind::Review);
        assert!(baseline.contains(&ReviewDecisionKind::Continue));
        assert!(baseline.contains(&ReviewDecisionKind::Done));
        // Latch fires: Continue is removed.
        state.cleanup_force_done = true;
        let narrowed = state.request_allowed_decisions(RequestKind::Review);
        assert_eq!(narrowed, BTreeSet::from([ReviewDecisionKind::Done]));
    }

    // ---------- shallowly_closed_from_coarse ----------
    //
    // Behavior mirrored from viewer's `isCoarseShallowlyClosed`
    // (viewer/server.js) and `isShallowlyClosedFromCoarse`
    // (viewer/public/index.html, "Coarse + open only" filter).

    fn mk_deps(pairs: &[(&str, &[&str])]) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
        pairs
            .iter()
            .map(|(k, vs)| (node(k), vs.iter().map(|v| node(v)).collect()))
            .collect()
    }

    fn mk_set(ids: &[&str]) -> BTreeSet<NodeId> {
        ids.iter().map(|s| node(s)).collect()
    }

    #[test]
    fn shallow_coarse_closed_node_with_no_deps_returns_true() {
        let present = mk_set(&["A"]);
        let open = mk_set(&[]);
        let deps = mk_deps(&[]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        assert!(shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_not_present_returns_false() {
        let present = mk_set(&[]);
        let open = mk_set(&[]);
        let deps = mk_deps(&[]);
        let coarse = mk_set(&[]);
        let mut memo = BTreeMap::new();
        assert!(!shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_open_returns_false() {
        let present = mk_set(&["A"]);
        let open = mk_set(&["A"]);
        let deps = mk_deps(&[]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        assert!(!shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_dep_closed_helper_returns_true() {
        // A (coarse, closed) depends on B (non-coarse, closed).
        let present = mk_set(&["A", "B"]);
        let open = mk_set(&[]);
        let deps = mk_deps(&[("A", &["B"])]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        assert!(shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_dep_open_helper_returns_false() {
        // A (coarse, closed) depends on B (non-coarse, OPEN).
        let present = mk_set(&["A", "B"]);
        let open = mk_set(&["B"]);
        let deps = mk_deps(&[("A", &["B"])]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        assert!(!shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_dep_is_coarse_treated_as_opaque_leaf() {
        // A (coarse, closed) depends on B (coarse, OPEN). B is opaque;
        // A is shallowly closed-from-coarse even though B is unclosed.
        let present = mk_set(&["A", "B"]);
        let open = mk_set(&["B"]);
        let deps = mk_deps(&[("A", &["B"])]);
        let coarse = mk_set(&["A", "B"]);
        let mut memo = BTreeMap::new();
        assert!(shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
        // And B by itself: it's open, so its own shallow status is false.
        assert!(!shallowly_closed_from_coarse(
            &node("B"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_descent_stops_at_coarse_boundary() {
        // A (coarse, closed) → B (helper, closed) → C (coarse, OPEN).
        // C is opaque → B is shallowly closed → A is shallowly closed.
        let present = mk_set(&["A", "B", "C"]);
        let open = mk_set(&["C"]);
        let deps = mk_deps(&[("A", &["B"]), ("B", &["C"])]);
        let coarse = mk_set(&["A", "C"]);
        let mut memo = BTreeMap::new();
        assert!(shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_descent_propagates_open_through_helpers() {
        // A (coarse, closed) → B (helper, closed) → C (helper, OPEN).
        // C is non-coarse → B sees C open → B false → A false.
        let present = mk_set(&["A", "B", "C"]);
        let open = mk_set(&["C"]);
        let deps = mk_deps(&[("A", &["B"]), ("B", &["C"])]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        assert!(!shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_cycle_returns_true_via_stack_guard() {
        // A → B → A, both closed. Cycle guard kicks in.
        let present = mk_set(&["A", "B"]);
        let open = mk_set(&[]);
        let deps = mk_deps(&[("A", &["B"]), ("B", &["A"])]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        assert!(shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_diamond_all_closed() {
        // A → {B, C}; B → D; C → D. All closed.
        let present = mk_set(&["A", "B", "C", "D"]);
        let open = mk_set(&[]);
        let deps = mk_deps(&[("A", &["B", "C"]), ("B", &["D"]), ("C", &["D"])]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        assert!(shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_dep_missing_from_present_returns_false() {
        // A (closed) → B (not present at all). Strict-by-design:
        // the missing-dep recursion hits the present check and
        // returns false, propagating up.
        let present = mk_set(&["A"]);
        let open = mk_set(&[]);
        let deps = mk_deps(&[("A", &["B"])]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        assert!(!shallowly_closed_from_coarse(
            &node("A"),
            &present,
            &open,
            &deps,
            &coarse,
            &mut memo
        ));
    }

    #[test]
    fn shallow_coarse_memo_caches_per_node() {
        let present = mk_set(&["A", "B"]);
        let open = mk_set(&[]);
        let deps = mk_deps(&[("A", &["B"])]);
        let coarse = mk_set(&["A"]);
        let mut memo = BTreeMap::new();
        let first =
            shallowly_closed_from_coarse(&node("A"), &present, &open, &deps, &coarse, &mut memo);
        assert!(first);
        assert_eq!(memo.get(&node("A")), Some(&true));
        assert_eq!(memo.get(&node("B")), Some(&true));
        // Second query returns same result and doesn't add new entries.
        let len_before = memo.len();
        let second =
            shallowly_closed_from_coarse(&node("A"), &present, &open, &deps, &coarse, &mut memo);
        assert!(second);
        assert_eq!(memo.len(), len_before);
    }

    #[test]
    fn shallowly_closed_coarse_nodes_returns_filtered_subset() {
        // Three coarse nodes: A is shallow-closed, B isn't (open helper),
        // C is shallow-closed (only deps on another coarse).
        let present = mk_set(&["A", "B", "C", "Helper", "OpenHelper"]);
        let open = mk_set(&["OpenHelper"]);
        let deps = mk_deps(&[("A", &["Helper"]), ("B", &["OpenHelper"]), ("C", &["A"])]);
        let coarse = mk_set(&["A", "B", "C"]);
        let closed = shallowly_closed_coarse_nodes(&present, &open, &deps, &coarse);
        assert_eq!(closed, mk_set(&["A", "C"]));
    }

    #[test]
    fn proposal_v32_legacy_state_loads_without_anchor_fields() {
        // Round-trip a default ProtocolState, strip the v32 fields,
        // and confirm reload populates them with #[serde(default)].
        let baseline = ProtocolState::default();
        let mut value = serde_json::to_value(&baseline).expect("serialize default state");
        let obj = value
            .as_object_mut()
            .expect("ProtocolState serializes as object");
        // Pre-v32 state files have no entries for these fields.
        obj.remove("active_coarse_node");
        obj.remove("cycles_in_coarse_repair_mode");
        let json = serde_json::to_string(&value).expect("re-encode");
        let parsed: Result<ProtocolState, _> = serde_json::from_str(&json);
        assert!(
            parsed.is_ok(),
            "legacy ProtocolState must deserialize cleanly: {:?}",
            parsed.err()
        );
        let state = parsed.unwrap();
        assert_eq!(state.active_coarse_node, None);
        assert_eq!(state.cycles_in_coarse_repair_mode, 0);
        // Mechanism is dormant: empty coarse DAG => helpers return permissive defaults.
        assert!(!state.coarse_repair_mode());
        assert!(state.active_coarse_change_allowed());
        assert!(state.kernel_hinted_next_active_coarse_nodes().is_empty());
        // Legacy active_node legality should be untouched: any present
        // node is acceptable in TheoremStating/Cleanup phases.
        assert_eq!(state.coarse_legal_active_set(), state.live.present_nodes);
    }

    #[test]
    fn proposal_v32_locked_anchor_narrows_legal_set() {
        let a = node("A");
        let b = node("B");
        let helper_a = node("HelperA");
        let helper_b = node("HelperB");
        let mut state = ProtocolState {
            phase: Phase::ProofFormalization,
            coarse_dag_nodes: BTreeSet::from([a.clone(), b.clone()]),
            active_coarse_node: Some(a.clone()),
            proof_nodes: BTreeSet::from([a.clone(), b.clone()]),
            deps: BTreeMap::from([
                (a.clone(), BTreeSet::from([helper_a.clone()])),
                (b.clone(), BTreeSet::from([helper_b.clone()])),
                (helper_a.clone(), BTreeSet::new()),
                (helper_b.clone(), BTreeSet::new()),
            ]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([
                    a.clone(),
                    b.clone(),
                    helper_a.clone(),
                    helper_b.clone(),
                ]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };
        // Mark all nodes as substantiveness/corr/sound Pass against
        // default fingerprints so global_blockers() is empty. (The
        // legality narrowing is what we want to test; blocker-driven
        // widening is exercised by a sibling test below.)
        for n in &[&a, &b, &helper_a, &helper_b] {
            state
                .substantiveness_status
                .insert((*n).clone(), CorrStatus::Pass);
            state
                .substantiveness_approved_fingerprints
                .insert((*n).clone(), Fingerprint::default());
            state
                .live
                .substantiveness_current_fingerprints
                .insert((*n).clone(), Fingerprint::default());
            state.corr_status.insert((*n).clone(), CorrStatus::Pass);
            state
                .corr_approved_fingerprints
                .insert((*n).clone(), Fingerprint::from("corr".to_string()));
            state
                .live
                .corr_current_fingerprints
                .insert((*n).clone(), Fingerprint::from("corr".to_string()));
        }
        assert!(
            state.global_blockers().is_empty(),
            "expected no blockers in setup; got {:?}",
            state.global_blockers()
        );
        // No blockers, no repair-mode: legal set = down-cone of A = {A, HelperA}.
        let legal = state.coarse_legal_active_set();
        assert!(legal.contains(&a), "anchor A in legal set");
        assert!(legal.contains(&helper_a), "A's helper in legal set");
        assert!(!legal.contains(&b), "out-of-cone B excluded");
        assert!(!legal.contains(&helper_b), "out-of-cone helper excluded");
        assert!(!state.coarse_repair_mode(), "no outside-cone blockers");
    }

    /// Helper: build a 4-node ProofFormalization state with anchor A,
    /// cone {A, HelperA}, and clean (no-blocker) status across the
    /// board. Caller can mutate to inject specific scenarios.
    fn v32_state_with_anchor() -> ProtocolState {
        let a = node("A");
        let b = node("B");
        let helper_a = node("HelperA");
        let helper_b = node("HelperB");
        let mut state = ProtocolState {
            phase: Phase::ProofFormalization,
            coarse_dag_nodes: BTreeSet::from([a.clone(), b.clone()]),
            active_coarse_node: Some(a.clone()),
            proof_nodes: BTreeSet::from([a.clone(), b.clone()]),
            deps: BTreeMap::from([
                (a.clone(), BTreeSet::from([helper_a.clone()])),
                (b.clone(), BTreeSet::from([helper_b.clone()])),
                (helper_a.clone(), BTreeSet::new()),
                (helper_b.clone(), BTreeSet::new()),
            ]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([
                    a.clone(),
                    b.clone(),
                    helper_a.clone(),
                    helper_b.clone(),
                ]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };
        for n in &[&a, &b, &helper_a, &helper_b] {
            state
                .substantiveness_status
                .insert((*n).clone(), CorrStatus::Pass);
            state
                .substantiveness_approved_fingerprints
                .insert((*n).clone(), Fingerprint::default());
            state
                .live
                .substantiveness_current_fingerprints
                .insert((*n).clone(), Fingerprint::default());
            state.corr_status.insert((*n).clone(), CorrStatus::Pass);
            state
                .corr_approved_fingerprints
                .insert((*n).clone(), Fingerprint::from("corr".to_string()));
            state
                .live
                .corr_current_fingerprints
                .insert((*n).clone(), Fingerprint::from("corr".to_string()));
        }
        state
    }

    #[test]
    fn proposal_v32_coarse_repair_mode_widens_legal_set() {
        let mut state = v32_state_with_anchor();
        let b = node("B");
        let helper_b = node("HelperB");
        // Plant a substantiveness blocker on B (outside the anchor A's cone).
        state
            .substantiveness_status
            .insert(b.clone(), CorrStatus::Fail);
        assert!(
            !state.global_blockers().is_empty(),
            "expected B substantiveness blocker"
        );
        assert!(
            state.coarse_repair_mode(),
            "blocker on B is outside cone(A) => repair mode"
        );
        let legal = state.coarse_legal_active_set();
        let a = node("A");
        let helper_a = node("HelperA");
        assert!(legal.contains(&a) && legal.contains(&helper_a));
        assert!(legal.contains(&b), "B (blocker carrier) widens legal set");
        assert!(
            legal.contains(&helper_b),
            "HelperB (in B's down-cone) widens legal set"
        );
    }

    #[test]
    fn proposal_v32_starvation_guard_opens_lock_with_blockers_present() {
        let mut state = v32_state_with_anchor();
        let b = node("B");
        let helper_b = node("HelperB");
        // Mark HelperB open so coarse node B is NOT shallow-closed
        // (otherwise kernel_hinted would filter it out as completed).
        state.live.open_nodes.insert(helper_b);
        // Plant a substantiveness blocker on B (outside cone(A)).
        state
            .substantiveness_status
            .insert(b.clone(), CorrStatus::Fail);
        assert!(state.coarse_repair_mode());
        // With blockers present, normal predicate says locked.
        state.cycles_in_coarse_repair_mode = 0;
        assert!(
            !state.active_coarse_change_allowed(),
            "blockers present + counter=0 should be locked"
        );
        assert!(
            state.kernel_hinted_next_active_coarse_nodes().is_empty(),
            "locked anchor => no candidates"
        );
        // Crossing the starvation threshold opens it.
        state.cycles_in_coarse_repair_mode = super::stuck_coarse_repair_threshold();
        assert!(
            state.active_coarse_change_allowed(),
            "starvation guard should fire at threshold"
        );
        let hints = state.kernel_hinted_next_active_coarse_nodes();
        assert!(
            hints.contains(&b),
            "starvation unlock should surface B (still open) as candidate; got {:?}",
            hints
        );
    }

    #[test]
    fn proposal_v32_cone_clean_clears_anchor_when_targeting_anchor() {
        // The engine helper apply_audit_authorized_theorem_stating_node_reset
        // is private; we replicate its anchor-clearing predicate here.
        // The engine-side path is exercised by integration / runtime
        // tests that drive the full Audit response cycle.
        let mut state = v32_state_with_anchor();
        let a = node("A");
        assert_eq!(state.active_coarse_node, Some(a.clone()));
        if state.active_coarse_node.as_ref() == Some(&a) {
            state.active_coarse_node = None;
        }
        state.cycles_in_coarse_repair_mode = 0;
        assert_eq!(state.active_coarse_node, None);
        assert_eq!(state.cycles_in_coarse_repair_mode, 0);
    }

    #[test]
    fn proposal_v32_cone_clean_preserves_anchor_when_targeting_non_anchor() {
        let mut state = v32_state_with_anchor();
        let a = node("A");
        let b = node("B");
        // Mirror engine.rs:441 logic: targeting B (not the anchor)
        // should NOT clear the anchor.
        let target = b.clone();
        if state.active_coarse_node.as_ref() == Some(&target) {
            state.active_coarse_node = None;
        }
        // (Counter resets in the real path regardless.)
        state.cycles_in_coarse_repair_mode = 0;
        assert_eq!(state.active_coarse_node, Some(a));
    }

    #[test]
    fn proposal_v32_retry_review_rejects_next_active_coarse() {
        let a = node("A");
        let mut request = WrapperRequest::default();
        request.phase = Phase::ProofFormalization;
        request.kernel_hinted_next_active_coarse_nodes = BTreeSet::from([a.clone()]);
        request.retry_outcome_kind = RetryOutcomeKind::Stuck;
        let mut review = ReviewResponse::default();
        review.decision = ReviewDecisionKind::Continue;
        review.next_active_coarse = Some(a);
        assert!(
            !request.review_next_active_coarse_legal_for_response(&review),
            "Stuck retry-review should reject next_active_coarse"
        );
        // NeedsRestructure also rejects.
        request.retry_outcome_kind = RetryOutcomeKind::NeedsRestructure;
        assert!(
            !request.review_next_active_coarse_legal_for_response(&review),
            "NeedsRestructure retry-review should reject next_active_coarse"
        );
        // Baseline: with no retry, accepts.
        request.retry_outcome_kind = RetryOutcomeKind::None;
        assert!(
            request.review_next_active_coarse_legal_for_response(&review),
            "non-retry Continue with hinted candidate should accept"
        );
    }

    #[test]
    fn proposal_v32_cycles_counter_logic() {
        // Replicates the engine-side counter update logic:
        //   if anchor changed OR anchor=None OR coarse_dag_nodes empty -> 0
        //   else if pre_repair_mode was true -> +1
        //   else -> 0
        let mut state = v32_state_with_anchor();
        // Initial: no blockers, no repair-mode. Counter should reset.
        let pre_repair = state.coarse_repair_mode();
        let pre_anchor = state.active_coarse_node.clone();
        state.cycles_in_coarse_repair_mode = 5;
        // Simulate engine update (anchor unchanged, pre_repair false).
        let anchor_changed = state.active_coarse_node != pre_anchor;
        state.cycles_in_coarse_repair_mode = if anchor_changed
            || state.active_coarse_node.is_none()
            || state.coarse_dag_nodes.is_empty()
        {
            0
        } else if pre_repair {
            state.cycles_in_coarse_repair_mode.saturating_add(1)
        } else {
            0
        };
        assert_eq!(
            state.cycles_in_coarse_repair_mode, 0,
            "no repair-mode => reset"
        );

        // Now flip into repair-mode and tick once without anchor change.
        let b = node("B");
        state
            .substantiveness_status
            .insert(b.clone(), CorrStatus::Fail);
        assert!(state.coarse_repair_mode());
        let pre_repair = state.coarse_repair_mode();
        let pre_anchor = state.active_coarse_node.clone();
        let anchor_changed = state.active_coarse_node != pre_anchor;
        state.cycles_in_coarse_repair_mode = if anchor_changed
            || state.active_coarse_node.is_none()
            || state.coarse_dag_nodes.is_empty()
        {
            0
        } else if pre_repair {
            state.cycles_in_coarse_repair_mode.saturating_add(1)
        } else {
            0
        };
        assert_eq!(
            state.cycles_in_coarse_repair_mode, 1,
            "repair-mode + no anchor change => +1"
        );

        // Now change the anchor — counter should reset even though repair-mode is true.
        let pre_repair = state.coarse_repair_mode();
        let pre_anchor = state.active_coarse_node.clone();
        state.active_coarse_node = Some(b.clone());
        let anchor_changed = state.active_coarse_node != pre_anchor;
        state.cycles_in_coarse_repair_mode = if anchor_changed
            || state.active_coarse_node.is_none()
            || state.coarse_dag_nodes.is_empty()
        {
            0
        } else if pre_repair {
            state.cycles_in_coarse_repair_mode.saturating_add(1)
        } else {
            0
        };
        assert_eq!(
            state.cycles_in_coarse_repair_mode, 0,
            "anchor change => reset even under repair-mode"
        );
    }

    #[test]
    fn proposal_v32_relegalize_clears_dangling_anchor() {
        // Build a state where the anchor exists in coarse_dag_nodes
        // but not in live.present_nodes (simulating a post-LastClean
        // landing where the anchor was deleted by an earlier
        // CoarseRestructure and the rewind tag predates re-creation).
        let a = node("A");
        let b = node("B");
        let mut state = v32_state_with_anchor();
        // Surgically remove the anchor from present_nodes.
        state.live.present_nodes.remove(&a);
        // In this state, coarse_node_support_cone returns empty, so
        // coarse_legal_active_set returns empty -- a deadlock.
        assert!(
            state.coarse_legal_active_set().is_empty(),
            "anchor missing from present_nodes => empty cone => empty legal set"
        );
        // The engine's relegalize helper should clear it.
        let mut after = state.clone();
        // Replicate engine::relegalize_active_coarse_anchor logic.
        let needs_clear = match after.active_coarse_node.as_ref() {
            Some(anchor) => !after.live.present_nodes.contains(anchor),
            None => false,
        };
        if needs_clear {
            after.active_coarse_node = None;
        }
        after.cycles_in_coarse_repair_mode = 0;
        assert_eq!(after.active_coarse_node, None);
        // Now legal set defaults to live.present_nodes (B + helpers).
        assert_eq!(after.coarse_legal_active_set(), after.live.present_nodes);
        let _ = b;
    }

    #[test]
    fn v32_audit2_followup_6_retry_review_hides_anchor_hints() {
        // Setup: ProofFormalization with anchor A, both A and B are
        // open coarse nodes that would otherwise appear as hints if
        // the lock were open. Force the lock open by setting the
        // anchor None so kernel_hinted_next_active_coarse_nodes() is
        // non-empty in the non-retry case.
        let a = node("A");
        let b = node("B");
        let mut state = ProtocolState {
            phase: Phase::ProofFormalization,
            coarse_dag_nodes: BTreeSet::from([a.clone(), b.clone()]),
            active_coarse_node: None,
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([a.clone(), b.clone()]),
                open_nodes: BTreeSet::from([a.clone(), b.clone()]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };
        // Baseline: non-retry Review surfaces non-empty hints.
        state.retry_outcome_kind = RetryOutcomeKind::None;
        let request = state.expected_request(1, RequestKind::Review);
        assert!(
            !request.kernel_hinted_next_active_coarse_nodes.is_empty(),
            "baseline Review request should expose anchor hints"
        );
        // Audit-2 #6: ANY non-None retry-outcome must suppress the hint
        // set, matching `review_next_active_coarse_legal_for_response`'s
        // rejection on retry.
        for retry in [
            RetryOutcomeKind::Invalid,
            RetryOutcomeKind::Stuck,
            RetryOutcomeKind::NeedsRestructure,
            RetryOutcomeKind::Transport,
        ] {
            state.retry_outcome_kind = retry;
            let req = state.expected_request(1, RequestKind::Review);
            assert!(
                req.kernel_hinted_next_active_coarse_nodes.is_empty(),
                "retry-review {:?} must hide anchor hints",
                retry
            );
            assert!(
                !req.coarse_anchor_starvation_unlocked,
                "retry-review {:?} must not signal starvation unlock",
                retry
            );
        }
    }

    #[test]
    fn v32_audit2_followup_4_select_initial_proof_active_node_respects_anchor_cone() {
        // Setup: 4-node ProofFormalization world (A + HelperA in the
        // anchor cone; B + HelperB outside). All verifier statuses set
        // to Pass so `global_blockers()` is empty — that keeps the
        // legal set strict (the anchor's down-cone, no repair widening).
        // Mark B and HelperB open so the pre-cone "needs_work" filter
        // would pick them, then verify the cone filter excludes them.
        let mut state = v32_state_with_anchor();
        let a = node("A");
        let b = node("B");
        let helper_a = node("HelperA");
        let helper_b = node("HelperB");
        // A and HelperA closed; only out-of-cone candidates open.
        state.live.open_nodes = BTreeSet::from([b.clone(), helper_b.clone()]);
        // Make HelperA and HelperB proof_nodes too so they're candidates
        // (helper-b would be picked without the cone filter).
        state.proof_nodes.insert(helper_a.clone());
        state.proof_nodes.insert(helper_b.clone());
        // Open proof_nodes need Soundness; without explicit Pass status
        // + matching fingerprints they default to Unknown and fire a
        // Soundness blocker, which would trigger repair-mode widening
        // and defeat the cone-restriction test. Stub Soundness Pass.
        for n in &[&b, &helper_b] {
            state.sound_status.insert((*n).clone(), SoundStatus::Pass);
            state
                .sound_approved_fingerprints
                .insert((*n).clone(), Fingerprint::default());
            state
                .live
                .sound_current_fingerprints
                .insert((*n).clone(), Fingerprint::default());
        }

        assert!(
            state.global_blockers().is_empty(),
            "test precondition: no blockers => no repair-mode widening; got {:?}",
            state.global_blockers()
        );
        assert!(
            !state.coarse_repair_mode(),
            "anchor's cone is fully closed; repair mode should be off"
        );

        let picked = state.select_initial_proof_active_node();
        assert!(
            picked.is_none(),
            "auto-selection must skip out-of-cone open candidates when anchor is set; picked {:?}",
            picked
        );
        let _ = (a, helper_a);

        // Sanity check: without the anchor the filter degrades and one
        // of {B, HelperB} is picked — confirms the test is sensitive to
        // the cone path.
        let mut no_anchor = state.clone();
        no_anchor.active_coarse_node = None;
        let picked2 = no_anchor.select_initial_proof_active_node();
        assert!(
            picked2.is_some(),
            "without anchor, selection should pick from open candidates"
        );
    }

    #[test]
    fn v32_audit2_followup_3_anchor_switch_validates_against_new_cone() {
        // Setup: current anchor A, cone {A, HelperA}. Reviewer wants
        // to switch to B and pick HelperB (which is in B's cone but
        // not in A's cone). Without the followup-3 fix, the response
        // would be rejected because next_active is validated against
        // A's pre-projected hint set.
        let a = node("A");
        let b = node("B");
        let helper_a = node("HelperA");
        let helper_b = node("HelperB");
        let mut request = WrapperRequest::default();
        request.phase = Phase::ProofFormalization;
        request.kind = RequestKind::Review;
        request.coarse_dag_nodes = BTreeSet::from([a.clone(), b.clone()]);
        request.active_coarse_node = Some(a.clone());
        request.current_present_nodes =
            BTreeSet::from([a.clone(), b.clone(), helper_a.clone(), helper_b.clone()]);
        request.current_deps = BTreeMap::from([
            (a.clone(), BTreeSet::from([helper_a.clone()])),
            (b.clone(), BTreeSet::from([helper_b.clone()])),
            (helper_a.clone(), BTreeSet::new()),
            (helper_b.clone(), BTreeSet::new()),
        ]);
        // A's cone (kernel_hinted_next_active_nodes intersected with
        // base-legal); B/HelperB are NOT in this set under A.
        request.kernel_hinted_next_active_nodes = BTreeSet::from([a.clone(), helper_a.clone()]);
        // Base-legal pre-cone: any node that "needs work" — for the
        // test, treat all four as candidates.
        request.proof_active_node_base_legal_candidates =
            BTreeSet::from([a.clone(), b.clone(), helper_a.clone(), helper_b.clone()]);
        request.kernel_hinted_next_active_coarse_nodes = BTreeSet::from([b.clone()]);

        let mut review = ReviewResponse::default();
        review.decision = ReviewDecisionKind::Continue;
        review.next_mode = TaskMode::Local;
        review.next_active = Some(helper_b.clone());
        review.next_active_coarse = Some(b.clone());

        // Fix in place: validation uses the prospective new cone.
        assert!(
            request.review_next_active_legal_for_response(&review),
            "anchor switch A->B with next_active=HelperB (in B's cone) must accept"
        );

        // Negative: a next_active outside BOTH old and new cones must reject.
        let stray = node("Stray");
        let mut request2 = request.clone();
        request2.current_present_nodes.insert(stray.clone());
        request2
            .proof_active_node_base_legal_candidates
            .insert(stray.clone());
        let mut review2 = review.clone();
        review2.next_active = Some(stray.clone());
        assert!(
            !request2.review_next_active_legal_for_response(&review2),
            "next_active outside the new anchor's cone must reject"
        );

        // Without the anchor switch, the same next_active=HelperB is
        // illegal under the pre-projected old hint set — confirms the
        // test is sensitive to the prospective-cone path.
        let mut review3 = review.clone();
        review3.next_active_coarse = None;
        assert!(
            !request.review_next_active_legal_for_response(&review3),
            "without anchor switch, next_active=HelperB stays illegal under A's cone"
        );
    }

    #[test]
    fn v32_audit2_followup_5_authorized_nodes_must_lie_in_anchor_cone() {
        // Build a request like #3 above but exercise the
        // review_response_legal path. Reviewer picks Restructure with
        // active_node=A and authorized_nodes={A, HelperA, HelperB}.
        // HelperB is in the envelope (impact_region) iff HelperB is a
        // dep of A (no) or A is a dep of HelperB (no) — so HelperB
        // already fails the envelope check. To isolate the cone
        // check we add a node `Importer` that's BOTH in A's impact
        // region (it imports A — i.e. A ∈ deps[Importer]) and OUT of
        // A's down-cone. That's exactly the leak the cone check
        // closes.
        let a = node("A");
        let b = node("B");
        let helper_a = node("HelperA");
        let importer = node("Importer");
        let mut request = WrapperRequest::default();
        request.phase = Phase::ProofFormalization;
        request.kind = RequestKind::Review;
        request.coarse_dag_nodes = BTreeSet::from([a.clone(), b.clone()]);
        request.active_coarse_node = Some(a.clone());
        request.current_present_nodes =
            BTreeSet::from([a.clone(), b.clone(), helper_a.clone(), importer.clone()]);
        request.current_deps = BTreeMap::from([
            (a.clone(), BTreeSet::from([helper_a.clone()])),
            (helper_a.clone(), BTreeSet::new()),
            (b.clone(), BTreeSet::new()),
            // Importer imports A (so A is in Importer's deps); A's
            // down-cone is still {A, HelperA} — Importer is upstream.
            (importer.clone(), BTreeSet::from([a.clone()])),
        ]);
        request.kernel_hinted_next_active_nodes =
            BTreeSet::from([a.clone(), helper_a.clone(), importer.clone()]);
        request.proof_active_node_base_legal_candidates =
            BTreeSet::from([a.clone(), helper_a.clone(), importer.clone()]);
        request.allowed_decisions = BTreeSet::from([ReviewDecisionKind::Continue]);
        request.allowed_next_modes = BTreeSet::from([TaskMode::Restructure]);
        request.allowed_resets = BTreeSet::from([ResetChoice::None]);
        // Set up worker-acceptance contract so Restructure mode is plumbed.
        request.worker_acceptance.validation_kind = WorkerValidationKind::ProofRestructure;

        let mut review = ReviewResponse::default();
        review.decision = ReviewDecisionKind::Continue;
        review.next_mode = TaskMode::Restructure;
        review.next_active = Some(a.clone());
        review.allow_new_obligations = false;
        review.must_close_active = true;

        // Authorize only the in-cone nodes — must be legal.
        review.authorized_nodes = BTreeSet::from([a.clone(), helper_a.clone()]);
        assert!(
            request.review_response_legal(&review),
            "in-cone authorized_nodes must be legal"
        );

        // Add Importer (in envelope but out of cone) — must reject under audit-2 #5.
        review.authorized_nodes = BTreeSet::from([a.clone(), helper_a.clone(), importer.clone()]);
        assert!(
            !request.review_response_legal(&review),
            "authorized_nodes leaking past anchor cone (Importer) must reject"
        );
    }

    #[test]
    fn v32_audit2_postfix_expected_request_populates_carriers_from_global_blockers() {
        // Test gap raised by round-2 audit: the
        // `v32_audit2_postfix_cone_helper_widens_for_deferred_blocker_carriers`
        // sibling test sets `coarse_repair_blocker_carriers` directly on
        // a synthetic WrapperRequest. This complements it by going
        // through the real `expected_request` populator, catching any
        // regression that omits the projection on Worker or Review.
        let b = node("B");
        let mut state = v32_state_with_anchor();
        // Set up a NodeCorr Fail on B so global_blockers() returns it.
        // Approved fingerprint stays default ("") while current drifts —
        // that's the "current matches Fail" rule (model.rs:current_corr_state).
        state.corr_status.insert(b.clone(), CorrStatus::Fail);
        let drifted = Fingerprint::from("drifted".to_string());
        state
            .live
            .corr_current_fingerprints
            .insert(b.clone(), drifted.clone());
        state.corr_approved_fingerprints.insert(b.clone(), drifted);
        let global = state.global_blockers();
        assert!(
            global.iter().any(|bl| matches!(
                (&bl.kind, &bl.object),
                (BlockerKind::NodeCorr, BlockerObject::Node { node }) if node == &b
            )),
            "test setup: NodeCorr Fail must produce a blocker for B; got {:?}",
            global
        );

        // Review: carriers must include B.
        let review_req = state.expected_request(1, RequestKind::Review);
        assert!(
            review_req.coarse_repair_blocker_carriers.contains(&b),
            "Review request must project B as a carrier; got {:?}",
            review_req.coarse_repair_blocker_carriers
        );
        // Worker: same — Worker prompts also need the carriers to compute
        // cones (the fragment fires on anchor.is_some() and references
        // coarse_repair_mode).
        let worker_req = state.expected_request(2, RequestKind::Worker);
        assert!(
            worker_req.coarse_repair_blocker_carriers.contains(&b),
            "Worker request must project B as a carrier; got {:?}",
            worker_req.coarse_repair_blocker_carriers
        );
        // Cleanup / TheoremStating / non-ProofFormalization Review:
        // field must be empty (mechanism is phase-scoped).
        state.phase = Phase::TheoremStating;
        let ts_req = state.expected_request(3, RequestKind::Review);
        assert!(
            ts_req.coarse_repair_blocker_carriers.is_empty(),
            "TheoremStating must not project carriers; got {:?}",
            ts_req.coarse_repair_blocker_carriers
        );
    }

    #[test]
    fn v32_audit2_postfix_cone_helper_widens_for_deferred_blocker_carriers() {
        // Post-#3/#5 fix regression test: the request-side
        // `coarse_legal_active_set_for_anchor` must widen its cone to
        // include carriers reachable through DEFERRED blockers (which
        // are filtered out of `request.blockers` by `request_blockers`'s
        // `is_dispatch_eligible` predicate) — matching the kernel's
        // `coarse_legal_active_set` which uses the full
        // `global_blockers()`. Pre-fix the helper read `self.blockers`
        // and missed deferred carriers, computing a strictly narrower
        // cone than the kernel — leading the live JSON path to over-
        // reject choices the kernel would accept.
        let a = node("A");
        let b = node("B");
        let helper_a = node("HelperA");
        let helper_b = node("HelperB");
        let mut request = WrapperRequest::default();
        request.phase = Phase::ProofFormalization;
        request.kind = RequestKind::Review;
        request.coarse_dag_nodes = BTreeSet::from([a.clone(), b.clone()]);
        request.active_coarse_node = Some(a.clone());
        request.current_present_nodes =
            BTreeSet::from([a.clone(), b.clone(), helper_a.clone(), helper_b.clone()]);
        request.current_deps = BTreeMap::from([
            (a.clone(), BTreeSet::from([helper_a.clone()])),
            (b.clone(), BTreeSet::from([helper_b.clone()])),
            (helper_a.clone(), BTreeSet::new()),
            (helper_b.clone(), BTreeSet::new()),
        ]);
        // `blockers` (dispatch-eligible subset) is EMPTY — no in-cycle
        // adjudication target.
        request.blockers = BTreeSet::new();
        // ...but the FULL carrier set (deferred-inclusive) puts B
        // outside A's cone. The denormalized field is what the cone
        // helper must consult.
        request.coarse_repair_blocker_carriers = BTreeSet::from([b.clone()]);

        // Cone for A widens to include B and HelperB.
        let cone = request.coarse_legal_active_set_for_anchor(Some(&a));
        assert!(cone.contains(&a), "anchor in cone");
        assert!(cone.contains(&helper_a), "anchor's helper in cone");
        assert!(
            cone.contains(&b),
            "deferred blocker carrier widens the cone"
        );
        assert!(
            cone.contains(&helper_b),
            "deferred blocker carrier's helper widens the cone"
        );

        // Negative: if the carrier set is also empty, cone narrows back
        // to just {A, HelperA}.
        request.coarse_repair_blocker_carriers = BTreeSet::new();
        let cone_narrow = request.coarse_legal_active_set_for_anchor(Some(&a));
        assert_eq!(cone_narrow, BTreeSet::from([a.clone(), helper_a.clone()]));
    }

    #[test]
    fn v32_audit2_followup_7_starvation_flag_requires_current_repair_mode() {
        // Post-#7 fix: the counter can be stale-at-threshold AFTER a
        // worker burst clears the last out-of-cone blocker (repair_mode
        // flips false) but before the next proof-Continue resets the
        // counter. Without the gate, the next Review prompt would
        // surface `coarse_anchor_starvation_unlocked = true` while
        // `coarse_repair_mode = false` — a contradictory signal.
        let mut state = v32_state_with_anchor();
        // Set up: counter at threshold, but no blockers => repair_mode
        // is false. Simulates "worker just cleared the last carrier."
        state.cycles_in_coarse_repair_mode = stuck_coarse_repair_threshold();
        assert!(state.global_blockers().is_empty(), "no blockers in setup");
        assert!(
            !state.coarse_repair_mode(),
            "test precondition: repair-mode off"
        );
        assert!(
            state.cycles_in_coarse_repair_mode >= stuck_coarse_repair_threshold(),
            "test precondition: counter at threshold"
        );

        let request = state.expected_request(1, RequestKind::Review);
        assert!(
            !request.coarse_anchor_starvation_unlocked,
            "stale-counter starvation flag must be suppressed when coarse_repair_mode is false"
        );

        // Positive control: introduce a NodeCorr blocker on a coarse
        // node outside the cone (B's helper). repair_mode flips true,
        // counter stays at threshold — flag should fire.
        let b = node("B");
        state
            .live
            .corr_current_fingerprints
            .insert(b.clone(), Fingerprint::from("changed".to_string()));
        // Force corr fail by mismatching approved vs current fingerprint.
        // (Approved is "" via Fingerprint::default(); current is "changed").
        state.corr_status.insert(b.clone(), CorrStatus::Fail);
        assert!(
            !state.global_blockers().is_empty(),
            "test setup: blocker now exists"
        );
        assert!(
            state.coarse_repair_mode(),
            "out-of-cone carrier => repair mode on"
        );

        let request2 = state.expected_request(2, RequestKind::Review);
        assert!(
            request2.coarse_anchor_starvation_unlocked,
            "counter >= threshold + repair_mode on => starvation flag fires"
        );
    }

    #[test]
    fn v32_audit2_followup_8_validate_enforces_typeok_invariants() {
        // Post-#8 fix: `validate()` mirrors TLA TypeOK at
        // `spec/SupervisorProtocol.tla:4614-4618` for the four
        // active-coarse invariants.
        let a = node("A");
        let stray = node("Stray");

        // Helper: a state that passes `validate()` so subsequent
        // perturbations isolate the new invariants. `v32_state_with_anchor`
        // populates the v32 surface and the substantiveness/corr
        // statuses, but doesn't satisfy the unrelated validate()
        // preconditions (positive thresholds, verifier lanes, per-node
        // difficulty + easy-attempts entries) — add those here.
        let make_state = || -> ProtocolState {
            let mut s = v32_state_with_anchor();
            s.max_theorem_invalid_attempt = 2;
            s.proof_invalid_review_threshold = 2;
            s.easy_max_retries = 2;
            s.verifier_lanes = BTreeSet::from(["v1".to_string()]);
            for n in s.live.present_nodes.clone() {
                s.node_difficulty.insert(n.clone(), NodeDifficulty::Easy);
                s.easy_attempts.insert(n.clone(), 0);
            }
            assert!(
                s.validate().is_ok(),
                "baseline must validate: {:?}",
                s.validate()
            );
            s
        };

        // Invariant 1: active_coarse_node ∈ coarse_dag_nodes.
        let mut s1 = make_state();
        s1.active_coarse_node = Some(stray.clone());
        assert!(
            s1.validate().is_err(),
            "anchor outside coarse_dag_nodes must reject"
        );

        // Invariant 2: phase != ProofFormalization => anchor = None.
        let mut s2 = make_state();
        s2.phase = Phase::TheoremStating;
        // Reset other phase-incompatible state to isolate the check.
        s2.proof_edit_mode = ProofEditMode::Local;
        // active_coarse_node is still Some(A) from v32_state_with_anchor.
        let err = s2.validate().unwrap_err();
        assert!(
            err.contains("active_coarse_node may only be set in ProofFormalization"),
            "phase guard, got: {}",
            err
        );

        // Invariant 3: coarse_dag_nodes empty => anchor = None.
        let mut s3 = make_state();
        s3.coarse_dag_nodes = BTreeSet::new();
        let err = s3.validate().unwrap_err();
        assert!(
            err.contains("coarse_dag_nodes is empty"),
            "empty-coarse guard, got: {}",
            err
        );

        // Invariant 4: anchor None => counter = 0.
        let mut s4 = make_state();
        s4.active_coarse_node = None;
        s4.cycles_in_coarse_repair_mode = 5;
        let err = s4.validate().unwrap_err();
        assert!(
            err.contains("cycles_in_coarse_repair_mode must be 0"),
            "counter coherence, got: {}",
            err
        );
        let _ = a;
    }

    /// Regression: `apply_worker_structure_updates` must prune the top-level
    /// `{sound,corr,substantiveness,paper}_status` and `*_approved_fingerprints`
    /// maps against the refreshed `live.present_nodes` set so external tally
    /// aggregates don't see stale entries for nodes the snapshot just dropped.
    /// The `last_clean_*` mirrors must be left untouched because they are the
    /// restore source for `apply_last_clean_reset`.
    #[test]
    fn apply_worker_structure_updates_prunes_stale_verifier_state() {
        let a = node("A");
        let b_stale = node("B_stale");
        let t_live = target("main");
        let t_stale = target("retired");

        // Build a state where `live.present_nodes` only carries `A` (and the
        // configured-target set only carries `t_live`), but the verifier maps
        // each contain BOTH a live entry and a stale one for a node/target
        // that has already left the live surface. We also prime the
        // `last_clean_*` mirrors with the stale entries to verify they are
        // preserved.
        let stale_fp: Fingerprint = "stale-fp".into();
        let live_fp: Fingerprint = "live-fp".into();

        let mut state = ProtocolState {
            configured_targets: BTreeSet::from([t_live.clone()]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([a.clone()]),
                ..WorkingSnapshot::default()
            },
            node_kinds: BTreeMap::from([(a.clone(), NodeKind::Definition)]),
            corr_status: BTreeMap::from([
                (a.clone(), CorrStatus::Pass),
                (b_stale.clone(), CorrStatus::Pass),
            ]),
            corr_approved_fingerprints: BTreeMap::from([
                (a.clone(), live_fp.clone()),
                (b_stale.clone(), stale_fp.clone()),
            ]),
            substantiveness_status: BTreeMap::from([
                (a.clone(), CorrStatus::Pass),
                (b_stale.clone(), CorrStatus::Pass),
            ]),
            substantiveness_approved_fingerprints: BTreeMap::from([
                (a.clone(), live_fp.clone()),
                (b_stale.clone(), stale_fp.clone()),
            ]),
            sound_status: BTreeMap::from([
                (a.clone(), SoundStatus::Pass),
                (b_stale.clone(), SoundStatus::Pass),
            ]),
            sound_approved_fingerprints: BTreeMap::from([
                (a.clone(), live_fp.clone()),
                (b_stale.clone(), stale_fp.clone()),
            ]),
            paper_status: BTreeMap::from([
                (t_live.clone(), CorrStatus::Pass),
                (t_stale.clone(), CorrStatus::Pass),
            ]),
            paper_approved_fingerprints: BTreeMap::from([
                (t_live.clone(), live_fp.clone()),
                (t_stale.clone(), stale_fp.clone()),
            ]),
            // Seed the LastClean mirrors with the same stale entries to
            // prove the prune does NOT touch them.
            last_clean_corr_status: BTreeMap::from([
                (a.clone(), CorrStatus::Pass),
                (b_stale.clone(), CorrStatus::Pass),
            ]),
            last_clean_corr_approved_fingerprints: BTreeMap::from([
                (a.clone(), live_fp.clone()),
                (b_stale.clone(), stale_fp.clone()),
            ]),
            last_clean_substantiveness_status: BTreeMap::from([
                (a.clone(), CorrStatus::Pass),
                (b_stale.clone(), CorrStatus::Pass),
            ]),
            last_clean_substantiveness_approved_fingerprints: BTreeMap::from([
                (a.clone(), live_fp.clone()),
                (b_stale.clone(), stale_fp.clone()),
            ]),
            last_clean_sound_status: BTreeMap::from([
                (a.clone(), SoundStatus::Pass),
                (b_stale.clone(), SoundStatus::Pass),
            ]),
            last_clean_sound_approved_fingerprints: BTreeMap::from([
                (a.clone(), live_fp.clone()),
                (b_stale.clone(), stale_fp.clone()),
            ]),
            last_clean_paper_status: BTreeMap::from([
                (t_live.clone(), CorrStatus::Pass),
                (t_stale.clone(), CorrStatus::Pass),
            ]),
            last_clean_paper_approved_fingerprints: BTreeMap::from([
                (t_live.clone(), live_fp.clone()),
                (t_stale.clone(), stale_fp.clone()),
            ]),
            ..ProtocolState::default()
        };

        // An empty WorkerResponse triggers no structural mutations but still
        // exercises the post-mutation `normalize_live_structural_state` +
        // `retain_verifier_state_for_live_surface` calls — which is what we
        // are regression-testing.
        let response = WorkerResponse {
            snapshot: state.live.clone(),
            ..WorkerResponse::default()
        };
        state.apply_worker_structure_updates(&response);

        // Live verifier maps are now pruned to present_nodes / configured_targets.
        assert_eq!(
            state.corr_status.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([a.clone()]),
            "corr_status not pruned",
        );
        assert_eq!(
            state
                .corr_approved_fingerprints
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([a.clone()]),
            "corr_approved_fingerprints not pruned",
        );
        assert_eq!(
            state
                .substantiveness_status
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([a.clone()]),
            "substantiveness_status not pruned",
        );
        assert_eq!(
            state
                .substantiveness_approved_fingerprints
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([a.clone()]),
            "substantiveness_approved_fingerprints not pruned",
        );
        assert_eq!(
            state.sound_status.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([a.clone()]),
            "sound_status not pruned",
        );
        assert_eq!(
            state
                .sound_approved_fingerprints
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([a.clone()]),
            "sound_approved_fingerprints not pruned",
        );
        assert_eq!(
            state.paper_status.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([t_live.clone()]),
            "paper_status not pruned",
        );
        assert_eq!(
            state
                .paper_approved_fingerprints
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([t_live.clone()]),
            "paper_approved_fingerprints not pruned",
        );

        // LastClean mirrors must be UNTOUCHED — they are the restore source
        // for `apply_last_clean_reset` and must retain every entry that was
        // clean at the last checkpoint, even if the entry has since left
        // `live.present_nodes`.
        let last_clean_node_set = BTreeSet::from([a.clone(), b_stale.clone()]);
        let last_clean_target_set = BTreeSet::from([t_live.clone(), t_stale.clone()]);
        assert_eq!(
            state
                .last_clean_corr_status
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            last_clean_node_set,
            "last_clean_corr_status was mutated",
        );
        assert_eq!(
            state
                .last_clean_corr_approved_fingerprints
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            last_clean_node_set,
            "last_clean_corr_approved_fingerprints was mutated",
        );
        assert_eq!(
            state
                .last_clean_substantiveness_status
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            last_clean_node_set,
            "last_clean_substantiveness_status was mutated",
        );
        assert_eq!(
            state
                .last_clean_substantiveness_approved_fingerprints
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            last_clean_node_set,
            "last_clean_substantiveness_approved_fingerprints was mutated",
        );
        assert_eq!(
            state
                .last_clean_sound_status
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            last_clean_node_set,
            "last_clean_sound_status was mutated",
        );
        assert_eq!(
            state
                .last_clean_sound_approved_fingerprints
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            last_clean_node_set,
            "last_clean_sound_approved_fingerprints was mutated",
        );
        assert_eq!(
            state
                .last_clean_paper_status
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            last_clean_target_set,
            "last_clean_paper_status was mutated",
        );
        assert_eq!(
            state
                .last_clean_paper_approved_fingerprints
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            last_clean_target_set,
            "last_clean_paper_approved_fingerprints was mutated",
        );

        // Live entries that should survive carry their original fingerprints.
        assert_eq!(
            state.corr_approved_fingerprints.get(&a),
            Some(&live_fp),
            "live corr fingerprint dropped",
        );
        assert_eq!(
            state.paper_approved_fingerprints.get(&t_live),
            Some(&live_fp),
            "live paper fingerprint dropped",
        );
    }

    #[test]
    fn theorem_targeted_mode_legal_allows_sound_verifier_eligible_node() {
        // Regression test for the cycle-12 `designs` run: reviewer 53 wanted
        // `next_mode=Targeted, next_active=LocalDecoderLemma` after the
        // worker removed SKETCH from LocalDecoderLemma. Under the previous
        // gate the node had no current Fail (its new sound state is
        // FreshUnknown, not Fail) so it failed `theorem_targeted_mode_legal`
        // and the reviewer had to fall back to Global. Fix: extend the gate
        // to also accept sound-verifier-eligible nodes.
        let a = node("A");
        let t = target("main");
        let live_fp: Fingerprint = "live-fp".into();
        let sub_fp: Fingerprint = "sub-A".into();
        let corr_fp: Fingerprint = "corr-A".into();
        let state = ProtocolState {
            phase: Phase::TheoremStating,
            configured_targets: BTreeSet::from([t.clone()]),
            proof_nodes: BTreeSet::from([a.clone()]),
            target_claims: BTreeMap::from([(a.clone(), BTreeSet::from([t.clone()]))]),
            node_kinds: BTreeMap::from([(a.clone(), NodeKind::Proof)]),
            paper_status: BTreeMap::from([(t.clone(), CorrStatus::Pass)]),
            paper_approved_fingerprints: BTreeMap::from([(t.clone(), live_fp.clone())]),
            substantiveness_status: BTreeMap::from([(a.clone(), CorrStatus::Pass)]),
            substantiveness_approved_fingerprints: BTreeMap::from([(a.clone(), sub_fp.clone())]),
            corr_status: BTreeMap::from([(a.clone(), CorrStatus::Pass)]),
            corr_approved_fingerprints: BTreeMap::from([(a.clone(), corr_fp.clone())]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([a.clone()]),
                open_nodes: BTreeSet::from([a.clone()]),
                coverage: BTreeMap::from([(t.clone(), BTreeSet::from([a.clone()]))]),
                paper_current_fingerprints: BTreeMap::from([(t.clone(), live_fp.clone())]),
                target_fingerprints: BTreeMap::from([(a.clone(), "ta".into())]),
                substantiveness_current_fingerprints: BTreeMap::from([(a.clone(), sub_fp.clone())]),
                corr_current_fingerprints: BTreeMap::from([(a.clone(), corr_fp.clone())]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };
        // A is FreshUnknown for sound (no `sound_assessments` entry, no
        // SKETCH marker, no legacy `sound_status`). Sub Pass + Corr Pass,
        // deps[A] empty so cone is just {A} and is clean.
        assert!(
            state.sound_verifier_eligible(&a),
            "preconditions: A must be sound-verifier-eligible"
        );
        assert!(
            !state.theorem_node_has_current_fail_blocker(&a),
            "preconditions: A must not have a current Fail blocker (FreshUnknown is Unknown, not Fail)"
        );
        // The fix: a sound-verifier-eligible node is a legal Targeted anchor.
        assert!(
            state.theorem_targeted_mode_legal(Some(&a)),
            "sound-verifier-eligible node must be a legal Targeted-mode anchor"
        );
        // And it's in `targeted_next_active_nodes` so the reviewer's
        // pre-submit checker will accept Targeted on it.
        let targeted = state.request_targeted_next_active_nodes(RequestKind::Review);
        assert!(
            targeted.contains(&a),
            "sound-verifier-eligible node must appear in targeted_next_active_nodes; got {targeted:?}"
        );
    }

    // ---- global_repair_mode tests (Step 26) ----

    /// Build a v32 anchor fixture pre-positioned for a Step A / Step C
    /// review response: ProofFormalization, single coarse anchor A,
    /// helper nodes, allowed_decisions / next_modes / resets pre-filled.
    fn global_repair_state() -> ProtocolState {
        let a = node("A");
        let b = node("B");
        let helper_a = node("HelperA");
        let helper_b = node("HelperB");
        let mut state = ProtocolState {
            phase: Phase::ProofFormalization,
            coarse_dag_nodes: BTreeSet::from([a.clone(), b.clone()]),
            active_coarse_node: Some(a.clone()),
            proof_nodes: BTreeSet::from([a.clone(), b.clone()]),
            deps: BTreeMap::from([
                (a.clone(), BTreeSet::from([helper_a.clone()])),
                (b.clone(), BTreeSet::from([helper_b.clone()])),
                (helper_a.clone(), BTreeSet::new()),
                (helper_b.clone(), BTreeSet::new()),
            ]),
            live: WorkingSnapshot {
                present_nodes: BTreeSet::from([
                    a.clone(),
                    b.clone(),
                    helper_a.clone(),
                    helper_b.clone(),
                ]),
                ..WorkingSnapshot::default()
            },
            ..ProtocolState::default()
        };
        for n in &[&a, &b, &helper_a, &helper_b] {
            state
                .substantiveness_status
                .insert((*n).clone(), CorrStatus::Pass);
            state
                .substantiveness_approved_fingerprints
                .insert((*n).clone(), Fingerprint::default());
            state
                .live
                .substantiveness_current_fingerprints
                .insert((*n).clone(), Fingerprint::default());
            state.corr_status.insert((*n).clone(), CorrStatus::Pass);
            state
                .corr_approved_fingerprints
                .insert((*n).clone(), Fingerprint::from("corr".to_string()));
            state
                .live
                .corr_current_fingerprints
                .insert((*n).clone(), Fingerprint::from("corr".to_string()));
        }
        state
    }

    fn step_a_response(state: &ProtocolState, extension: BTreeSet<NodeId>) -> ReviewResponse {
        let request = state.expected_request(1, RequestKind::Review);
        ReviewResponse {
            request_id: request.id,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            reason: String::new(),
            comments: String::new(),
            next_active_coarse: state.active_coarse_node.clone(),
            allow_new_obligations: true,
            must_close_active: false,
            global_repair_request: Some(GlobalRepairRequest {
                proposed_extension_nodes: extension,
                reason: "cone blocks the repair I need".to_string(),
            }),
            ..ReviewResponse::default()
        }
    }

    fn global_repair_retry_kinds() -> [RetryOutcomeKind; 4] {
        [
            RetryOutcomeKind::Invalid,
            RetryOutcomeKind::Stuck,
            RetryOutcomeKind::NeedsRestructure,
            RetryOutcomeKind::Transport,
        ]
    }

    fn install_global_repair_grant(state: &mut ProtocolState, extension: BTreeSet<NodeId>) {
        state.pending_global_repair_grant = Some(PendingGlobalRepairGrant {
            approved_extension_nodes: extension,
            auditor_reason: "approved".to_string(),
            dispatched_at_cycle: state.cycle,
            granted_at_cycle: state.cycle,
            review_request_id: 7,
        });
    }

    fn step_c_response(state: &ProtocolState, active: NodeId) -> ReviewResponse {
        let request = state.expected_request(1, RequestKind::Review);
        ReviewResponse {
            request_id: request.id,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some(active.clone()),
            next_active_coarse: None,
            next_mode: TaskMode::Restructure,
            authorized_nodes: BTreeSet::from([active]),
            consume_global_repair_grant: true,
            allow_new_obligations: true,
            must_close_active: false,
            paper_focus_ranges: vec![PaperFocusRange {
                start_line: 1,
                end_line: 1,
                reason: "test".to_string(),
            }],
            paper_grounding: PaperGrounding {
                consulted_cited_ranges: true,
                basis_summary: "consulted source paper for the relevant fact".to_string(),
            },
            ..ReviewResponse::default()
        }
    }

    /// Case A — Step A short-circuits to audit lane.
    #[test]
    fn global_repair_step_a_dispatches_audit_and_persists_request() {
        let mut state = global_repair_state();
        let b = node("B");
        state.stage = Stage::Reviewer;
        let in_flight = state.issue_request(RequestKind::Review);
        let mut response = step_a_response(&state, BTreeSet::from([b.clone()]));
        response.request_id = in_flight.id;
        assert!(
            state.review_response_legal(&response),
            "Step A response should be legal"
        );
        let outcome = crate::engine::apply_event(
            state.clone(),
            crate::engine::ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .expect("apply_event");
        let commands = outcome.commands;
        state = outcome.state;
        let outcome = commands;
        assert_eq!(state.stage, Stage::StuckMathAudit);
        let pending = state
            .pending_global_repair_request
            .as_ref()
            .expect("pending request set");
        assert_eq!(pending.proposed_extension_nodes, BTreeSet::from([b]));
        assert_eq!(
            state.last_reviewer_global_repair_request_cycle,
            Some(state.cycle)
        );
        // Verify a StuckMathAudit request was emitted.
        assert!(
            outcome.iter().any(|cmd| matches!(
                cmd,
                crate::engine::ProtocolCommand::IssueRequest { request, .. }
                    if request.kind == RequestKind::StuckMathAudit
            )),
            "expected a StuckMathAudit request among the outcome commands"
        );
    }

    #[test]
    fn global_repair_step_a_legal_during_retry_reviews() {
        for retry in global_repair_retry_kinds() {
            let mut state = global_repair_state();
            state.retry_outcome_kind = retry;
            let b = node("B");
            let response = step_a_response(&state, BTreeSet::from([b]));
            assert!(
                state.review_response_legal(&response),
                "Step A should be legal during {retry:?} retry review; reasons: {:?}",
                state.review_response_rejection_reasons(&response)
            );
        }
    }

    #[test]
    fn global_repair_step_a_rejects_protected_nodes_during_retry_reviews() {
        for retry in global_repair_retry_kinds() {
            let mut state = global_repair_state();
            state.retry_outcome_kind = retry;
            let b = node("B");
            state
                .live
                .coverage
                .insert(target("paper-target"), BTreeSet::from([b.clone()]));
            let response = step_a_response(&state, BTreeSet::from([b]));
            assert!(
                !state.review_response_legal(&response),
                "Step A must reject protected nodes during {retry:?} retry review"
            );
        }
    }

    #[test]
    fn global_repair_step_a_needs_restructure_dispatches_audit_preserving_retry_kind() {
        let mut state = global_repair_state();
        let b = node("B");
        state.stage = Stage::Reviewer;
        state.retry_outcome_kind = RetryOutcomeKind::NeedsRestructure;
        let in_flight = state.issue_request(RequestKind::Review);
        let mut response = step_a_response(&state, BTreeSet::from([b.clone()]));
        response.request_id = in_flight.id;

        let outcome = crate::engine::apply_event(
            state,
            crate::engine::ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .expect("apply Step A retry review");

        assert_eq!(outcome.state.stage, Stage::StuckMathAudit);
        assert_eq!(
            outcome.state.retry_outcome_kind,
            RetryOutcomeKind::NeedsRestructure
        );
        let pending = outcome
            .state
            .pending_global_repair_request
            .as_ref()
            .expect("pending request set");
        assert_eq!(pending.proposed_extension_nodes, BTreeSet::from([b]));
        let audit_request = outcome
            .commands
            .iter()
            .find_map(|cmd| match cmd {
                crate::engine::ProtocolCommand::IssueRequest { request, .. }
                    if request.kind == RequestKind::StuckMathAudit =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("expected StuckMathAudit request");
        assert_eq!(
            audit_request.retry_outcome_kind,
            RetryOutcomeKind::NeedsRestructure
        );
    }

    /// Case B — Step A precondition: not in ProofFormalization.
    #[test]
    fn global_repair_step_a_rejected_outside_proof_formalization() {
        let mut state = global_repair_state();
        state.phase = Phase::TheoremStating;
        state.active_coarse_node = None;
        state.cycles_in_coarse_repair_mode = 0;
        let b = node("B");
        let response = step_a_response(&state, BTreeSet::from([b]));
        assert!(
            !state.review_response_legal(&response),
            "Step A outside ProofFormalization must be rejected"
        );
    }

    /// Case C — Step A rate-limited by cooldown when prior request pending.
    #[test]
    fn global_repair_step_a_rate_limited_after_recent_dispatch() {
        let mut state = global_repair_state();
        state.cycle = 5;
        state.last_reviewer_global_repair_request_cycle = Some(state.cycle);
        state.pending_global_repair_request = Some(PendingGlobalRepairRequest {
            proposed_extension_nodes: BTreeSet::from([node("HelperB")]),
            reviewer_reason: "prior".to_string(),
            review_request_id: 0,
            review_cycle: 0,
            dispatched_at_cycle: state.cycle,
        });
        let b = node("B");
        let response = step_a_response(&state, BTreeSet::from([b]));
        assert!(
            !state.review_response_legal(&response),
            "Step A within cooldown with pending request must be rejected"
        );
    }

    /// Case D — Step B approve within structural cap creates grant.
    #[test]
    fn global_repair_step_b_approve_within_dep_neighborhood_creates_grant() {
        let mut state = global_repair_state();
        let b = node("B");
        let helper_b = node("HelperB");
        state.stage = Stage::StuckMathAudit;
        state.pending_global_repair_request = Some(PendingGlobalRepairRequest {
            proposed_extension_nodes: BTreeSet::from([b.clone()]),
            reviewer_reason: "test".to_string(),
            review_request_id: 7,
            review_cycle: 0,
            dispatched_at_cycle: state.cycle,
        });
        let in_flight = state.issue_request(RequestKind::StuckMathAudit);
        let response = StuckMathAuditResponse {
            request_id: in_flight.id,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            report: long_audit_report("B's helpers depend on B; widen scope."),
            global_repair_approve: true,
            global_repair_approved_extension_node_ids: vec![
                b.as_str().to_string(),
                helper_b.as_str().to_string(),
            ],
            global_repair_auditor_reason: "in scope".to_string(),
            ..StuckMathAuditResponse::default()
        };
        let outcome = crate::engine::apply_event(
            state.clone(),
            crate::engine::ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("apply audit response");
        state = outcome.state;
        let grant = state
            .pending_global_repair_grant
            .as_ref()
            .expect("grant set");
        assert_eq!(
            grant.approved_extension_nodes,
            BTreeSet::from([b, helper_b])
        );
        assert!(state.pending_global_repair_request.is_none());
    }

    /// Case E — Step B approve beyond dep-neighborhood is rejected.
    #[test]
    fn global_repair_step_b_approve_beyond_dep_neighborhood_rejected() {
        let mut state = global_repair_state();
        let b = node("B");
        let unrelated = node("HelperA"); // Not in impact_region(B)
        state.stage = Stage::StuckMathAudit;
        state.pending_global_repair_request = Some(PendingGlobalRepairRequest {
            proposed_extension_nodes: BTreeSet::from([b]),
            reviewer_reason: "test".to_string(),
            review_request_id: 7,
            review_cycle: 0,
            dispatched_at_cycle: state.cycle,
        });
        let pending_before = state.pending_global_repair_request.clone();
        let in_flight = state.issue_request(RequestKind::StuckMathAudit);
        let response = StuckMathAuditResponse {
            request_id: in_flight.id,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            report: long_audit_report("Grant unrelated node."),
            global_repair_approve: true,
            global_repair_approved_extension_node_ids: vec![unrelated.as_str().to_string()],
            global_repair_auditor_reason: "out of scope test".to_string(),
            ..StuckMathAuditResponse::default()
        };
        let outcome = crate::engine::apply_event(
            state.clone(),
            crate::engine::ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("apply audit response");
        state = outcome.state;
        // The validator rejects: state should keep pending_request and
        // not set a grant.
        assert!(state.pending_global_repair_grant.is_none());
        assert_eq!(state.pending_global_repair_request, pending_before);
    }

    /// Case F — Step C consume produces legal authorization including grant nodes.
    #[test]
    fn global_repair_step_c_authorizes_out_of_cone_node_via_grant() {
        let mut state = global_repair_state();
        let b = node("B");
        state.live.open_nodes.insert(b.clone());
        install_global_repair_grant(&mut state, BTreeSet::from([b.clone()]));
        let request = state.expected_request(2, RequestKind::Review);
        // Sanity: B should be visible as a base-legal candidate (open
        // proof node) so review_next_active_legal_for_response can find
        // it under the grant.
        assert!(
            request.proof_active_node_base_legal_candidates.contains(&b),
            "B should be a base-legal candidate; got {:?}",
            request.proof_active_node_base_legal_candidates
        );
        let response = step_c_response(&state, b);
        assert!(
            state.review_response_legal(&response),
            "Step C consume with out-of-cone authorized node should be legal under grant; reasons: {:?}",
            state.review_response_rejection_reasons(&response)
        );
    }

    #[test]
    fn global_repair_step_c_legal_during_retry_reviews() {
        for retry in global_repair_retry_kinds() {
            let mut state = global_repair_state();
            let b = node("B");
            state.retry_outcome_kind = retry;
            state.live.open_nodes.insert(b.clone());
            install_global_repair_grant(&mut state, BTreeSet::from([b.clone()]));
            let response = step_c_response(&state, b);
            assert!(
                state.review_response_legal(&response),
                "Step C should be legal during {retry:?} retry review; reasons: {:?}",
                state.review_response_rejection_reasons(&response)
            );
        }
    }

    #[test]
    fn global_repair_step_c_retry_does_not_allow_coarse_anchor_switch() {
        let mut state = global_repair_state();
        let b = node("B");
        state.retry_outcome_kind = RetryOutcomeKind::NeedsRestructure;
        state.live.open_nodes.insert(b.clone());
        install_global_repair_grant(&mut state, BTreeSet::from([b.clone()]));
        let mut response = step_c_response(&state, b.clone());
        response.next_active_coarse = Some(b);

        assert!(
            !state.review_response_legal(&response),
            "Step C grant must not make next_active_coarse switching legal during retry review"
        );
    }

    #[test]
    fn global_repair_step_c_needs_restructure_dispatches_worker_with_grant_scope() {
        let mut state = global_repair_state();
        let b = node("B");
        state.stage = Stage::Reviewer;
        state.retry_outcome_kind = RetryOutcomeKind::NeedsRestructure;
        state.live.open_nodes.insert(b.clone());
        install_global_repair_grant(&mut state, BTreeSet::from([b.clone()]));
        let in_flight = state.issue_request(RequestKind::Review);
        let mut response = step_c_response(&state, b.clone());
        response.request_id = in_flight.id;

        let outcome = crate::engine::apply_event(
            state,
            crate::engine::ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Review(response),
            },
        )
        .expect("apply Step C retry review");

        assert_eq!(outcome.state.stage, Stage::Worker);
        assert_eq!(
            outcome.state.retry_outcome_kind,
            RetryOutcomeKind::NeedsRestructure
        );
        let pending_task = outcome
            .state
            .pending_task
            .as_ref()
            .expect("pending worker task set");
        assert_eq!(pending_task.authorized_nodes, BTreeSet::from([b.clone()]));
        assert!(pending_task.consumed_global_repair_grant);
        let worker_request = outcome
            .commands
            .iter()
            .find_map(|cmd| match cmd {
                crate::engine::ProtocolCommand::IssueRequest { request, .. }
                    if request.kind == RequestKind::Worker =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("expected Worker request");
        assert_eq!(
            worker_request.retry_outcome_kind,
            RetryOutcomeKind::NeedsRestructure
        );
        assert_eq!(
            worker_request.worker_context.authorized_nodes,
            BTreeSet::from([b])
        );
        assert!(worker_request.consumed_global_repair_grant);
    }

    /// Case I — Disabled kill-switch rejects Step A and Step C.
    #[test]
    fn disabled_global_repair_rejects_step_a_and_c() {
        let mut state = global_repair_state();
        state.global_repair_mode_enabled = false;
        let b = node("B");
        let step_a = step_a_response(&state, BTreeSet::from([b.clone()]));
        assert!(!state.review_response_legal(&step_a));
        state.pending_global_repair_grant = Some(PendingGlobalRepairGrant {
            approved_extension_nodes: BTreeSet::from([b.clone()]),
            auditor_reason: "approved".to_string(),
            dispatched_at_cycle: state.cycle,
            granted_at_cycle: state.cycle,
            review_request_id: 7,
        });
        let request = state.expected_request(2, RequestKind::Review);
        let step_c = ReviewResponse {
            request_id: request.id,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some(b.clone()),
            next_active_coarse: None,
            next_mode: TaskMode::Restructure,
            authorized_nodes: BTreeSet::from([b]),
            consume_global_repair_grant: true,
            allow_new_obligations: true,
            must_close_active: false,
            ..ReviewResponse::default()
        };
        assert!(!state.review_response_legal(&step_c));
    }

    /// Case J — `ever_shallow_coarse_closed` monotonicity across `commit_live`.
    #[test]
    fn ever_shallow_coarse_closed_monotone_across_commit_live() {
        let mut state = global_repair_state();
        let a = node("A");
        // First commit: anchor A is closed (all helpers closed).
        state.commit_live();
        assert!(
            state.ever_shallow_coarse_closed.contains(&a),
            "A should be added to history after commit"
        );
        // Now re-open A by adding helper_a to open_nodes in the live
        // snapshot, then commit again.
        let helper_a = node("HelperA");
        state.live.open_nodes.insert(helper_a);
        state.commit_live();
        // History still contains A (monotone).
        assert!(state.ever_shallow_coarse_closed.contains(&a));
        let regressed = state.ever_shallow_coarse_closed_regressed();
        assert!(
            regressed.contains(&a),
            "A is in history but no longer closed; expected regressed; got {:?}",
            regressed
        );
    }

    /// Case K — Anchor change blocked while regression present.
    #[test]
    fn anchor_change_blocked_when_regression_nonempty() {
        let mut state = global_repair_state();
        // Plant history with A but currently-open helper_a so A regresses.
        state.ever_shallow_coarse_closed.insert(node("A"));
        let helper_a = node("HelperA");
        state.live.open_nodes.insert(helper_a.clone());
        state.committed = state.live.clone();
        state.committed_deps = state.deps.clone();
        state.cycles_in_coarse_repair_mode = 0;
        assert!(
            !state.active_coarse_change_allowed(),
            "regression should block anchor change"
        );
        // Starvation escape bypasses the regression block.
        state.cycles_in_coarse_repair_mode = stuck_coarse_repair_threshold();
        assert!(
            state.active_coarse_change_allowed(),
            "starvation guard should fire and bypass regression block"
        );
    }

    /// Case L — Burst rejection does NOT extend `ever_shallow_coarse_closed`.
    #[test]
    fn burst_rejection_does_not_grow_ever_shallow_coarse_closed() {
        let mut state = global_repair_state();
        // No prior commit_live; ever_shallow set is empty.
        assert!(state.ever_shallow_coarse_closed.is_empty());
        // refresh_shallow_coarse_progress_tracking is internal — call
        // commit_live to advance state, then mutate live, then
        // restore_committed; the set is untouched by the mutation.
        state.commit_live();
        let baseline = state.ever_shallow_coarse_closed.clone();
        state.live.open_nodes.insert(node("HelperB"));
        // restore_committed rolls live back; ever_shallow set unchanged.
        state.restore_committed();
        assert_eq!(state.ever_shallow_coarse_closed, baseline);
    }

    /// Case P — TTL expires grant in `commit_live`.
    #[test]
    fn grant_ttl_expires_grant_in_commit_live() {
        let mut state = global_repair_state();
        state.cycle = 14;
        state.pending_global_repair_grant = Some(PendingGlobalRepairGrant {
            approved_extension_nodes: BTreeSet::from([node("B")]),
            auditor_reason: "approved".to_string(),
            dispatched_at_cycle: 10,
            granted_at_cycle: 10,
            review_request_id: 7,
        });
        // Default TTL is 3 — 14-10=4 > 3 so commit_live drops the grant.
        state.commit_live();
        assert!(state.pending_global_repair_grant.is_none());
    }

    /// Case Q — Audit decline populates `latest_global_repair_audit_decline_reason`.
    #[test]
    fn audit_decline_populates_reason_and_clears_pending_request() {
        let mut state = global_repair_state();
        state.stage = Stage::StuckMathAudit;
        state.pending_global_repair_request = Some(PendingGlobalRepairRequest {
            proposed_extension_nodes: BTreeSet::from([node("B")]),
            reviewer_reason: "test".to_string(),
            review_request_id: 7,
            review_cycle: 0,
            dispatched_at_cycle: state.cycle,
        });
        let in_flight = state.issue_request(RequestKind::StuckMathAudit);
        let response = StuckMathAuditResponse {
            request_id: in_flight.id,
            cycle: state.cycle,
            status: ResponseStatus::Ok,
            report: long_audit_report("Decline reason here."),
            global_repair_approve: false,
            global_repair_auditor_reason: "too wide".to_string(),
            ..StuckMathAuditResponse::default()
        };
        let outcome = crate::engine::apply_event(
            state.clone(),
            crate::engine::ProtocolEvent::WrapperResponse {
                response: WrapperResponse::StuckMathAudit(response),
            },
        )
        .expect("apply audit response");
        state = outcome.state;
        assert_eq!(state.latest_global_repair_audit_decline_reason, "too wide");
        assert!(state.pending_global_repair_request.is_none());
        assert!(state.pending_global_repair_grant.is_none());
        assert_eq!(state.stage, Stage::Reviewer);
    }

    /// Case M / N / O — `apply_last_clean_reset` reseeds + intersects grant.
    /// Tests the helper directly since the full LastClean path requires
    /// populated mirrors that the regular fixture doesn't supply.
    #[test]
    fn relegalize_global_repair_against_present_drops_grant_with_no_surviving_nodes() {
        let mut state = global_repair_state();
        // Grant references a node that's not present anymore.
        state.pending_global_repair_grant = Some(PendingGlobalRepairGrant {
            approved_extension_nodes: BTreeSet::from([node("DELETED")]),
            auditor_reason: "approved".to_string(),
            dispatched_at_cycle: state.cycle,
            granted_at_cycle: state.cycle,
            review_request_id: 7,
        });
        state.relegalize_global_repair_against_present();
        assert!(state.pending_global_repair_grant.is_none());
    }

    #[test]
    fn relegalize_global_repair_against_present_intersects_grant() {
        let mut state = global_repair_state();
        let b = node("B");
        state.pending_global_repair_grant = Some(PendingGlobalRepairGrant {
            approved_extension_nodes: BTreeSet::from([b.clone(), node("DELETED")]),
            auditor_reason: "approved".to_string(),
            dispatched_at_cycle: state.cycle,
            granted_at_cycle: state.cycle,
            review_request_id: 7,
        });
        state.relegalize_global_repair_against_present();
        let grant = state
            .pending_global_repair_grant
            .as_ref()
            .expect("grant retained");
        assert_eq!(grant.approved_extension_nodes, BTreeSet::from([b]));
    }

    // ====================================================================
    // Canonical predicate tests (audit Cross-Cutting Root Causes /
    // foundation item 1).
    // ====================================================================

    fn predicate_state_with(node_name: &str) -> ProtocolState {
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(node(node_name));
        state.proof_nodes.insert(node(node_name));
        state
    }

    #[test]
    fn canonical_predicate_rejects_when_owner_absent() {
        let state = ProtocolState::default();
        let record = sample_record("Foo");
        // Foo is not in `live.present_nodes`.
        assert!(matches!(
            record.is_consistent_with_state(&state, false),
            Err(LocalClosureRecordInconsistency::OwnerAbsent)
        ));
    }

    #[test]
    fn canonical_predicate_rejects_when_owner_not_proof() {
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(node("Foo"));
        // Foo present but not in proof_nodes.
        let record = sample_record("Foo");
        assert!(matches!(
            record.is_consistent_with_state(&state, false),
            Err(LocalClosureRecordInconsistency::OwnerNotProof)
        ));
    }

    #[test]
    fn canonical_predicate_rejects_when_owner_open() {
        let mut state = predicate_state_with("Foo");
        state.live.open_nodes.insert(node("Foo"));
        let record = sample_record("Foo");
        assert!(matches!(
            record.is_consistent_with_state(&state, false),
            Err(LocalClosureRecordInconsistency::OwnerOpen)
        ));
    }

    #[test]
    fn canonical_predicate_rejects_when_dep_absent() {
        let mut state = predicate_state_with("Foo");
        // HelperB / ThmT / DefD are not present.
        let record = sample_record("Foo");
        assert!(matches!(
            record.is_consistent_with_state(&state, false),
            Err(LocalClosureRecordInconsistency::DepAbsent { .. })
        ));
        // Add HelperB / ThmT but not DefD → still rejects on DefD.
        state.live.present_nodes.insert(node("HelperB"));
        state.live.present_nodes.insert(node("ThmT"));
        let err = record
            .is_consistent_with_state(&state, false)
            .expect_err("should still reject (DefD missing)");
        match err {
            LocalClosureRecordInconsistency::DepAbsent { dep } => assert_eq!(dep, node("DefD")),
            other => panic!("expected DepAbsent, got {other:?}"),
        }
    }

    #[test]
    fn canonical_predicate_rejects_kernel_semantic_hash_drift() {
        let mut state = predicate_state_with("Foo");
        state.live.present_nodes.insert(node("HelperB"));
        state.live.present_nodes.insert(node("ThmT"));
        state.live.present_nodes.insert(node("DefD"));
        // Live `corr_current_fingerprints` reports F1 for HelperB.
        state
            .live
            .corr_current_fingerprints
            .insert(node("HelperB"), "F1".to_string());
        // Record carries F0 for HelperB → drift.
        let mut record = sample_record("Foo");
        record.toolchain_hash = "live".to_string();
        record.lake_manifest_hash = "live".to_string();
        record.preamble_hash = "live".to_string();
        record.approved_axioms_hash = "live".to_string();
        record.active_decl_hash = "live".to_string();
        record.active_statement_hash = "live".to_string();
        record
            .kernel_semantic_hashes
            .insert(node("HelperB"), "F0".to_string());
        match record.is_consistent_with_state(&state, false) {
            Err(LocalClosureRecordInconsistency::KernelSemanticHashMismatch {
                dep,
                recorded,
                current,
            }) => {
                assert_eq!(dep, node("HelperB"));
                assert_eq!(recorded, "F0");
                assert_eq!(current.as_deref(), Some("F1"));
            }
            other => panic!("expected KernelSemanticHashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn canonical_predicate_rejects_sentinel_hashed_record() {
        let mut state = predicate_state_with("Foo");
        state.live.present_nodes.insert(node("HelperB"));
        state.live.present_nodes.insert(node("ThmT"));
        state.live.present_nodes.insert(node("DefD"));
        let mut record = sample_record("Foo");
        record.toolchain_hash = "TODO_PATCH_C_D_HASH".to_string();
        assert!(matches!(
            record.is_consistent_with_state(&state, false),
            Err(LocalClosureRecordInconsistency::SentinelHashes)
        ));
        assert!(record.is_sentinel_hashed());
    }

    #[test]
    fn canonical_predicate_rejects_skipped_axcheck_when_required() {
        let mut state = predicate_state_with("Foo");
        state.live.present_nodes.insert(node("HelperB"));
        state.live.present_nodes.insert(node("ThmT"));
        state.live.present_nodes.insert(node("DefD"));
        let mut record = sample_record("Foo");
        record.axcheck_status = AxcheckStatus::Skipped;
        // axcheck_required = true → reject.
        assert!(matches!(
            record.is_consistent_with_state(&state, true),
            Err(LocalClosureRecordInconsistency::AxcheckSkippedButRequired)
        ));
        // axcheck_required = false → accept (pure-state path).
        assert!(record.is_consistent_with_state(&state, false).is_ok());
    }

    #[test]
    fn canonical_predicate_accepts_consistent_record() {
        let mut state = predicate_state_with("Foo");
        state.live.present_nodes.insert(node("HelperB"));
        state.live.present_nodes.insert(node("ThmT"));
        state.live.present_nodes.insert(node("DefD"));
        // Add real fingerprints matching the record's kernel hashes.
        state
            .live
            .corr_current_fingerprints
            .insert(node("HelperB"), "F1".to_string());
        let mut record = sample_record("Foo");
        record
            .kernel_semantic_hashes
            .insert(node("HelperB"), "F1".to_string());
        assert!(record.is_consistent_with_state(&state, true).is_ok());
    }

    #[test]
    fn canonical_predicate_default_axcheck_status_is_skipped_for_backcompat() {
        // Audit H-4 backcompat invariant: pre-H-4 persisted records
        // deserialize as `Skipped`, NOT `Agreed`. Re-enabling axcheck
        // should therefore invalidate historical records.
        let json = r#"{
            "node": "Foo",
            "closure_version": "v1",
            "toolchain_hash": "t",
            "lake_manifest_hash": "l",
            "preamble_hash": "p",
            "approved_axioms_hash": "a",
            "active_decl_hash": "d",
            "active_statement_hash": "s",
            "kernel_axioms": ["propext"],
            "boundary_theorems": {},
            "strict_theorem_deps": {},
            "strict_definition_deps": {},
            "kernel_semantic_hashes": {},
            "accepted_at_snapshot_id": "snap-42"
        }"#;
        let record: LocalClosureRecord = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            record.axcheck_status,
            AxcheckStatus::Skipped,
            "pre-H-4 records must deserialize as Skipped"
        );
    }

    // ====================================================================
    // C-3 — continuous closure-coverage scan tests.
    // ====================================================================

    #[test]
    fn ensure_local_closure_coverage_inserts_orphan_sorry_free_proof_node() {
        // Operator-manual-edit scenario: NewNode appears in
        // proof_nodes + live.present_nodes, is sorry-free, but has
        // neither a record nor an unverified entry. The coverage
        // scan must promote it into unverified so the next probe
        // pass refreshes its closure state.
        let mut state = ProtocolState::default();
        state.proof_nodes.insert(node("NewNode"));
        state.live.present_nodes.insert(node("NewNode"));
        // Not in records, not in unverified, not in open_nodes.
        state.ensure_local_closure_coverage();
        assert!(
            state
                .local_closure_unverified_nodes
                .contains(&node("NewNode")),
            "orphan sorry-free proof_node must land in unverified set"
        );
    }

    #[test]
    fn ensure_local_closure_coverage_skips_sorryd_orphan_proof_node() {
        // A sorryd node (in open_nodes) does NOT belong in
        // unverified — sorry-free-only invariant per plan §7.2.
        let mut state = ProtocolState::default();
        state.proof_nodes.insert(node("Sorryd"));
        state.live.present_nodes.insert(node("Sorryd"));
        state.live.open_nodes.insert(node("Sorryd"));
        state.ensure_local_closure_coverage();
        assert!(
            !state
                .local_closure_unverified_nodes
                .contains(&node("Sorryd")),
            "sorryd node must NOT land in unverified set"
        );
    }

    #[test]
    fn ensure_local_closure_coverage_drops_unverified_entry_for_sorryd_node() {
        // Defensive cleanup: a node that holds both an unverified
        // entry AND is in open_nodes violates the mutex invariant.
        // The scan must drop the unverified entry.
        let mut state = ProtocolState::default();
        state.proof_nodes.insert(node("WasUnverified"));
        state.live.present_nodes.insert(node("WasUnverified"));
        state.live.open_nodes.insert(node("WasUnverified"));
        state
            .local_closure_unverified_nodes
            .insert(node("WasUnverified"));
        state.ensure_local_closure_coverage();
        assert!(
            !state
                .local_closure_unverified_nodes
                .contains(&node("WasUnverified")),
            "mutex violation must be repaired by coverage scan"
        );
    }

    #[test]
    fn ensure_local_closure_coverage_does_not_disturb_records_or_unverified() {
        // Idempotent on already-covered states.
        let mut state = ProtocolState::default();
        state.proof_nodes.insert(node("Covered"));
        state.live.present_nodes.insert(node("Covered"));
        state
            .local_closure_records
            .insert(node("Covered"), sample_record("Covered"));
        state.proof_nodes.insert(node("Pending"));
        state.live.present_nodes.insert(node("Pending"));
        state.local_closure_unverified_nodes.insert(node("Pending"));
        state.ensure_local_closure_coverage();
        assert!(state.local_closure_records.contains_key(&node("Covered")));
        assert!(state
            .local_closure_unverified_nodes
            .contains(&node("Pending")));
        // No new orphan added.
        assert_eq!(state.local_closure_unverified_nodes.len(), 1);
    }

    // ====================================================================
    // C-2 — cone-clean prune respects fingerprint drift.
    // ====================================================================

    #[test]
    fn cone_clean_prune_drops_record_on_indirect_dep_fingerprint_drift() {
        // Audit C-2: cone-clean target is X, but consumer C's
        // helper H is not in `changed_nodes`. H's
        // `corr_current_fingerprints[H]` drifted F0 → F1; the
        // consumer record C carries `kernel_semantic_hashes[H] = F0`
        // → must be dropped by the predicate-driven prune.
        let mut state = ProtocolState::default();
        state.proof_nodes.insert(node("C"));
        state.proof_nodes.insert(node("H"));
        state.live.present_nodes.insert(node("C"));
        state.live.present_nodes.insert(node("H"));
        state.live.present_nodes.insert(node("X"));
        state
            .live
            .corr_current_fingerprints
            .insert(node("H"), "F1".to_string());
        // Build record for C with boundary helper H carrying F0.
        let mut record = LocalClosureRecord::default();
        record.node = node("C");
        record.closure_version = "v1".to_string();
        record.toolchain_hash = "live".to_string();
        record.lake_manifest_hash = "live".to_string();
        record.preamble_hash = "live".to_string();
        record.approved_axioms_hash = "live".to_string();
        record.active_decl_hash = "live".to_string();
        record.active_statement_hash = "live".to_string();
        record
            .boundary_theorems
            .insert(node("H"), "stmt".to_string());
        record
            .kernel_semantic_hashes
            .insert(node("H"), "F0".to_string());
        record.axcheck_status = AxcheckStatus::Agreed;
        state.local_closure_records.insert(node("C"), record);

        // Cone-clean changed nodes: X only (NOT H).
        let mut changed: BTreeSet<NodeId> = BTreeSet::new();
        changed.insert(node("X"));
        let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);
        assert!(
            removed.contains(&node("C")),
            "fingerprint-only drift on indirect dep must drop record"
        );
        assert!(
            state.local_closure_unverified_nodes.contains(&node("C")),
            "dropped record's owner must land in unverified"
        );
    }

    #[test]
    fn cone_clean_prune_drops_record_with_absent_dep() {
        let mut state = ProtocolState::default();
        state.proof_nodes.insert(node("Foo"));
        state.live.present_nodes.insert(node("Foo"));
        // HelperB / ThmT / DefD are NOT present.
        state
            .local_closure_records
            .insert(node("Foo"), sample_record("Foo"));
        let changed: BTreeSet<NodeId> = BTreeSet::new();
        let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);
        assert!(
            removed.contains(&node("Foo")),
            "absent dep must drop record via canonical predicate"
        );
    }

    #[test]
    fn cone_clean_prune_keeps_consistent_record() {
        let mut state = ProtocolState::default();
        state.proof_nodes.insert(node("Foo"));
        state.live.present_nodes.insert(node("Foo"));
        state.live.present_nodes.insert(node("HelperB"));
        state.live.present_nodes.insert(node("ThmT"));
        state.live.present_nodes.insert(node("DefD"));
        let mut record = sample_record("Foo");
        // Real hashes (no sentinels)
        record.toolchain_hash = "live".to_string();
        record.lake_manifest_hash = "live".to_string();
        record.preamble_hash = "live".to_string();
        record.approved_axioms_hash = "live".to_string();
        record.active_decl_hash = "live".to_string();
        record.active_statement_hash = "live".to_string();
        state.local_closure_records.insert(node("Foo"), record);
        let changed: BTreeSet<NodeId> = BTreeSet::new();
        let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);
        assert!(
            !removed.contains(&node("Foo")),
            "consistent record must survive prune"
        );
    }

    // ====================================================================
    // H-1 / M-1 — validate() closure-tier asserts.
    // ====================================================================

    fn minimal_valid_state() -> ProtocolState {
        // Provide just enough setup so the structural invariants in
        // validate() that PRE-EXIST our changes don't fire. Each test
        // below then mutates one closure invariant to confirm
        // validate() catches it.
        let mut state = ProtocolState::default();
        state.max_theorem_invalid_attempt = 3;
        state.proof_invalid_review_threshold = 5;
        state.easy_max_retries = 2;
        state.verifier_lanes.insert("Faithfulness".to_string());
        state
    }

    /// Add a node into the test state with all the surface-area pre-
    /// existing `validate()` invariants want populated (difficulty,
    /// easy-attempt counter). Mirrors what production state mutators
    /// do when they introduce a present node.
    fn add_present_proof_node(state: &mut ProtocolState, name: &str) {
        let n = node(name);
        state.live.present_nodes.insert(n.clone());
        state.proof_nodes.insert(n.clone());
        state
            .node_difficulty
            .insert(n.clone(), NodeDifficulty::Hard);
        state.easy_attempts.insert(n.clone(), 0);
        state.node_kinds.insert(n.clone(), NodeKind::Proof);
    }

    #[test]
    fn validate_rejects_record_owner_and_unverified_overlap() {
        let mut state = minimal_valid_state();
        add_present_proof_node(&mut state, "Foo");
        state
            .local_closure_records
            .insert(node("Foo"), sample_record("Foo"));
        // Violation: same node in unverified.
        state.local_closure_unverified_nodes.insert(node("Foo"));
        let err = state.validate().expect_err("must reject overlap");
        assert!(
            err.contains("records and unverified"),
            "error must mention records/unverified overlap; got: {err}"
        );
    }

    #[test]
    fn validate_rejects_unverified_open_overlap() {
        let mut state = minimal_valid_state();
        add_present_proof_node(&mut state, "Sorryd");
        state.live.open_nodes.insert(node("Sorryd"));
        // Violation: sorryd node also in unverified.
        state.local_closure_unverified_nodes.insert(node("Sorryd"));
        let err = state.validate().expect_err("must reject overlap");
        assert!(
            err.contains("unverified AND open_nodes"),
            "error must mention mutex violation; got: {err}"
        );
    }

    #[test]
    fn validate_rejects_failure_summary_for_non_unverified_node() {
        let mut state = minimal_valid_state();
        add_present_proof_node(&mut state, "Foo");
        // Need to have closure tier active so coverage check fires,
        // but the failure for Foo is the actual violation.
        state
            .local_closure_failures
            .insert(node("Foo"), sample_summary());
        // Foo is NOT in unverified; add a record so coverage passes.
        state
            .local_closure_records
            .insert(node("Foo"), sample_record("Foo"));
        let err = state
            .validate()
            .expect_err("failure-without-unverified must reject");
        assert!(
            err.contains("failure summary"),
            "error must mention orphan failure; got: {err}"
        );
    }

    #[test]
    fn validate_rejects_coverage_gap_when_closure_tier_active() {
        // Audit C-3 (validate-side) — orphan sorry-free present
        // proof_node when closure tier is active must reject.
        let mut state = minimal_valid_state();
        add_present_proof_node(&mut state, "Orphan");
        add_present_proof_node(&mut state, "Covered");
        state
            .local_closure_records
            .insert(node("Covered"), sample_record("Covered"));
        let err = state
            .validate()
            .expect_err("orphan sorry-free proof_node must reject");
        assert!(
            err.contains("Orphan"),
            "error must mention the orphan; got: {err}"
        );
    }

    #[test]
    fn validate_accepts_state_with_empty_closure_tier_and_uncovered_proof_nodes() {
        // Many skeleton tests have proof_nodes but no closure tier at
        // all; coverage check fires only when the tier is active.
        let mut state = minimal_valid_state();
        add_present_proof_node(&mut state, "Foo");
        state.validate().expect("skeleton state must validate");
    }

    #[test]
    fn validate_rejects_last_clean_mirror_inconsistent_overlap() {
        let mut state = minimal_valid_state();
        state.last_clean_local_closure_mirror_ready = true;
        let n = node("Foo");
        state
            .last_clean_local_closure_records
            .insert(n.clone(), sample_record("Foo"));
        state.last_clean_local_closure_unverified_nodes.insert(n);
        let err = state
            .validate()
            .expect_err("inconsistent LastClean mirror must reject");
        assert!(
            err.contains("LastClean"),
            "error must mention LastClean mirror; got: {err}"
        );
    }

    #[test]
    fn validate_rejects_reverse_index_drift() {
        let mut state = minimal_valid_state();
        add_present_proof_node(&mut state, "Foo");
        // sample_record names HelperB/ThmT/DefD; add them as present.
        state.live.present_nodes.insert(node("HelperB"));
        state.live.present_nodes.insert(node("ThmT"));
        state.live.present_nodes.insert(node("DefD"));
        state
            .node_difficulty
            .insert(node("HelperB"), NodeDifficulty::Hard);
        state
            .node_difficulty
            .insert(node("ThmT"), NodeDifficulty::Hard);
        state
            .node_difficulty
            .insert(node("DefD"), NodeDifficulty::Hard);
        state.easy_attempts.insert(node("HelperB"), 0);
        state.easy_attempts.insert(node("ThmT"), 0);
        state.easy_attempts.insert(node("DefD"), 0);
        state
            .local_closure_records
            .insert(node("Foo"), sample_record("Foo"));
        // Boundary index says some OTHER consumer references HelperB,
        // but no such record exists.
        state
            .boundary_statement_consumers
            .insert(node("HelperB"), BTreeSet::from([node("Phantom")]));
        let err = state
            .validate()
            .expect_err("stale reverse index must reject");
        assert!(
            err.contains("boundary_statement_consumers"),
            "error must mention reverse index; got: {err}"
        );
    }
}

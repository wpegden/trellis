//! PV under-model (Slice 2) — the ASSUMPTIONS REGISTRY (disk side).
//!
//! An Aeneas under-model assumption `C` is a Rust LANGUAGE/COMPILER guarantee
//! (§5a: slice/alloc ≤ `isize::MAX`, refs non-null, valid discriminants, `str`
//! UTF-8, layout/niche) that the extraction dropped — a FAITHFULNESS PATCH that
//! restores a compiler guarantee, adding ZERO new trust. It is NEVER an
//! `unsafe`-precondition or program-maintained invariant (those are proof
//! obligations).
//!
//! Three on-disk states, layered onto the EXISTING fail-closed closure-gate
//! machinery (`load_approved_axioms` in `runtime_cli_observations.rs`):
//!
//! - **Staged/Proposed** → the worker writes a marked block into
//!   `Tablet/Assumptions.lean` and the paired NL block into
//!   `Tablet/Assumptions.tex`; ordinary NodeCorr then gates that pair. If the
//!   distinct assumptions lane also passes, `PROPOSED_ASSUMPTIONS.json` records
//!   it as `status:"pending"`. A pending entry is admitted PROVISIONALLY for
//!   closure verification (`lane_passed_pending_axioms`, unioned into
//!   `load_approved_axioms`) so dependent proofs are not quarantined for the
//!   cycles until the human gate; it is still outside the permanent TCB
//!   (`APPROVED_AXIOMS.json`) and the gate stays LIVE while it is pending. A
//!   gate REJECT revokes the provisional admission (status `"rejected"`
//!   shrinks the effective allowlist; stale-hash rescinding reverts every
//!   closure record blessed under it).
//! - **Approved** (human ratifies at the legacy `AssumptionReview` gate, or a
//!   restored pending batch is included in required-v1's sole `Advance` gate):
//!   the kernel first writes the disclosure sink, then flips the trust switch —
//!     1. `tcb_manifest.json` `tcb_disclosure` (`classification:
//!        "rust-undermodel-assumption"` + statement + justification + locator +
//!        provenance),
//!     2. `APPROVED_AXIOMS.json` `global` (the namespace-qualified format
//!        `load_approved_axioms` consumes, same as `setup_pv_repo.sh` seeds from
//!        `tcb_manifest.json`).
//!   The staged Lean/NL blocks remain in `Tablet/Assumptions.{lean,tex}`; approval
//!   only admits trust. It never rewrites Lean.
//!   Dependent closure records now cite a permanent human-ratified allowlist
//!   entry rather than a revocable provisional admission.
//! - **Rejected** (lane or human declines): the marked staged Lean/NL blocks are
//!   removed and any pending record is marked rejected with reason; the parked
//!   `Decide` target routes to `Disprove` or a genuine `NeedInput` (engine side).
//!
//! This module is the DISK side honored by the runtime CLI for the engine's
//! `ProjectApprovedAssumption` / `RenderAssumptionsReview` /
//! `RecordProposedAssumption` commands; the engine stays deterministic-state-
//! only (mirrors `dormant_store` / `tablet_root`). A lane-passed pending
//! assumption is usable only PROVISIONALLY through the effective closure
//! allowlist. `APPROVED_AXIOMS.json` remains the permanent, human-ratified
//! trust switch and is written only by `project_approved`; rejection shrinks
//! the effective allowlist and rescinds dependent closure records.
//!
//! CONSISTENCY BASIS (Slice 3, 2026-07-03): the ratified assumption set is
//! jointly consistent because it is jointly TRUE of Rust under one intended
//! interpretation — each validity hook reads as its type's
//! language-admissibility predicate, each opaque boundary primitive as its
//! documented Rust behavior on admissible values, and Rust itself is the
//! model of the set. Certification per axiom = corpus citation + adversarial
//! hunt + the human gate. Claims come in two classes (`claim_class`):
//! `behavior` (hook-conditioned facts about operations on valid values;
//! refutation-shaped hunts) and `domain` (language-guaranteed
//! existence/coverage of valid values; construction-shaped hunts). A `domain`
//! claim makes the set BINDING — the empty reading of the hook no longer
//! satisfies it — so a domain lane-pass takes the `AssumptionReview` gate
//! live mid-phase instead of waiting for end-of-phase. The mechanical safety
//! interlock is the TYPE-DRIVEN CONDITIONING RULE
//! (`staged_assumption_statement_errors`): every quantified variable of an
//! over-approximating Aeneas container type must sit under a Rust-validity
//! hook applied to it — the formal signature of "Rust is more restrictive
//! than the model here" is that the unconditional form is refutable
//! in-model, and hook-conditioning is what keeps the claim a statement about
//! Rust rather than about model-unreachable values.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// The classification stamped on an approved under-model assumption in
/// `tcb_manifest.json`'s `tcb_disclosure` (distinct from the
/// `aeneas-stdlib-prim` / `auto-derived-method` boundary-axiom classes the
/// extractor emits).
pub const UNDER_MODEL_CLASSIFICATION: &str = "rust-undermodel-assumption";

/// Claim classes (Slice 3). `behavior` = hook-conditioned facts about
/// operations on valid values; `domain` = language-guaranteed
/// existence/coverage of valid values.
pub const CLAIM_CLASS_BEHAVIOR: &str = "behavior";
pub const CLAIM_CLASS_DOMAIN: &str = "domain";

/// Aeneas structural container types that OVER-APPROXIMATE Rust: quantifying
/// over one admits model values no Rust execution realizes. A property of the
/// extractor stack (fixed per Aeneas-Std version), campaign-independent — so
/// this list is not per-campaign setup foresight. Matched as exact
/// dot-segment tokens inside binder types.
pub const OVER_APPROXIMATING_CONTAINER_TOKENS: [&str; 4] = ["Slice", "Str", "Vec", "Array"];

/// Empty → `behavior` (every pre-Slice-3 record is a behavior contract).
pub fn normalize_claim_class(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        CLAIM_CLASS_BEHAVIOR.to_string()
    } else {
        trimmed.to_string()
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '\''
}

/// Split `text` into identifier tokens (Lean-ish: dotted names stay whole).
fn ident_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if is_ident_char(c) {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Return every Rust-validity hook identifier named by `text`.
///
/// This is the single hook-name rule shared by assumption conditioning and
/// human-gate disclosure. Hook-derived axiom names deliberately do not
/// qualify: those carry an underscore suffix, while the seeded/minted hook
/// symbols themselves are single, unqualified CamelCase identifiers.
fn is_rust_validity_hook_identifier(token: &str) -> bool {
    token.starts_with("RustValid")
        && token.len() > "RustValid".len()
        && !token.contains('.')
        && !token.contains('_')
}

pub(crate) fn rust_validity_hook_names(text: &str) -> std::collections::BTreeSet<String> {
    ident_tokens(text)
        .into_iter()
        .filter(|token| is_rust_validity_hook_identifier(token))
        .collect()
}

/// True when a binder TYPE names an over-approximating container: some exact
/// dot-segment of some identifier token equals a listed container token.
fn type_names_over_approximating_container(binder_type: &str) -> Option<String> {
    for token in ident_tokens(binder_type) {
        for segment in token.split('.') {
            if OVER_APPROXIMATING_CONTAINER_TOKENS.contains(&segment) {
                return Some(segment.to_string());
            }
        }
    }
    None
}

/// One quantified binder of an over-approximating container type.
struct ContainerBinder {
    var: String,
    binder_type: String,
    existential: bool,
}

/// Scan a Lean statement for `∀`/`forall`/`∃`/`exists` binder groups and
/// return the bound variables whose type names an over-approximating
/// container. Text-level and conservative by design (the lane's judgment and
/// the human gate sit behind it); handles the parenthesized
/// (`∀ (s s1 : Slice Std.U8) (b : Std.U8),`), braced, and bare
/// (`∃ tail : Slice Std.U8,`) binder forms, unicode and ASCII spellings, and
/// multi-line statements. The binder region ends at the first depth-0 `,`.
fn quantified_container_binders(statement: &str) -> Vec<ContainerBinder> {
    // Normalize the two unicode quantifiers into ASCII keyword tokens so a
    // single scanner handles both spellings.
    let normalized = statement.replace('∀', " forall ").replace('∃', " exists ");
    let chars: Vec<char> = normalized.chars().collect();
    let mut binders = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !is_ident_char(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_ident_char(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let existential = match word.as_str() {
            "forall" => false,
            "exists" => true,
            _ => continue,
        };
        // The binder region: from here to the first `,` at bracket depth 0.
        let mut depth = 0i32;
        let mut region = String::new();
        let mut j = i;
        while j < chars.len() {
            let c = chars[j];
            match c {
                '(' | '{' | '[' | '⟨' => depth += 1,
                ')' | '}' | ']' | '⟩' => depth -= 1,
                ',' if depth == 0 => break,
                _ => {}
            }
            region.push(c);
            j += 1;
        }
        // Split the region into binder groups: bracketed `(names : Type)` /
        // `{names : Type}` groups, else the whole region is one bare group.
        let region_chars: Vec<char> = region.chars().collect();
        let mut groups: Vec<String> = Vec::new();
        let mut k = 0;
        let mut saw_bracket_group = false;
        while k < region_chars.len() {
            let c = region_chars[k];
            if c == '(' || c == '{' {
                let close = if c == '(' { ')' } else { '}' };
                let mut inner_depth = 1;
                let mut group = String::new();
                k += 1;
                while k < region_chars.len() && inner_depth > 0 {
                    let g = region_chars[k];
                    if g == '(' || g == '{' {
                        inner_depth += 1;
                    } else if g == ')' || g == '}' {
                        inner_depth -= 1;
                        if inner_depth == 0 {
                            break;
                        }
                    }
                    let _ = close;
                    group.push(g);
                    k += 1;
                }
                groups.push(group);
                saw_bracket_group = true;
            }
            k += 1;
        }
        if !saw_bracket_group {
            groups.push(region.clone());
        }
        for group in groups {
            let Some(colon_pos) = group.find(':') else {
                continue;
            };
            let (names_part, type_part) = group.split_at(colon_pos);
            let type_part = &type_part[1..];
            if let Some(_container) = type_names_over_approximating_container(type_part) {
                for var in ident_tokens(names_part) {
                    binders.push(ContainerBinder {
                        var,
                        binder_type: type_part.trim().to_string(),
                        existential,
                    });
                }
            }
        }
    }
    binders
}

/// True when `statement` applies some Rust-validity hook (an identifier
/// starting with `RustValid`) to the variable `var`.
fn statement_hook_guards_var(statement: &str, var: &str) -> bool {
    let tokens = ident_tokens(statement);
    let mut prev_was_hook = false;
    for token in &tokens {
        if prev_was_hook && token == var {
            return true;
        }
        prev_was_hook = is_rust_validity_hook_identifier(token);
    }
    false
}

/// The Slice-3 TYPE-DRIVEN CONDITIONING + SHAPE CHECK, run fail-closed when a
/// proposed assumption is recorded (and by the engine before the lane
/// dispatch). Errors returned:
/// - unknown `claim_class` (must be `behavior` or `domain` after
///   normalization);
/// - a quantified variable of an over-approximating container type with no
///   Rust-validity hook applied to it anywhere in the statement (if no hook
///   for that type exists yet, a carrier must be minted first);
/// - a `domain` claim with no existential container binder (the realization
///   shape is `∃` a hook-valid container value).
pub fn staged_assumption_statement_errors(
    lean_statement: &str,
    claim_class: &str,
) -> Vec<String> {
    let mut errors = Vec::new();
    let class = normalize_claim_class(claim_class);
    if class != CLAIM_CLASS_BEHAVIOR && class != CLAIM_CLASS_DOMAIN {
        errors.push(format!(
            "claim_class must be `{CLAIM_CLASS_BEHAVIOR}` or `{CLAIM_CLASS_DOMAIN}`, got `{class}`"
        ));
        return errors;
    }
    let binders = quantified_container_binders(lean_statement);
    for binder in &binders {
        if !statement_hook_guards_var(lean_statement, &binder.var) {
            errors.push(format!(
                "quantified variable `{}` has over-approximating container type `{}` but no \
                 Rust-validity hook (`RustValid*`) is applied to it; condition the claim on \
                 that type's hook. A missing hook must first be authored as an ordinary \
                 validity-definition node; the assumption-authoring burst cannot create hooks",
                binder.var, binder.binder_type
            ));
        }
    }
    if class == CLAIM_CLASS_DOMAIN && !binders.iter().any(|b| b.existential) {
        errors.push(
            "a domain claim asserts language-guaranteed existence/coverage: it must contain an \
             existential binder of a container type whose witness is hook-valid"
                .to_string(),
        );
    }
    errors
}

/// The Preamble-tier Lean node where workers stage under-model `axiom` blocks.
/// `project_approved` does not rewrite Lean; it admits trust via the registry
/// sinks.
pub const ASSUMPTIONS_NODE: &str = "Assumptions";
pub const LEAN_ASSUMPTION_BEGIN_PREFIX: &str = "-- BEGIN UNDERMODEL ASSUMPTION ";
pub const LEAN_ASSUMPTION_END_PREFIX: &str = "-- END UNDERMODEL ASSUMPTION ";
pub const TEX_ASSUMPTION_BEGIN_PREFIX: &str = "% BEGIN UNDERMODEL ASSUMPTION ";
pub const TEX_ASSUMPTION_END_PREFIX: &str = "% END UNDERMODEL ASSUMPTION ";

fn proposed_path(repo_path: &Path) -> PathBuf {
    repo_path.join("PROPOSED_ASSUMPTIONS.json")
}

fn approved_axioms_path(repo_path: &Path) -> PathBuf {
    repo_path.join("APPROVED_AXIOMS.json")
}

fn tcb_manifest_path(repo_path: &Path) -> PathBuf {
    repo_path.join("tcb_manifest.json")
}

fn assumptions_lean_path(repo_path: &Path) -> PathBuf {
    repo_path
        .join("Tablet")
        .join(format!("{ASSUMPTIONS_NODE}.lean"))
}

fn assumptions_tex_path(repo_path: &Path) -> PathBuf {
    repo_path
        .join("Tablet")
        .join(format!("{ASSUMPTIONS_NODE}.tex"))
}

fn assumptions_review_path(repo_path: &Path) -> PathBuf {
    repo_path.join("ASSUMPTIONS_REVIEW.md")
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StagedAssumptionBlocks {
    pub lean_statement: String,
    pub nl_statement: String,
}

/// One assumption record in `PROPOSED_ASSUMPTIONS.json`. The worker AUTHORS the
/// Lean/NL blocks in `Tablet/Assumptions.{lean,tex}` plus axiom name + citation
/// locator + Rust justification + the `needed_by` targets; the assumptions lane
/// fills the adversarial-hunt result and flips `status` to `pending` on a pass.
/// The kernel only ever reads these to render the gate and project trust on
/// approval.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProposedAssumption {
    /// Stable id (the auditor-named candidate's slug, or the axiom name). Keyed
    /// for idempotent record / project / reject.
    pub id: String,
    /// The fully-qualified Lean `axiom` name (namespace-qualified, the form
    /// `load_approved_axioms` matches against `#print axioms` output).
    pub axiom_name: String,
    /// The verbatim staged Lean statement block of the axiom, e.g.
    /// `axiom <ns>.slice_len_le_isize_max : ∀ ...`. Rendered at the gate.
    pub lean_statement: String,
    /// The paired NL statement block staged in `Tablet/Assumptions.tex`.
    pub nl_statement: String,
    /// The resolvable citation locator into the fetched corpus (§5b): a Rust
    /// Reference section, std item path, or Nomicon section. The lane checked
    /// it RESOLVES (mechanical fabrication guard).
    pub citation_locator: String,
    /// The Rust LANGUAGE/COMPILER guarantee justification (the corpus section's
    /// content + why it guarantees `C` and maps to the Lean statement).
    pub rust_justification: String,
    /// The `Decide` targets blocked on `C` (its `needed_by` set).
    #[serde(default)]
    pub needed_by: Vec<String>,
    /// `"behavior"` or `"domain"` (Slice 3). Empty (a pre-Slice-3 record)
    /// reads as `behavior`.
    #[serde(default)]
    pub claim_class: String,
    /// The adversarial Rust hunt's result (corroboration only — "failed to
    /// refute over N runs", never "established"). Surfaced at the gate.
    pub adversarial_hunt_result: String,
    /// The lane's in-model relativization probe record (Slice 3, diagnostic
    /// only): the attempted Lean refutation of the candidate's unconditional
    /// form. Surfaced at the gate; empty when the lane did not run one.
    #[serde(default)]
    pub relativization_probe: String,
    /// `"pending"` while proposed; `"rejected"` after a human declines. An
    /// `"approved"` record is REMOVED from this file (it now lives in the three
    /// sinks), so a lingering `"approved"` here would be a projection bug.
    pub status: String,
    /// The reviewer/operator reason recorded on a rejection.
    #[serde(default)]
    pub rejected_reason: String,
}

/// The on-disk `PROPOSED_ASSUMPTIONS.json` shape: a flat list of records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProposedAssumptions {
    pub assumptions: Vec<ProposedAssumption>,
}

impl ProposedAssumptions {
    pub fn pending(&self) -> impl Iterator<Item = &ProposedAssumption> {
        self.assumptions.iter().filter(|a| a.status == "pending")
    }

    pub fn has_pending(&self) -> bool {
        self.pending().next().is_some()
    }
}

/// Provisional-admission axiom set: the `axiom_name`s of every lane-passed
/// `status:"pending"` entry in `PROPOSED_ASSUMPTIONS.json`.
///
/// Design (2026-07-02, operator decision on the dec2flt example): an assumption
/// the adversarial assumptions lane has PASSED — citation resolves in the
/// pinned corpus, language/compiler-guarantee eligibility holds, the Rust
/// hunt failed to refute — is admitted PROVISIONALLY for closure
/// verification, instead of quarantining every dependent proof as
/// "closure-unverified" for the (potentially many) cycles until the
/// end-of-TheoremStating AssumptionReview gate. The human gate remains the
/// permanent authority: APPROVE moves the entry into `APPROVED_AXIOMS.json`
/// (the only permanent sink, still written exclusively by
/// `project_approved`); REJECT flips the entry to `status:"rejected"`, which
/// drops it from this set — the effective-allowlist hash then changes and
/// `rescind_records_with_stale_approved_axioms_hash` automatically rescinds
/// every closure record blessed under it, reverting dependents to
/// unverified. Nothing crosses the phase boundary on lane authority alone:
/// the gate stays LIVE while any entry is pending.
///
/// Fail-closed: a missing file admits nothing; a parse error is surfaced as
/// `Err` by `load_proposed` and must propagate (an unreadable registry must
/// never widen the allowlist).
pub fn lane_passed_pending_axioms(
    repo_path: &Path,
) -> Result<std::collections::BTreeSet<String>, String> {
    let store = load_proposed(repo_path)?;
    Ok(store
        .pending()
        .map(|a| a.axiom_name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect())
}

/// Read `PROPOSED_ASSUMPTIONS.json`. Absent file → empty (no pending). A parse
/// error is an explicit `Err` (fail loud, never silently "no pending" — that
/// would skip the human gate).
pub fn load_proposed(repo_path: &Path) -> Result<ProposedAssumptions, String> {
    let path = proposed_path(repo_path);
    if !path.exists() {
        return Ok(ProposedAssumptions::default());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("Failed to read {}: {err}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(ProposedAssumptions::default());
    }
    serde_json::from_str(&raw)
        .map_err(|err| format!("Failed to parse {}: {err}", path.display()))
}

fn write_proposed(repo_path: &Path, value: &ProposedAssumptions) -> Result<(), String> {
    let path = proposed_path(repo_path);
    let body = serde_json::to_string_pretty(value)
        .map_err(|err| format!("Failed to serialize proposed assumptions: {err}"))?;
    fs::write(&path, body + "\n")
        .map_err(|err| format!("Failed to write {}: {err}", path.display()))
}

/// True iff `PROPOSED_ASSUMPTIONS.json` has ≥1 `status:"pending"` entry. Used by
/// the engine (via the runtime CLI) to suppress the mode-B vacuous AdvancePhase
/// auto-advance, taking the human `AssumptionReview` gate LIVE.
pub fn has_pending(repo_path: &Path) -> Result<bool, String> {
    Ok(load_proposed(repo_path)?.has_pending())
}

/// Count pending records in `PROPOSED_ASSUMPTIONS.json`. This is the durable
/// source of truth used to rehydrate the engine's in-memory routing counter on
/// runtime load.
pub fn pending_count(repo_path: &Path) -> Result<usize, String> {
    Ok(load_proposed(repo_path)?.pending().count())
}

/// Append a worker-authored, lane-gated assumption as `status:"pending"`
/// (idempotent on `id`: a re-record replaces the prior entry). Honors the
/// engine's `RecordProposedAssumption` command (assumptions-lane PASS).
pub fn record_proposed(
    repo_path: &Path,
    mut record: ProposedAssumption,
) -> Result<(), String> {
    ensure_staged_matches_record(repo_path, &record)?;
    record.claim_class = normalize_claim_class(&record.claim_class);
    // Slice 3 fail-closed backstop: the engine runs this check before the
    // lane dispatch, but the registry is the last writer before a statement
    // becomes provisionally usable, so it re-runs it.
    let statement_errors =
        staged_assumption_statement_errors(&record.lean_statement, &record.claim_class);
    if !statement_errors.is_empty() {
        return Err(format!(
            "record_proposed: staged assumption id={} fails the conditioning/shape check: {}",
            record.id,
            statement_errors.join("; ")
        ));
    }
    record.status = "pending".to_string();
    let mut store = load_proposed(repo_path)?;
    store.assumptions.retain(|a| a.id != record.id);
    store.assumptions.push(record);
    write_proposed(repo_path, &store)
}

/// Mark a pending assumption `rejected` (human declined at the gate). Honors the
/// engine's reject path; the parked `Decide` target is routed engine-side.
pub fn reject(repo_path: &Path, id: &str, reason: &str) -> Result<(), String> {
    let mut store = load_proposed(repo_path)?;
    let mut found = false;
    for a in &mut store.assumptions {
        if a.id == id {
            remove_staged_assumption_blocks(repo_path, id)?;
            a.status = "rejected".to_string();
            a.rejected_reason = reason.to_string();
            found = true;
        }
    }
    if !found {
        return Err(format!(
            "reject: no PROPOSED_ASSUMPTIONS.json entry with id={id}"
        ));
    }
    write_proposed(repo_path, &store)
}

/// Project EVERY pending assumption into the three sinks (human approved the
/// batch at the gate). Honors the engine's `ProjectAllPendingAssumptions`
/// command (the engine carries only the count, so the ids are read from disk
/// here). Returns the number projected.
pub fn project_all_pending(repo_path: &Path) -> Result<usize, String> {
    let ids: Vec<String> = load_proposed(repo_path)?
        .pending()
        .map(|a| a.id.clone())
        .collect();
    let n = ids.len();
    for id in ids {
        project_approved(repo_path, &id)?;
    }
    Ok(n)
}

/// Reject EVERY pending assumption (human declined the batch). Honors the
/// engine's `RejectAllPendingAssumptions` command. Returns the number rejected.
pub fn reject_all_pending(repo_path: &Path, reason: &str) -> Result<usize, String> {
    let ids: Vec<String> = load_proposed(repo_path)?
        .pending()
        .map(|a| a.id.clone())
        .collect();
    let n = ids.len();
    for id in ids {
        reject(repo_path, &id, reason)?;
    }
    Ok(n)
}

// ─────────────────────────── the three-sink projection ─────────────────────

/// Project an approved assumption `id` into the trust sinks idempotently in
/// sequence. This is not a multi-file filesystem transaction: a write failure
/// can leave a prefix of the projection on disk, but it fails loud while the
/// pending record remains available for the same recorded decision to finish.
/// The runtime starts this sequence only after the human decision's checkpoint,
/// state, and event-log durability barrier. On success the record is REMOVED
/// from `PROPOSED_ASSUMPTIONS.json` (it now lives in the sinks). Honors the
/// engine's `ProjectApprovedAssumption` command (human-gate APPROVE).
///
/// The staged Lean/NL blocks are already authoritative. Approval checks they
/// still match the pending record, writes the disclosure manifest, and only
/// then admits the axiom permanently through `APPROVED_AXIOMS.json`. Before
/// that it may be admitted provisionally by `lane_passed_pending_axioms`; the
/// approved-axioms write is the permanent human-ratified trust switch, so it
/// intentionally happens last.
pub fn project_approved(repo_path: &Path, id: &str) -> Result<(), String> {
    let store = load_proposed(repo_path)?;
    let record = store
        .assumptions
        .iter()
        .find(|a| a.id == id && a.status == "pending")
        .cloned()
        .ok_or_else(|| {
            format!("project_approved: no PENDING PROPOSED_ASSUMPTIONS.json entry with id={id}")
        })?;

    ensure_staged_matches_record(repo_path, &record)?;
    project_into_tcb_manifest(repo_path, &record)?;
    project_into_approved_axioms(repo_path, &record.axiom_name)?;

    // Remove the now-projected record from the proposed file.
    let mut store = load_proposed(repo_path)?;
    store.assumptions.retain(|a| a.id != id);
    write_proposed(repo_path, &store)
}

/// Sink 1: add `axiom_name` to `APPROVED_AXIOMS.json` `global` (the
/// `{global, nodes}` object form `load_approved_axioms` consumes). Creates the
/// file in object form if absent; upgrades a bare-array file to object form;
/// idempotent (no duplicate). This is the permanent human-ratified admission;
/// a pending proposal may already be usable through the separate, revocable
/// provisional allowlist.
fn project_into_approved_axioms(repo_path: &Path, axiom_name: &str) -> Result<(), String> {
    let path = approved_axioms_path(repo_path);
    let mut value: serde_json::Value = if path.exists() {
        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read {}: {err}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|err| format!("Failed to parse {}: {err}", path.display()))?
    } else {
        serde_json::json!({ "global": [], "nodes": {} })
    };

    // Normalize a bare-array file into the {global, nodes} object form so the
    // projection has a `global` list to extend (load_approved_axioms accepts
    // both, but the seed + projection canonicalize to the object form).
    if let serde_json::Value::Array(items) = &value {
        value = serde_json::json!({ "global": items.clone(), "nodes": {} });
    }
    let obj = value
        .as_object_mut()
        .ok_or_else(|| format!("{} is neither a JSON array nor object", path.display()))?;
    let global = obj
        .entry("global".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let arr = global
        .as_array_mut()
        .ok_or_else(|| format!("{} `global` is not a JSON array", path.display()))?;
    let already = arr
        .iter()
        .any(|v| v.as_str().map(str::trim) == Some(axiom_name.trim()));
    if !already {
        arr.push(serde_json::Value::String(axiom_name.trim().to_string()));
    }
    if !obj.contains_key("nodes") {
        obj.insert("nodes".to_string(), serde_json::json!({}));
    }
    let body = serde_json::to_string_pretty(&value)
        .map_err(|err| format!("Failed to serialize {}: {err}", path.display()))?;
    fs::write(&path, body + "\n")
        .map_err(|err| format!("Failed to write {}: {err}", path.display()))
}

/// Sink 2: append a `tcb_disclosure` entry to `tcb_manifest.json` with
/// `classification: "rust-undermodel-assumption"` + justification + locator +
/// provenance. Idempotent on `axiom_name`. The manifest's `global`/`nodes` siblings
/// are left to sink 1 (`APPROVED_AXIOMS.json` is the loader's source); the
/// disclosure is the operator-readable TCB rationale.
fn project_into_tcb_manifest(
    repo_path: &Path,
    record: &ProposedAssumption,
) -> Result<(), String> {
    let path = tcb_manifest_path(repo_path);
    let mut value: serde_json::Value = if path.exists() {
        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read {}: {err}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|err| format!("Failed to parse {}: {err}", path.display()))?
    } else {
        serde_json::json!({
            "schema": "pv-tcb-disclosure/v1",
            "tcb_disclosure": [],
        })
    };
    let obj = value
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
    let disclosure = obj
        .entry("tcb_disclosure".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let arr = disclosure
        .as_array_mut()
        .ok_or_else(|| format!("{} `tcb_disclosure` is not a JSON array", path.display()))?;
    let already = arr.iter().any(|v| {
        v.get("name").and_then(serde_json::Value::as_str).map(str::trim)
            == Some(record.axiom_name.trim())
    });
    if !already {
        arr.push(serde_json::json!({
            "id": record.id.trim(),
            "name": record.axiom_name.trim(),
            "classification": UNDER_MODEL_CLASSIFICATION,
            "claim_class": normalize_claim_class(&record.claim_class),
            "lean_statement": record.lean_statement.trim(),
            "nl_statement": record.nl_statement.trim(),
            "citation_locator": record.citation_locator.trim(),
            "rust_justification": record.rust_justification.trim(),
            "needed_by": record.needed_by,
            "adversarial_hunt_result": record.adversarial_hunt_result.trim(),
            "relativization_probe": record.relativization_probe.trim(),
            "provenance": "worker-authored, assumptions-lane gated, human-ratified \
                           (PV under-model assumptions Slice 2)",
        }));
    }
    let body = serde_json::to_string_pretty(&value)
        .map_err(|err| format!("Failed to serialize {}: {err}", path.display()))?;
    fs::write(&path, body + "\n")
        .map_err(|err| format!("Failed to write {}: {err}", path.display()))
}

/// Freeze the independently-authorized assumption records cited by a
/// conditional proposal. Both the permanent TCB disclosure and the current
/// approved-axiom projection must agree; pending/rejected/missing ids fail
/// closed and cannot be promoted by a proposal.
pub fn conditional_assumption_snapshots(
    repo_path: &Path,
    ids: &std::collections::BTreeSet<String>,
) -> Result<Vec<crate::trust_base::ConditionalAssumptionSnapshot>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let proposed = load_proposed(repo_path)?;
    let tcb_path = tcb_manifest_path(repo_path);
    let tcb: serde_json::Value = if tcb_path.exists() {
        serde_json::from_str(
            &fs::read_to_string(&tcb_path)
                .map_err(|error| format!("Failed to read {}: {error}", tcb_path.display()))?,
        )
        .map_err(|error| format!("Failed to parse {}: {error}", tcb_path.display()))?
    } else {
        serde_json::json!({"tcb_disclosure": []})
    };
    let disclosures = tcb
        .get("tcb_disclosure")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "tcb_manifest.json tcb_disclosure is not an array".to_owned())?;
    let approved_path = approved_axioms_path(repo_path);
    let approved_value: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&approved_path)
            .map_err(|error| format!("Failed to read {}: {error}", approved_path.display()))?,
    )
    .map_err(|error| format!("Failed to parse {}: {error}", approved_path.display()))?;
    let approved_global: std::collections::BTreeSet<&str> = match &approved_value {
        serde_json::Value::Array(items) => items.iter(),
        serde_json::Value::Object(object) => object
            .get("global")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "APPROVED_AXIOMS.json global is not an array".to_owned())?
            .iter(),
        _ => return Err("APPROVED_AXIOMS.json is neither an array nor object".into()),
    }
    .filter_map(serde_json::Value::as_str)
    .collect();
    let mut out = Vec::new();
    for id in ids {
        if proposed
            .assumptions
            .iter()
            .any(|record| record.id == *id && record.status != "approved")
        {
            return Err(format!(
                "conditional proposal cannot use non-approved assumption id `{id}`"
            ));
        }
        let disclosure = disclosures.iter().find(|row| {
            row.get("id").and_then(serde_json::Value::as_str) == Some(id.as_str())
                && row.get("classification").and_then(serde_json::Value::as_str)
                    == Some(UNDER_MODEL_CLASSIFICATION)
        });
        let Some(disclosure) = disclosure else {
            return Err(format!(
                "conditional proposal cites assumption id `{id}` without a pending or approved record"
            ));
        };
        let axiom_name = disclosure
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| format!("approved assumption `{id}` has no axiom name"))?;
        if !approved_global.contains(axiom_name) {
            return Err(format!(
                "conditional proposal cites assumption `{id}` whose axiom is not currently approved"
            ));
        }
        out.push(crate::trust_base::ConditionalAssumptionSnapshot {
            id: id.clone(),
            axiom_name: axiom_name.to_owned(),
            status: "approved".into(),
            record: disclosure.clone(),
            record_sha256: crate::trust_base::raw_sha256(
                &crate::trust_base::canonical_json(disclosure)
                    .map_err(|error| error.to_string())?,
            ),
        });
    }
    Ok(out)
}

// ───────────────────── extraction provenance (manifest) ─────────────────────

/// One deduplicated `{source_file, source_sha256}` extraction-source pair from
/// the config's `pv_tablet.extraction_models[].provenance` — the byte-pin of
/// the Rust source the models were extracted from.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractionSourceDigest {
    pub source_file: String,
    pub source_sha256: String,
}

/// One extraction fact the producing pipeline could not capture.  A missing
/// value is never represented by an empty string in the manifest: the field
/// name and an operator-readable reason travel together here instead.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionNotRecorded {
    pub field: String,
    pub reason: String,
}

/// Repo-relative path of the pinned docs-corpus manifest (the fetched Rust
/// documentation corpus the assumptions lane resolves citations against).
/// When present, its pinned source revisions are embedded into the manifest's
/// `extraction_provenance.docs_corpus`; when absent, `docs_corpus_revision`
/// is listed in `not_recorded` instead.
pub const DOCS_CORPUS_MANIFEST_REL: &str = "rust_docs_corpus/CORPUS_MANIFEST.json";

/// Complete extraction environment surface expected in new PV seeds.  Legacy
/// configs remain loadable, but every absent member is rendered as an honest
/// negative with a reason.
const EXTRACTION_TOOLCHAIN_EVIDENCE_FIELDS: [&str; 13] = [
    "charon",
    "charon_revision",
    "aeneas",
    "aeneas_revision",
    "lean",
    "rustc_vv",
    "target_triple",
    "target_pointer_width",
    "cargo_features",
    "cargo_lock_sha256",
    "extraction_profile",
    "overflow_checks",
    "panic_strategy",
];

/// Canonical fingerprint used by both per-target provenance and the manifest.
/// Keep in lockstep with `scripts/extract_pv_model.py`.
pub fn extraction_toolchain_sha256(
    extractor_stack: &[String],
    extraction_toolchain: &serde_json::Value,
) -> String {
    let mut sorted_stack = extractor_stack.to_vec();
    sorted_stack.sort();
    let canonical = serde_json::json!({
        "extraction_toolchain": extraction_toolchain,
        "extractor_stack": sorted_stack,
    });
    let serialized = serde_json::to_string(&canonical).unwrap_or_else(|_| String::from("{}"));
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// (Re)write the `extraction_provenance` section of `tcb_manifest.json`:
/// the config's `extraction_toolchain` map (verbatim), the `extractor_stack`,
/// the DISTINCT `{source_file, source_sha256}` pairs from the extraction
/// models' provenance, the pinned docs-corpus revisions (embedded from
/// `rust_docs_corpus/CORPUS_MANIFEST.json` when present), and an honest
/// `not_recorded` list, with reasons, for any requested extraction fact the
/// run could not capture (and for the docs-corpus revision when its manifest
/// is absent or invalid).
///
/// Schema note: this is ADDITIVE under `pv-tcb-disclosure/v1`. No consumer
/// branches on the `schema` field (`setup_pv_repo.sh` reads
/// `namespace`/`global`/`tcb_disclosure`; the closure gate consumes
/// `APPROVED_AXIOMS.json`, not this file; the audit bundle and viewer treat
/// the manifest as opaque content), and every reader ignores unknown object
/// keys — so adding `extraction_provenance` does not warrant a version bump.
///
/// Idempotent: all other manifest keys are preserved read-modify-write, and
/// the file is rewritten only when the computed section differs from what is
/// already on disk. Returns `true` iff the manifest was (re)written.
pub fn record_extraction_provenance(
    repo_path: &Path,
    extraction_toolchain: &serde_json::Value,
    extractor_stack: &[String],
    source_digests: &[ExtractionSourceDigest],
    extraction_not_recorded: &[ExtractionNotRecorded],
) -> Result<bool, String> {
    let mut not_recorded = std::collections::BTreeMap::<String, String>::new();
    for entry in extraction_not_recorded {
        let field = entry.field.trim();
        let reason = entry.reason.trim();
        if field.is_empty() || reason.is_empty() {
            return Err(
                "extraction_not_recorded entries require nonempty field and reason".to_string(),
            );
        }
        if not_recorded
            .insert(field.to_string(), reason.to_string())
            .is_some()
        {
            return Err(format!(
                "extraction_not_recorded repeats field {field:?}"
            ));
        }
    }

    // Empty strings/nulls look like answers but carry no evidence.  Remove
    // them from the displayed toolchain and disclose the gap explicitly.
    let mut recorded_toolchain = if extraction_toolchain.is_object() {
        extraction_toolchain.clone()
    } else {
        not_recorded.insert(
            "extraction_toolchain".to_string(),
            "pv_tablet.extraction_toolchain is not an object".to_string(),
        );
        serde_json::json!({})
    };
    if let Some(object) = recorded_toolchain.as_object_mut() {
        object.retain(|field, value| {
            let recorded = !value.is_null()
                && !value.as_str().is_some_and(|value| value.trim().is_empty());
            if !recorded {
                not_recorded.entry(field.clone()).or_insert_with(|| {
                    "pv_tablet.extraction_toolchain carried an empty value".to_string()
                });
            }
            recorded
        });
    }
    for field in EXTRACTION_TOOLCHAIN_EVIDENCE_FIELDS {
        if !recorded_toolchain
            .as_object()
            .is_some_and(|object| object.contains_key(field))
        {
            not_recorded.entry(field.to_string()).or_insert_with(|| {
                "pv_tablet.extraction_toolchain does not carry this field".to_string()
            });
        }
    }
    if !recorded_toolchain
        .as_object()
        .and_then(|object| object.get("cfg"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|values| !values.is_empty())
    {
        not_recorded.entry("cfg".to_string()).or_insert_with(|| {
            "pv_tablet.extraction_toolchain does not carry effective cfg values".to_string()
        });
    }
    if extractor_stack.is_empty() {
        not_recorded
            .entry("extractor_stack".to_string())
            .or_insert_with(|| "pv_tablet.extractor_stack is empty".to_string());
    }

    // Dedupe + sort the complete source pairs (a whole-crate extraction stamps
    // the same pair on every model node). Incomplete pairs are disclosed, not
    // serialized with an empty string.
    if source_digests.is_empty() {
        not_recorded
            .entry("source_digests".to_string())
            .or_insert_with(|| "no extraction model provenance was supplied".to_string());
    }
    for digest in source_digests {
        if digest.source_file.trim().is_empty() {
            not_recorded
                .entry("source_file".to_string())
                .or_insert_with(|| "an extraction model omits source_file".to_string());
        }
        if digest.source_sha256.trim().is_empty() {
            not_recorded
                .entry("source_sha256".to_string())
                .or_insert_with(|| "an extraction model omits source_sha256".to_string());
        }
    }
    let digests: std::collections::BTreeSet<&ExtractionSourceDigest> = source_digests
        .iter()
        .filter(|digest| {
            !digest.source_file.trim().is_empty() && !digest.source_sha256.trim().is_empty()
        })
        .collect();
    let digests: Vec<serde_json::Value> = digests
        .into_iter()
        .map(|d| {
            serde_json::json!({
                "source_file": d.source_file.trim(),
                "source_sha256": d.source_sha256.trim(),
            })
        })
        .collect();

    // Docs corpus: embed the pinned source revisions when the repo carries the
    // corpus manifest (the zip does not include the corpus itself, so the pins
    // must ride the manifest); an absent/unreadable corpus manifest is
    // DISCLOSED as not recorded rather than failing init or implying a pin.
    let corpus_path = repo_path.join(DOCS_CORPUS_MANIFEST_REL);
    let docs_corpus: Option<serde_json::Value> = match fs::read_to_string(&corpus_path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value)
                if value.get("schema").is_some_and(serde_json::Value::is_string)
                    && value.get("sources").is_some_and(serde_json::Value::is_array) =>
            {
                Some(serde_json::json!({
                    "manifest_path": DOCS_CORPUS_MANIFEST_REL,
                    "schema": value["schema"],
                    "sources": value["sources"],
                }))
            }
            Ok(_) => {
                not_recorded.insert(
                    "docs_corpus_revision".to_string(),
                    "rust_docs_corpus/CORPUS_MANIFEST.json lacks string schema or array sources"
                        .to_string(),
                );
                None
            }
            Err(_) => {
                not_recorded.insert(
                    "docs_corpus_revision".to_string(),
                    "rust_docs_corpus/CORPUS_MANIFEST.json is not valid JSON".to_string(),
                );
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            not_recorded.insert(
                "docs_corpus_revision".to_string(),
                "rust_docs_corpus/CORPUS_MANIFEST.json is absent".to_string(),
            );
            None
        }
        Err(_) => {
            not_recorded.insert(
                "docs_corpus_revision".to_string(),
                "rust_docs_corpus/CORPUS_MANIFEST.json could not be read".to_string(),
            );
            None
        }
    };
    if docs_corpus.is_none() {
        not_recorded
            .entry("docs_corpus_revision".to_string())
            .or_insert_with(|| "docs corpus revision was not captured".to_string());
    }

    let not_recorded: Vec<serde_json::Value> = not_recorded
        .into_iter()
        .map(|(field, reason)| serde_json::json!({"field": field, "reason": reason}))
        .collect();

    let mut section = serde_json::json!({
        "extraction_toolchain": recorded_toolchain,
        "extractor_stack": extractor_stack,
        "extractor_toolchain_sha256": extraction_toolchain_sha256(
            extractor_stack,
            &recorded_toolchain,
        ),
        "source_digests": digests,
        "not_recorded": not_recorded,
    });
    if let Some(corpus) = docs_corpus {
        section
            .as_object_mut()
            .expect("section is an object")
            .insert("docs_corpus".to_string(), corpus);
    }

    let path = tcb_manifest_path(repo_path);
    let mut value: serde_json::Value = if path.exists() {
        let raw = fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read {}: {err}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|err| format!("Failed to parse {}: {err}", path.display()))?
    } else {
        serde_json::json!({
            "schema": "pv-tcb-disclosure/v1",
            "tcb_disclosure": [],
        })
    };
    let obj = value
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
    if path.exists() && obj.get("extraction_provenance") == Some(&section) {
        return Ok(false);
    }
    obj.insert("extraction_provenance".to_string(), section);
    let body = serde_json::to_string_pretty(&value)
        .map_err(|err| format!("Failed to serialize {}: {err}", path.display()))?;
    fs::write(&path, body + "\n")
        .map_err(|err| format!("Failed to write {}: {err}", path.display()))?;
    Ok(true)
}

/// The empty `Tablet/Assumptions.lean` fallback used when runtime initialization
/// finds no extractor-created scaffold. `import Tablet.Preamble` lets later
/// worker-authored assumptions reference model types. The fallback itself
/// declares nothing.
pub fn empty_assumptions_lean_scaffold() -> String {
    "import Tablet.Preamble\n\
         open Aeneas Aeneas.Std Result ControlFlow Error\n\n\
         -- [PV UNDER-MODEL ASSUMPTIONS]\n\
         -- Worker-authored staged assumptions live between markers:\n\
         --   -- BEGIN UNDERMODEL ASSUMPTION <id>\n\
         --   axiom <name> : <type>\n\
         --   -- END UNDERMODEL ASSUMPTION <id>\n"
        .to_string()
}

/// The empty paired NL scaffold for `Tablet/Assumptions.tex`.
pub fn empty_assumptions_tex_scaffold() -> String {
    "% [PV UNDER-MODEL ASSUMPTIONS]\n\
     % Worker-authored staged assumptions live between markers:\n\
     %   % BEGIN UNDERMODEL ASSUMPTION <id>\n\
     %   \\begin{definition}\n\
     %   ...\n\
     %   \\end{definition}\n\
     %   % END UNDERMODEL ASSUMPTION <id>\n"
        .to_string()
}

fn marker(prefix: &str, id: &str) -> String {
    format!("{prefix}{}", id.trim())
}

fn validate_assumption_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("staged assumption id must be non-empty".to_string());
    }
    if id
        .chars()
        .any(|ch| !(ch == '_' || ch == '-' || ch == '.' || ch.is_ascii_alphanumeric()))
    {
        return Err(format!(
            "staged assumption id `{id}` may contain only ASCII letters, digits, `_`, `-`, and `.`"
        ));
    }
    Ok(id.to_string())
}

fn staged_begin_marker_id(line: &str, begin_prefix: &str) -> Option<String> {
    let id = line.trim().strip_prefix(begin_prefix)?.trim();
    validate_assumption_id(id).ok()
}

fn text_has_worker_authored_staged_begin_marker(text: &str, begin_prefix: &str) -> bool {
    text.lines()
        .any(|line| staged_begin_marker_id(line, begin_prefix).is_some())
}

/// True when `Tablet/Assumptions.{lean,tex}` contains a real worker-authored
/// staged-assumption begin marker. The empty scaffold's template examples are
/// intentionally ignored: after trimming, those lines still start with the
/// outer comment marker (`--   -- ...` / `%   % ...`), not the real marker
/// prefix, and `<id>` is rejected by the assumption-id validator.
pub fn has_worker_authored_staged_assumption(repo_path: &Path) -> Result<bool, String> {
    let lean_path = assumptions_lean_path(repo_path);
    let tex_path = assumptions_tex_path(repo_path);
    let lean_text = if lean_path.exists() {
        fs::read_to_string(&lean_path)
            .map_err(|err| format!("Failed to read {}: {err}", lean_path.display()))?
    } else {
        String::new()
    };
    let tex_text = if tex_path.exists() {
        fs::read_to_string(&tex_path)
            .map_err(|err| format!("Failed to read {}: {err}", tex_path.display()))?
    } else {
        String::new()
    };
    Ok(text_has_worker_authored_staged_begin_marker(
        &lean_text,
        LEAN_ASSUMPTION_BEGIN_PREFIX,
    ) || text_has_worker_authored_staged_begin_marker(
        &tex_text,
        TEX_ASSUMPTION_BEGIN_PREFIX,
    ))
}

fn extract_marked_block(
    text: &str,
    begin: &str,
    end: &str,
    path: &Path,
) -> Result<String, String> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut found = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == begin {
            if in_block || found {
                return Err(format!(
                    "{} contains duplicate begin marker `{begin}`",
                    path.display()
                ));
            }
            in_block = true;
            found = true;
            continue;
        }
        if trimmed == end {
            if !in_block {
                return Err(format!(
                    "{} contains end marker `{end}` before its begin marker",
                    path.display()
                ));
            }
            in_block = false;
            continue;
        }
        if in_block {
            out.push(line);
        }
    }
    if in_block {
        return Err(format!(
            "{} contains begin marker `{begin}` without end marker `{end}`",
            path.display()
        ));
    }
    if !found {
        return Err(format!(
            "{} contains no staged assumption block marked `{begin}` / `{end}`",
            path.display()
        ));
    }
    let block = out.join("\n").trim().to_string();
    if block.is_empty() {
        return Err(format!(
            "{} staged assumption block `{begin}` is empty",
            path.display()
        ));
    }
    Ok(block)
}

fn remove_marked_block(text: &str, begin: &str, end: &str, path: &Path) -> Result<String, String> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut removed = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == begin {
            if in_block {
                return Err(format!(
                    "{} contains nested staged assumption marker `{begin}`",
                    path.display()
                ));
            }
            in_block = true;
            removed = true;
            continue;
        }
        if trimmed == end {
            if !in_block {
                return Err(format!(
                    "{} contains end marker `{end}` before its begin marker",
                    path.display()
                ));
            }
            in_block = false;
            continue;
        }
        if !in_block {
            out.push(line);
        }
    }
    if in_block {
        return Err(format!(
            "{} contains begin marker `{begin}` without end marker `{end}`",
            path.display()
        ));
    }
    if !removed {
        return Ok(text.to_string());
    }
    let mut result = out.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

pub fn extract_staged_assumption_blocks(
    repo_path: &Path,
    id: &str,
) -> Result<StagedAssumptionBlocks, String> {
    let id = validate_assumption_id(id)?;
    let lean_path = assumptions_lean_path(repo_path);
    let tex_path = assumptions_tex_path(repo_path);
    let lean_text = fs::read_to_string(&lean_path)
        .map_err(|err| format!("Failed to read {}: {err}", lean_path.display()))?;
    let tex_text = fs::read_to_string(&tex_path)
        .map_err(|err| format!("Failed to read {}: {err}", tex_path.display()))?;
    Ok(StagedAssumptionBlocks {
        lean_statement: extract_marked_block(
            &lean_text,
            &marker(LEAN_ASSUMPTION_BEGIN_PREFIX, &id),
            &marker(LEAN_ASSUMPTION_END_PREFIX, &id),
            &lean_path,
        )?,
        nl_statement: extract_marked_block(
            &tex_text,
            &marker(TEX_ASSUMPTION_BEGIN_PREFIX, &id),
            &marker(TEX_ASSUMPTION_END_PREFIX, &id),
            &tex_path,
        )?,
    })
}

pub fn remove_staged_assumption_blocks(repo_path: &Path, id: &str) -> Result<(), String> {
    let id = validate_assumption_id(id)?;
    let lean_path = assumptions_lean_path(repo_path);
    let tex_path = assumptions_tex_path(repo_path);
    if lean_path.exists() {
        let text = fs::read_to_string(&lean_path)
            .map_err(|err| format!("Failed to read {}: {err}", lean_path.display()))?;
        let next = remove_marked_block(
            &text,
            &marker(LEAN_ASSUMPTION_BEGIN_PREFIX, &id),
            &marker(LEAN_ASSUMPTION_END_PREFIX, &id),
            &lean_path,
        )?;
        if next != text {
            fs::write(&lean_path, next)
                .map_err(|err| format!("Failed to write {}: {err}", lean_path.display()))?;
        }
    }
    if tex_path.exists() {
        let text = fs::read_to_string(&tex_path)
            .map_err(|err| format!("Failed to read {}: {err}", tex_path.display()))?;
        let next = remove_marked_block(
            &text,
            &marker(TEX_ASSUMPTION_BEGIN_PREFIX, &id),
            &marker(TEX_ASSUMPTION_END_PREFIX, &id),
            &tex_path,
        )?;
        if next != text {
            fs::write(&tex_path, next)
                .map_err(|err| format!("Failed to write {}: {err}", tex_path.display()))?;
        }
    }
    Ok(())
}

fn ensure_staged_matches_record(repo_path: &Path, record: &ProposedAssumption) -> Result<(), String> {
    let staged = extract_staged_assumption_blocks(repo_path, &record.id)?;
    if staged.lean_statement.trim() != record.lean_statement.trim() {
        return Err(format!(
            "staged Lean block for assumption id={} no longer matches PROPOSED_ASSUMPTIONS.json",
            record.id
        ));
    }
    if staged.nl_statement.trim() != record.nl_statement.trim() {
        return Err(format!(
            "staged NL block for assumption id={} no longer matches PROPOSED_ASSUMPTIONS.json",
            record.id
        ));
    }
    Ok(())
}

/// Render `ASSUMPTIONS_REVIEW.md` (next to `HUMAN_INPUT.md`/`INPUT_REQUEST.md`)
/// showing, per PENDING assumption: the verbatim Lean statement + axiom name,
/// the citation locator, the Rust justification, the `needed_by` targets, and
/// the adversarial-hunt result. The operator reads this at the `AssumptionReview`
/// gate. Honors the engine's `RenderAssumptionsReview` command.
pub fn render_review(repo_path: &Path) -> Result<(), String> {
    let store = load_proposed(repo_path)?;
    let mut md = String::new();
    md.push_str("# Assumptions Review\n\n");
    md.push_str(
        "The supervisor reached the AssumptionReview human gate with >=1 pending Rust \
         under-model assumption. Each entry below is a Rust LANGUAGE/COMPILER guarantee a \
         worker authored, the assumptions lane gated (citation resolves + corpus read + \
         adversarial Rust hunt), and now requires your ratification into the disclosed TCB.\n\n",
    );
    md.push_str(
        "Consistency basis: the ratified set is jointly consistent because it is jointly \
         TRUE of Rust under one intended interpretation -- each validity hook reads as its \
         type's language-admissibility predicate, each opaque boundary primitive as its \
         documented behavior on admissible values; Rust itself is the model. Certification \
         per axiom = citation + hunt + this gate.\n\n",
    );
    md.push_str(
        "Eligible assumptions are language/compiler guarantees true for ALL valid Rust \
         independent of the program (slice/alloc <= isize::MAX, refs non-null, valid \
         discriminants, str UTF-8, layout/niche), in two classes. `behavior`: \
         hook-conditioned facts about operations on valid values (refutation-shaped hunt). \
         `domain`: language-guaranteed existence/coverage of valid values \
         (construction-shaped hunt; corroboration is SIZE-BOUNDED -- it cannot reach \
         astronomically sized witnesses, so truth rests on the citation plus this \
         ratification). A `domain` claim makes the assumption set BINDING: the empty \
         reading of the hook no longer satisfies it. An `unsafe`-precondition or \
         program-maintained invariant is NOT eligible -- reject it.\n\n",
    );
    let pending: Vec<&ProposedAssumption> = store.pending().collect();
    if pending.is_empty() {
        md.push_str("_No pending assumptions._\n");
    }
    for (i, a) in pending.iter().enumerate() {
        md.push_str(&format!("## {}. `{}`\n\n", i + 1, a.axiom_name.trim()));
        let class = normalize_claim_class(&a.claim_class);
        md.push_str(&format!("**Claim class**: `{class}`"));
        if class == CLAIM_CLASS_DOMAIN {
            md.push_str(
                " -- makes the assumption set BINDING; ratifying it decides that the hook's \
                 extension is inhabited as claimed",
            );
        }
        md.push_str("\n\n");
        md.push_str("### NL statement (verbatim)\n\n```tex\n");
        md.push_str(a.nl_statement.trim());
        md.push_str("\n```\n\n");
        md.push_str("### Lean statement (verbatim)\n\n```lean\n");
        md.push_str(a.lean_statement.trim());
        md.push_str("\n```\n\n");
        md.push_str(&format!(
            "### Citation locator\n\n{}\n\n",
            a.citation_locator.trim()
        ));
        md.push_str(&format!(
            "### Rust justification\n\n{}\n\n",
            a.rust_justification.trim()
        ));
        md.push_str("### Needed by (blocked targets)\n\n");
        if a.needed_by.is_empty() {
            md.push_str("_none recorded_\n\n");
        } else {
            for t in &a.needed_by {
                md.push_str(&format!("- {}\n", t.trim()));
            }
            md.push('\n');
        }
        md.push_str(&format!(
            "### Adversarial Rust hunt (corroboration only)\n\n{}\n\n",
            a.adversarial_hunt_result.trim()
        ));
        if !a.relativization_probe.trim().is_empty() {
            md.push_str(&format!(
                "### In-model relativization probe (diagnostic)\n\n{}\n\n",
                a.relativization_probe.trim()
            ));
        }
        md.push_str("---\n\n");
    }
    fs::write(assumptions_review_path(repo_path), md).map_err(|err| {
        format!(
            "Failed to write {}: {err}",
            assumptions_review_path(repo_path).display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("kernel has a workspace parent")
            .join("scratch/soundfix/assumptions-registry-tests");
        fs::create_dir_all(&scratch).unwrap();
        let base = scratch.join(format!(
            "assumptions_registry_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(base.join("Tablet")).unwrap();
        base
    }

    fn sample(id: &str) -> ProposedAssumption {
        ProposedAssumption {
            id: id.to_string(),
            axiom_name: "dec2flt.slice_len_le_isize_max".to_string(),
            lean_statement:
                "axiom dec2flt.slice_len_le_isize_max : ∀ (s : Slice U8), RustValidSliceU8 s → s.len ≤ Isize.max"
                    .to_string(),
            nl_statement:
                "\\begin{definition}\nEvery Rust slice length is at most isize::MAX.\n\\end{definition}"
                    .to_string(),
            citation_locator: "Rust Reference §Behavior considered undefined (alloc ≤ isize::MAX)"
                .to_string(),
            rust_justification:
                "The allocator never produces an allocation larger than isize::MAX bytes."
                    .to_string(),
            needed_by: vec!["parse_number_faithful".to_string()],
            claim_class: String::new(),
            adversarial_hunt_result: "failed to refute over 10k randomized runs".to_string(),
            relativization_probe: String::new(),
            status: "pending".to_string(),
            rejected_reason: String::new(),
        }
    }

    fn write_staged(repo: &Path, record: &ProposedAssumption) {
        fs::write(
            assumptions_lean_path(repo),
            format!(
                "{}{}\n{}\n{}{}\n",
                LEAN_ASSUMPTION_BEGIN_PREFIX,
                record.id,
                record.lean_statement,
                LEAN_ASSUMPTION_END_PREFIX,
                record.id,
            ),
        )
        .unwrap();
        fs::write(
            assumptions_tex_path(repo),
            format!(
                "{}{}\n{}\n{}{}\n",
                TEX_ASSUMPTION_BEGIN_PREFIX,
                record.id,
                record.nl_statement,
                TEX_ASSUMPTION_END_PREFIX,
                record.id,
            ),
        )
        .unwrap();
    }

    #[test]
    fn assumptions_scaffold_declares_nothing() {
        let scaffold = empty_assumptions_lean_scaffold();
        let code_lines: Vec<_> = scaffold
            .lines()
            .map(str::trim_start)
            .filter(|line| !line.starts_with("--"))
            .collect();
        assert!(!code_lines.iter().any(|line| line.starts_with("def ")));
        assert!(!code_lines.iter().any(|line| line.starts_with("axiom ")));
        assert!(!code_lines.iter().any(|line| line.starts_with("opaque ")));
        assert!(!scaffold.contains("RustValidSliceU8"));
    }

    #[test]
    fn assumptions_scaffold_agrees_with_extractor() {
        let extractor = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("scripts/extract_pv_model.py");
        let output = std::process::Command::new("python3")
            .arg("-c")
            .arg(
                "import importlib.util, sys\n\
                 spec = importlib.util.spec_from_file_location('extract_pv_model', sys.argv[1])\n\
                 module = importlib.util.module_from_spec(spec)\n\
                 sys.modules[spec.name] = module\n\
                 spec.loader.exec_module(module)\n\
                 sys.stdout.write(module._empty_assumptions_lean())",
            )
            .arg(extractor)
            .output()
            .expect("run the Python extractor scaffold");
        assert!(
            output.status.success(),
            "loading the Python extractor failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            empty_assumptions_lean_scaffold()
        );
    }

    #[test]
    fn staged_marker_detector_ignores_empty_scaffolds() {
        let repo = tmp();
        fs::write(
            assumptions_lean_path(&repo),
            empty_assumptions_lean_scaffold(),
        )
        .unwrap();
        fs::write(assumptions_tex_path(&repo), empty_assumptions_tex_scaffold()).unwrap();

        assert!(
            !has_worker_authored_staged_assumption(&repo).unwrap(),
            "template marker examples in the empty scaffold must not count as staged assumptions"
        );
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn staged_marker_detector_rejects_template_id_even_on_exact_marker_line() {
        let repo = tmp();
        fs::write(
            assumptions_lean_path(&repo),
            "-- BEGIN UNDERMODEL ASSUMPTION <id>\naxiom fake : True\n-- END UNDERMODEL ASSUMPTION <id>\n",
        )
        .unwrap();
        fs::write(
            assumptions_tex_path(&repo),
            "% BEGIN UNDERMODEL ASSUMPTION <id>\nFake assumption.\n% END UNDERMODEL ASSUMPTION <id>\n",
        )
        .unwrap();

        assert!(
            !has_worker_authored_staged_assumption(&repo).unwrap(),
            "`<id>` is a template placeholder, not a worker-authored assumption id"
        );
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn staged_marker_detector_accepts_real_marked_block() {
        let repo = tmp();
        let record = sample("a1");
        write_staged(&repo, &record);

        assert!(
            has_worker_authored_staged_assumption(&repo).unwrap(),
            "a real exact marker line with a concrete id should activate Assumptions corr"
        );
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn record_then_pending_visible() {
        let repo = tmp();
        assert!(!has_pending(&repo).unwrap());
        let record = sample("a1");
        write_staged(&repo, &record);
        record_proposed(&repo, record).unwrap();
        assert!(has_pending(&repo).unwrap());
        let store = load_proposed(&repo).unwrap();
        assert_eq!(store.pending().count(), 1);
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn project_writes_all_three_sinks_and_clears_proposed() {
        let repo = tmp();
        let record = sample("a1");
        write_staged(&repo, &record);
        record_proposed(&repo, record).unwrap();
        project_approved(&repo, "a1").unwrap();

        // Sink 1: APPROVED_AXIOMS.json global has the axiom.
        let aa: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(approved_axioms_path(&repo)).unwrap()).unwrap();
        let global = aa["global"].as_array().unwrap();
        assert!(global
            .iter()
            .any(|v| v.as_str() == Some("dec2flt.slice_len_le_isize_max")));

        // Sink 2: tcb_manifest.json disclosure has the classified entry.
        let tcb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tcb_manifest_path(&repo)).unwrap()).unwrap();
        let disc = tcb["tcb_disclosure"].as_array().unwrap();
        assert!(disc.iter().any(|e| {
            e["name"].as_str() == Some("dec2flt.slice_len_le_isize_max")
                && e["classification"].as_str() == Some(UNDER_MODEL_CLASSIFICATION)
        }));

        // Staged file remains the authoritative Lean source; approval does not
        // append a second copy.
        let lean = fs::read_to_string(assumptions_lean_path(&repo)).unwrap();
        assert!(lean.contains("axiom dec2flt.slice_len_le_isize_max"));
        assert_eq!(lean.matches("axiom dec2flt.slice_len_le_isize_max").count(), 1);

        // Proposed file no longer lists it.
        assert!(!has_pending(&repo).unwrap());
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn reject_marks_rejected_not_pending() {
        let repo = tmp();
        let record = sample("a1");
        write_staged(&repo, &record);
        record_proposed(&repo, record).unwrap();
        reject(&repo, "a1", "operator judged it a program invariant").unwrap();
        assert!(!has_pending(&repo).unwrap());
        let store = load_proposed(&repo).unwrap();
        assert_eq!(store.assumptions[0].status, "rejected");
        let lean = fs::read_to_string(assumptions_lean_path(&repo)).unwrap();
        assert!(!lean.contains("axiom dec2flt.slice_len_le_isize_max"));
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn project_is_idempotent() {
        let repo = tmp();
        let record = sample("a1");
        write_staged(&repo, &record);
        record_proposed(&repo, record.clone()).unwrap();
        project_approved(&repo, "a1").unwrap();
        // A second record + project of the same axiom must not duplicate sinks.
        record_proposed(&repo, record).unwrap();
        project_approved(&repo, "a1").unwrap();
        let aa: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(approved_axioms_path(&repo)).unwrap()).unwrap();
        let count = aa["global"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v.as_str() == Some("dec2flt.slice_len_le_isize_max"))
            .count();
        assert_eq!(count, 1);
        fs::remove_dir_all(&repo).ok();
    }

    // ── Slice 3: the type-driven conditioning + shape check ────────────────

    /// The five behavior contracts actually staged by the dec2flt run (two
    /// unicode spellings, multi-binder groups, a bare `∃` binder, ASCII
    /// `forall`/`->`): all must pass the check unchanged.
    const REAL_STAGED_STATEMENTS: [&str; 5] = [
        "axiom RustValidSliceU8_range_from_one_result :\n  ∀ (s s1 : Slice Std.U8),\n    RustValidSliceU8 s →\n    core.slice.index.Slice.index\n      (core.slice.index.SliceIndexRangeFromUsizeSlice Std.U8) s\n      { start := 1#usize } = ok s1 →\n    RustValidSliceU8 s1",
        "axiom RustValidSliceU8_first_index_zero :\n  ∀ (s : Slice Std.U8) (b : Std.U8),\n    RustValidSliceU8 s →\n    0 < (Slice.len s).val →\n    Slice.index_usize s 0#usize = ok b →\n    dec2flt_full_integer.core.slice.Slice.first s = ok (some b)",
        "axiom RustValidSliceU8_split_first_index_zero_tail :\n  ∀ (s : Slice Std.U8) (b : Std.U8),\n    RustValidSliceU8 s →\n    0 < (Slice.len s).val →\n    Slice.index_usize s 0#usize = ok b →\n    ∃ tail : Slice Std.U8,\n      dec2flt_full_integer.core.slice.Slice.split_first s =\n        ok (some (b, tail)) ∧\n      RustValidSliceU8 tail ∧\n      tail.val = s.val.drop 1",
        "axiom RustValidSliceU8_split_first_empty :\n  ∀ (s : Slice Std.U8),\n    RustValidSliceU8 s →\n    (Slice.len s).val = 0 →\n    dec2flt_full_integer.core.slice.Slice.split_first s = ok none",
        "axiom RustValidSliceU8_len_isize_bound :\n  forall (s : Slice Std.U8),\n    RustValidSliceU8 s ->\n    ((Slice.len s).val : Int) <= IScalar.max .Isize",
    ];

    #[test]
    fn conditioning_check_accepts_all_real_staged_behavior_contracts() {
        for statement in REAL_STAGED_STATEMENTS {
            let errors = staged_assumption_statement_errors(statement, "behavior");
            assert!(
                errors.is_empty(),
                "real staged contract flagged: {errors:?}\n{statement}"
            );
        }
    }

    #[test]
    fn conditioning_check_rejects_unconditional_container_quantification() {
        // The origin episode's shape: false-in-model when unconditioned.
        let statement = "axiom bad_len_bound :\n  ∀ (s : Slice Std.U8),\n    ((Slice.len s).val : Int) ≤ IScalar.max .Isize";
        let errors = staged_assumption_statement_errors(statement, "behavior");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("`s`"), "{errors:?}");
        assert!(errors[0].contains("hook"), "{errors:?}");
    }

    #[test]
    fn conditioning_check_rejects_non_rustvalid_prefixed_hook() {
        let statement = "axiom bad_len_bound :\n  ∀ (s : Slice Std.U8),\n    SliceInRustDomain s →\n    ((Slice.len s).val : Int) ≤ IScalar.max .Isize";
        let errors = staged_assumption_statement_errors(statement, "behavior");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("RustValid*"), "{errors:?}");
        assert!(
            errors[0].contains("validity-definition node"),
            "{errors:?}"
        );
        assert!(
            errors[0].contains("assumption-authoring burst cannot create hooks"),
            "{errors:?}"
        );
    }

    #[test]
    fn conditioning_check_accepts_realization_domain_claim() {
        // The cycle-280 realization candidate's shape: quantifies over a pure
        // List (unconditioned — List is not a container token) and asserts a
        // hook-valid Slice witness exists.
        let statement = "axiom RustValidSliceU8_byte_list_complete :\n  ∀ (bs : List Std.U8),\n    ((bs.length : Int) ≤ IScalar.max .Isize) →\n    ∃ s : Slice Std.U8,\n      RustValidSliceU8 s ∧\n      bs.length = (Slice.len s).val";
        let errors = staged_assumption_statement_errors(statement, "domain");
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn shape_check_rejects_domain_claim_without_existential_container_binder() {
        let statement = "axiom not_a_domain_claim :\n  ∀ (s : Slice Std.U8),\n    RustValidSliceU8 s →\n    (Slice.len s).val = (Slice.len s).val";
        let errors = staged_assumption_statement_errors(statement, "domain");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("existential"), "{errors:?}");
    }

    #[test]
    fn shape_check_rejects_unknown_claim_class() {
        let errors = staged_assumption_statement_errors("axiom x : True", "realization");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("claim_class"), "{errors:?}");
    }

    #[test]
    fn record_proposed_fails_closed_on_unconditioned_statement() {
        let repo = tmp();
        let mut record = sample("a1");
        record.lean_statement =
            "axiom dec2flt.slice_len_le_isize_max : ∀ (s : Slice U8), s.len ≤ Isize.max"
                .to_string();
        write_staged(&repo, &record);
        let err = record_proposed(&repo, record).unwrap_err();
        assert!(err.contains("conditioning/shape check"), "{err}");
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn render_review_flags_domain_class_as_binding() {
        let repo = tmp();
        let mut record = sample("a1");
        record.claim_class = "domain".to_string();
        record.lean_statement = "axiom dec2flt.realized :\n  ∀ (bs : List Std.U8),\n    ((bs.length : Int) ≤ IScalar.max .Isize) →\n    ∃ s : Slice Std.U8, RustValidSliceU8 s"
            .to_string();
        write_staged(&repo, &record);
        record_proposed(&repo, record).unwrap();
        render_review(&repo).unwrap();
        let md = fs::read_to_string(repo.join("ASSUMPTIONS_REVIEW.md")).unwrap();
        assert!(md.contains("**Claim class**: `domain`"), "{md}");
        assert!(md.contains("BINDING"), "{md}");
        assert!(md.contains("Consistency basis"), "{md}");
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn projected_disclosure_carries_claim_class() {
        let repo = tmp();
        let record = sample("a1");
        write_staged(&repo, &record);
        record_proposed(&repo, record).unwrap();
        project_approved(&repo, "a1").unwrap();
        let tcb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tcb_manifest_path(&repo)).unwrap()).unwrap();
        let disc = tcb["tcb_disclosure"].as_array().unwrap();
        assert!(disc.iter().any(|e| {
            e["name"].as_str() == Some("dec2flt.slice_len_le_isize_max")
                && e["claim_class"].as_str() == Some("behavior")
        }));
        fs::remove_dir_all(&repo).ok();
    }

    // ── extraction provenance (manifest) ───────────────────────────────────

    fn sample_digest(file: &str, sha: &str) -> ExtractionSourceDigest {
        ExtractionSourceDigest {
            source_file: file.to_string(),
            source_sha256: sha.to_string(),
        }
    }

    fn complete_extraction_toolchain() -> serde_json::Value {
        serde_json::json!({
            "charon": "0.1.216",
            "charon_revision": "1111111111111111111111111111111111111111",
            "aeneas": "fa699427",
            "aeneas_revision": "2222222222222222222222222222222222222222",
            "lean": "4.30.0-rc2",
            "rustc_vv": "rustc fixture\nhost: x86_64-unknown-linux-gnu\n",
            "target_triple": "x86_64-unknown-linux-gnu",
            "target_pointer_width": 64,
            "cargo_features": [{"name":"demo","version":"0.1.0","source":"workspace:Cargo.toml","features":[]}],
            "cargo_lock_sha256": "3333333333333333333333333333333333333333333333333333333333333333",
            "extraction_profile": "dev",
            "overflow_checks": true,
            "panic_strategy": "unwind",
            "cfg": ["overflow_checks", "panic=\"unwind\"", "target_pointer_width=\"64\""],
        })
    }

    #[test]
    fn extraction_provenance_records_toolchain_stack_and_deduped_digests() {
        let repo = tmp();
        let toolchain = complete_extraction_toolchain();
        let stack = vec![
            "charon@0.1.216".to_string(),
            "aeneas@fa699427".to_string(),
        ];
        // 66 models, one crate file: every pair identical ⇒ ONE digest entry.
        let digests = vec![
            sample_digest("src/lib.rs", "abc123"),
            sample_digest("src/lib.rs", "abc123"),
            sample_digest("src/lib.rs", "abc123"),
        ];
        let updated =
            record_extraction_provenance(&repo, &toolchain, &stack, &digests, &[]).unwrap();
        assert!(updated, "first write must report a change");

        let tcb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tcb_manifest_path(&repo)).unwrap()).unwrap();
        assert_eq!(tcb["schema"].as_str(), Some("pv-tcb-disclosure/v1"));
        let prov = &tcb["extraction_provenance"];
        assert_eq!(prov["extraction_toolchain"], toolchain);
        assert_eq!(
            prov["extractor_toolchain_sha256"].as_str(),
            Some(extraction_toolchain_sha256(&stack, &toolchain).as_str())
        );
        assert_eq!(
            prov["extractor_stack"],
            serde_json::json!(["charon@0.1.216", "aeneas@fa699427"])
        );
        assert_eq!(
            prov["source_digests"],
            serde_json::json!([{"source_file": "src/lib.rs", "source_sha256": "abc123"}]),
            "duplicate per-model pairs must collapse to one digest entry"
        );

        // Idempotent: an identical re-record must not rewrite the file.
        let updated =
            record_extraction_provenance(&repo, &toolchain, &stack, &digests, &[]).unwrap();
        assert!(!updated, "identical re-record must be a no-op");
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn extraction_provenance_flags_unrecorded_pins_honestly() {
        let repo = tmp();
        // A legacy config pins only the three original tool versions. Every
        // missing environment/source fact is DISCLOSED with a reason.
        let toolchain = serde_json::json!({"charon": "c", "aeneas": "a", "lean": "l"});
        record_extraction_provenance(&repo, &toolchain, &[], &[], &[]).unwrap();
        let tcb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tcb_manifest_path(&repo)).unwrap()).unwrap();
        let prov = &tcb["extraction_provenance"];
        let entries = prov["not_recorded"].as_array().unwrap();
        let fields: std::collections::BTreeSet<_> = entries
            .iter()
            .filter_map(|entry| entry["field"].as_str())
            .collect();
        for field in [
            "rustc_vv",
            "target_triple",
            "target_pointer_width",
            "cargo_features",
            "cargo_lock_sha256",
            "extraction_profile",
            "overflow_checks",
            "panic_strategy",
            "cfg",
            "charon_revision",
            "aeneas_revision",
            "extractor_stack",
            "source_digests",
            "docs_corpus_revision",
        ] {
            assert!(fields.contains(field), "missing honest negative for {field}");
        }
        assert!(entries.iter().all(|entry| {
            entry["field"].as_str().is_some_and(|value| !value.is_empty())
                && entry["reason"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
        }));
        assert!(
            prov.get("docs_corpus").is_none(),
            "no corpus manifest ⇒ no docs_corpus section"
        );
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn extraction_provenance_embeds_docs_corpus_and_drops_recorded_pins() {
        let repo = tmp();
        fs::create_dir_all(repo.join("rust_docs_corpus")).unwrap();
        fs::write(
            repo.join(DOCS_CORPUS_MANIFEST_REL),
            serde_json::json!({
                "schema": "pv-rust-docs-corpus/v1",
                "sources": [
                    {"name": "rust-reference", "url": "u", "rev": "86635e3"},
                ],
            })
            .to_string(),
        )
        .unwrap();
        // A complete config drops all toolchain evidence from not_recorded.
        let toolchain = complete_extraction_toolchain();
        record_extraction_provenance(&repo, &toolchain, &[], &[], &[]).unwrap();
        let tcb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tcb_manifest_path(&repo)).unwrap()).unwrap();
        let prov = &tcb["extraction_provenance"];
        assert_eq!(
            prov["not_recorded"],
            serde_json::json!([
                {"field":"extractor_stack","reason":"pv_tablet.extractor_stack is empty"},
                {"field":"source_digests","reason":"no extraction model provenance was supplied"}
            ])
        );
        assert_eq!(
            prov["docs_corpus"]["manifest_path"].as_str(),
            Some(DOCS_CORPUS_MANIFEST_REL)
        );
        assert_eq!(
            prov["docs_corpus"]["sources"][0]["rev"].as_str(),
            Some("86635e3")
        );
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn extraction_provenance_preserves_and_survives_disclosure_entries() {
        let repo = tmp();
        // Disclosure first, provenance second: the append is preserved.
        let record = sample("a1");
        write_staged(&repo, &record);
        record_proposed(&repo, record).unwrap();
        project_approved(&repo, "a1").unwrap();
        let toolchain = serde_json::json!({"charon": "c"});
        record_extraction_provenance(&repo, &toolchain, &[], &[], &[]).unwrap();
        // Provenance first is covered by re-projecting after: the disclosure
        // sink's read-modify-write must keep the provenance section.
        let record = sample("a1");
        write_staged(&repo, &record);
        record_proposed(&repo, record).unwrap();
        project_approved(&repo, "a1").unwrap();
        let tcb: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tcb_manifest_path(&repo)).unwrap()).unwrap();
        assert!(tcb["tcb_disclosure"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"].as_str() == Some("dec2flt.slice_len_le_isize_max")));
        assert_eq!(
            tcb["extraction_provenance"]["extraction_toolchain"],
            toolchain
        );
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn render_review_lists_pending_verbatim() {
        let repo = tmp();
        let record = sample("a1");
        write_staged(&repo, &record);
        record_proposed(&repo, record).unwrap();
        render_review(&repo).unwrap();
        let md = fs::read_to_string(repo.join("ASSUMPTIONS_REVIEW.md")).unwrap();
        assert!(md.contains("dec2flt.slice_len_le_isize_max"));
        assert!(md.contains("Every Rust slice length"));
        assert!(md.contains("parse_number_faithful"));
        assert!(md.contains("failed to refute"));
        fs::remove_dir_all(&repo).ok();
    }
}

use crate::model::{
    Blocker, BlockerKind, BlockerObject, CleanupTaskKind, NodeId, Phase, PvRole,
    RefPaperId, RequestKind, RetryOutcomeKind, TargetId, TaskMode, WorkerProfile,
    WorkerValidationKind, WorkerWorkKind, WrapperRequest,
};
use crate::{blocker_choice_ids, blocker_choices, extract_tex_statement_items};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

const UNDER_MODEL_VALIDITY_HOOK_POLICY: &str = concat!(
    "State property axioms through the available Rust-validity/correspondence ",
    "hook. Every quantified variable of an over-approximating container type ",
    "(Slice, Str, Vec, Array) must sit under that type's hook applied to it; ",
    "a domain claim's existential witness must assert hook membership. The ",
    "kernel rejects a staged statement violating this."
);
const UNDER_MODEL_THEOREM_STATEMENT_POLICY: &str = "Leave existing theorem statements unchanged.";

pub fn default_contract_value() -> Value {
    json!({})
}

pub fn prompt_contract_version() -> u32 {
    // Bumped 63 -> 64 because the Phase-0 worker request now carries a
    // prompt-sized VIEW of the failing checker receipt instead of the whole
    // thing: the top-level `stdout_base64` duplicate and the captured streams
    // of stages that passed are dropped, an `inline_view` note records what
    // went and points at the receipt store, and the failing stage is verbatim
    // so every quotable byte keeps its offset.  The worker sees materially
    // different request bytes, so the version moves with them.
    //
    // Bumped 62 -> 63 because the Phase-0 worker draft contract now states how
    // the quoted diagnostic's recurrence is checked: the entry is omitted, the
    // tools are re-run, and the quoted diagnostic is matched in the same file,
    // ignoring line and column numbers.
    // Bumped 61 -> 62 because the Phase-0 worker draft contract now states that
    // every ledgered edit is already made in the candidate tree at
    // candidate_tree_path, that the ledger only describes what that tree holds,
    // and that the framework computes the candidate manifest from it.
    // The same bump carries the seam-class semantics, stated identically in the
    // worker draft contract and the seam-repair audit result contract.
    // Bumped 60 -> 61 because the Phase-0 worker draft contract now states that
    // before_span is measured in the uploaded file and after_span in the final
    // candidate, that several entries may modify one file, and that every
    // prior entry is carried forward unless re-specified.
    // Bumped 59 -> 60 because the Phase-0 worker draft contract now states the
    // bridge computes span digests from the patch bytes.
    // Bumped 58 -> 59 because the Phase-0 worker draft contract now requires the
    // quoted diagnostic to be one complete line of the cited stream.
    // Bumped 57 -> 58 because the Phase-0 worker draft contract shows citation
    // shapes as literal example objects (the field-list map was misread).
    // Bumped 56 -> 57 because the Phase-0 worker draft contract now states that
    // untouched prior ledger entries are carried forward and that the bridge
    // binds the worker thread identity.
    // Bumped 55 -> 56 because the Phase-0 worker draft contract now states the
    // span, digest, citation and quoted-error shapes.
    // Bumped 54 -> 55 because the Phase-0 worker request now carries the
    // exact GOAL target ids its ledger entries may name, and the worker draft
    // contract states the accepted delivery format.
    // Bumped 53 -> 54 because the Phase-0 correspondence contract now names
    // target/supporting/adaptation origin and outside-target status per GOAL binding.
    // Bumped 52 -> 53 because the Phase-0 correspondence contract now requires
    // an explicit answer for every GOAL binding supplied by adaptation.
    // Bumped 51 -> 52 for the three role-local Phase-0 source-adaptation
    // contracts. Their strict field contracts are kernel-generated.
    // Bumped 50 -> 51 for generic conditional proposal, dedicated
    // correspondence authorization, and ratification packet contracts.
    // Bumped 49 -> 50 for the role-local Rust witness declaration and the
    // dedicated request-bound artifact correspondence scenario.
    // Bumped 48 -> 49 for the Lean-only disproof cutover: callable/source
    // station, give-up, witness-evaluation, and conditional-candidate prompt
    // carriers were removed from every surviving request contract.
    // Bumped 47 -> 48 when the retired PV goal-mode selector and its pinned-
    // statement prompt branch were removed; PV requests now have one GOAL.md
    // authoring contract.
    // Worker and Corr requests receive the bootstrap-disclosed primitive
    // signatures, call sites, and reachability edges. The additive field is
    // skipped when empty and flows through the structural bridge payload.
    // Spec mirrored (`promptContractVersion` 47 -> 48).
    //
    // Bumped 42 -> 43 for the typed substantiveness outcome
    // `FalseAsStated`; the verifier contract and persistent status mirror
    // preserve it independently from generic Fail. Spec mirrored.
    //
    // Bumped 41 -> 42 for RequiredV1 trust-route completeness: GapResearch
    // advertises structured adjudication / statement binding or a terminal
    // refusal, never a NeedInput HumanGate; target-bound refutation carriers
    // and certificate-context approved assumptions are now projected into
    // the audit request. Spec mirrored (`promptContractVersion` 41 -> 42).
    //
    // Bumped 40 -> 41 for operator-provided reference papers: a
    // configured registry (`workflow.reference_papers`) of additional
    // grounding documents for results the primary paper cites. The
    // primary paper remains the sole target authority. Workers claim a
    // registry document per node via the new `node_reference_grounds`
    // payload field (full-set replace, unknown ids rejected
    // fail-closed); claims feed the substantiveness fingerprint
    // (`claimed_reference_shas`) and reopen the lane. Reviewer
    // `paper_focus_ranges` entries gain an optional `doc` naming a
    // registry document. New conditionally-pushed prompt fragments for
    // worker/review/stuck-math-audit (registry non-empty) and the
    // substantiveness verifier (a frontier node has claims); an
    // empty-registry run's prompts and wire JSON are byte-identical to
    // v40 apart from this number. Spec mirrored
    // (`promptContractVersion` 40 -> 41).
    //
    // Bumped 39 -> 40 for four new Lean declaration kinds as Tablet
    // nodes. `structure` / `inductive` / `class` are recognized as
    // definition (non-proof-bearing) nodes, siblings of `def`/`abbrev`,
    // paired to the `.tex` `definition` environment; they split at
    // `-- BODY` with `where` as the body delimiter (head above, fields /
    // constructors below). An empty `where`-body is the structural
    // placeholder, the analog of the `True`-body for defs, and autofails
    // correspondence. `inductive` must use the `where` form (bare-pipe
    // rejected). `instance` is recognized as a proof-bearing node paired
    // to a `.tex` `lemma` claim, routed through soundness; it must be
    // named (anonymous instances rejected) and its body delimiter may be
    // `:=`, `:= by`, or structure-instance `where`. Scheme/FILESPEC docs
    // state the new kinds. Spec mirrored (`promptContractVersion`
    // 39 -> 40).
    //
    // Bumped 38 -> 39 for the TheoremStating Restructure mode: a third
    // legal TheoremStating next-mode (alongside Global/Targeted) that
    // escapes the candidate cone. The reviewer names `authorized_node_ids`
    // (the coordinated cross-cone set to restate) and a `next_active` focus
    // within it; the candidate / authorization envelope broadens from the
    // cone to `present_nodes \ frozen`, where a node is frozen iff it is in
    // the approved-target protected closure, is challenge byte-pinned, or is
    // Preamble/Axioms. The Review request gains
    // `theorem_restructure_next_active_nodes` (the envelope). It is FREE (no
    // new audit gate): every restated node re-enters the correspondence +
    // substantiveness lanes. Also generalizes the targeted-mode next_active
    // requirement to waive every non-acting decision (AdvancePhase / Done /
    // NeedInput / reset-Continue), not only AdvancePhase (Defect A). New
    // worker fragment: `worker/theorem_stating/19_restructure_scope.md`.
    // Spec mirrored (`promptContractVersion` 38 -> 39).
    //
    // Bumped 37 -> 38 for challenge targets: a second target type
    // mirroring paper targets, prescribing exact Lean declarations from
    // an imported lean-eval problem (`configured_challenge_targets`).
    // Worker payload gains `challenge_claim_updates` (exclusive: one
    // challenge target per node; claiming node named exactly the
    // prescribed declaration name). The kernel byte-compares the
    // claiming node's FILESPEC slice against the prescription at every
    // acceptance, in every mode including proof_coarse_restructure.
    // Each uncovered challenge target derives a `ChallengeCoverage`
    // blocker riding the existing AdvancePhase/Done gating, excluded
    // from reset_blockers. Worker / Review / Audit / StuckMathAudit
    // requests surface the registry plus `current_challenge_claims`;
    // verifier lanes stay challenge-free. New prompt fragments:
    // `worker/theorem_stating/18_challenge_targets.md`,
    // `worker/proof_formalization/09_challenge_target_frozen.md`,
    // `review/common/33d_challenge_targets.md`,
    // `verifier/correspondence/08_challenge_covering_nodes.md`.
    //
    // Bumped 36 -> 37 for rejected-deviation recovery: a node claiming a
    // deviation the Deviation lane rejected (`current_deviation_fail`) now
    // reads substantiveness Fail (not Unknown), dropping it off
    // `substantiveness_verify_nodes()` and onto the worker frontier. The
    // substantiveness scenario payload gains a `rejected_deviations` map so
    // the verifier treats such a claim as an expected Fail to route to the
    // worker rather than a system inconsistency.
    //
    // Bumped 35 -> 36 for the GapResearch planner ↔ critic loop: a
    // confirmed-genuine paper gap now dispatches a GapResearch Planner
    // (role fragment `stuck_math_audit/common/01_gap_research_role.md`)
    // that produces a brief `report` + a natural-language proof-route
    // `route_tex` (or sets `route_needs_human`); a dedicated critic then
    // returns `gap_decision` (accept/reject). On ACCEPT the critic writes
    // the existing AuditPlan shape (`report` + `tasks`), routed to the
    // Reviewer; on REJECT it returns `gap_feedback`. Response fields:
    // `route_tex` / `route_needs_human` / `gap_decision` / `gap_feedback`
    // (+ the persisted `gap_brief`). Spec mirrored
    // (`promptContractVersion` 35 -> 36).
    //
    // Bumped 34 -> 35 for the NeedInput re-entry guard (Defect 2): a
    // reviewer `need_input` decision is now illegal while
    // `human_input_outstanding` is set unless the reviewer also consumes
    // the input with `clear_human_input=true`. This closes the
    // Review -> StuckMathAudit -> HumanGate -> Review livelock a
    // content-free human approve could spin. The reviewer-facing field
    // schema is unchanged (`clear_human_input` already existed); only the
    // legality precondition is tightened.
    //
    // Bumped 33 -> 34 for deviation protocol prompts and reviewer
    // evidence separation.
    //
    // Bumped 32 -> 33 for NeedInputAuditor: reviewer `need_input`
    // requests now first dispatch a dedicated auditor scenario on the
    // existing stuck_math_audit lane, and the audit artifact gains
    // `confirm_need_input`.
    //
    // Bumped 31 -> 32 for the active-coarse-anchor workflow layer
    // (proposal v32, 2026-05-20). ProofFormalization adds a locked
    // coarse-DAG focus on top of the existing per-cycle active_node:
    // the reviewer picks an `active_coarse_node` from
    // `kernel_hinted_next_active_coarse_nodes`, then `active_node`
    // legality is narrowed to the down-cone of that anchor (widened
    // to blocker-repair cones when `coarse_repair_mode` is true).
    // The anchor stays locked against change until shallow-coarse
    // closure + empty global blockers, OR a starvation threshold
    // (`stuck_coarse_repair_threshold`) is hit. Reviewer response
    // gains `next_active_coarse: Option<NodeId>`; review request
    // surfaces `active_coarse_node`, `kernel_hinted_next_active_coarse_nodes`,
    // `coarse_repair_mode`, `cycles_in_coarse_repair_mode`. New
    // prompt fragments: `08_coarse_anchor_locked.md`,
    // `08_coarse_anchor_open.md`, `09_coarse_repair_mode.md`.
    // Mechanism dormant when `coarse_dag_nodes` is empty.
    //
    // Bumped 30 -> 31 to rename proof next-active routing from hard
    // allowed nodes to kernel hints and surface reviewer rejection reasons.
    //
    // Bumped 29 -> 30 to surface shallow-coarse progress counters and
    // make StuckMathAudit activation use a configurable no-progress
    // threshold instead of worker Stuck/NeedsRestructure outcomes.
    //
    // Bumped 28 -> 29 for a dedicated StuckMathAudit reference-paper
    // fragment that exposes the configured paper source path and makes
    // paper-grounding explicit for the read-only audit role.
    //
    // Bumped 27 -> 28 for StuckMathAudit as an independent read-only
    // audit role with its own prompt/artifact contract and durable
    // audit_plan handoff to reviewers/workers.
    //
    // Bumped 26 -> 27 for StuckMathAudit reviewer Lean product handoff.
    // Repeated proof-formalization math blockage can now activate a
    // reviewer-side Lean scratch mode and forward a neutral
    // `reviewer_lean_product` to the next worker.
    //
    // Bumped 25 -> 26 for explicit proof-obligation scope controls
    // (2026-05-04). Reviewers now choose allow_new_obligations and
    // must_close_active; easy/hard difficulty is advisory only.
    //
    // Bumped 24 → 25 for pending protected-reapproval visibility
    // (2026-05-03). Review requests now surface the pending protected
    // reapproval node set when ordinary verifier blockers still need to
    // drain before the HumanGate reapproval.
    //
    // Bumped 23 → 24 for protected semantic change scoping (2026-05-03).
    // Reviewer contracts can now surface protected_semantic_change_node_ids
    // plus a confirmation flag; worker contracts surface the approved
    // protected scope when such a change is explicitly authorized.
    //
    // Bumped 22 → 23 for the substantiveness lane (2026-04-29).
    // Kernel now emits Paper requests with `substantiveness_verify_nodes`
    // populated in the per-node scenario; verifier prompt picks between
    // target-package and per-node-frontier rubrics; PaperResponse carries
    // a new `node_lane_updates` field with `SubstantivenessStatus` (admits
    // `NotDoneYet` for verifier triage). Spec must match.
    //
    // Bumped 45 -> 46 for the witness-shape authoring lint (2026-09-01).
    // Worker requests on a run whose seed registry carries a statement-
    // deferred contract now carry `deferred_witness_fields` (target ->
    // witness-schema field names), so the acceptance lint can ask whether
    // an authored statement would yield a derivable certificate goal.
    // Additive and skipped when empty, so non-deferred runs stay
    // byte-identical. Spec must match.
    64
}

/// Render the challenge-target registry for request payloads: id ->
/// {kind, name, lean, namespace_context, informal}. Provenance is
/// operator metadata and stays out of prompts.
fn challenge_registry_json(request: &WrapperRequest) -> Value {
    let registry: BTreeMap<&crate::model::ChallengeTargetId, Value> = request
        .configured_challenge_targets
        .iter()
        .map(|(id, spec)| {
            (
                id,
                json!({
                    "kind": spec.kind,
                    "name": spec.name,
                    "lean": spec.lean,
                    "namespace_context": spec.namespace_context,
                    "informal": spec.informal,
                }),
            )
        })
        .collect();
    json!(registry)
}

/// Current challenge coverage derived from the request's claims view:
/// one entry per configured challenge target, covering nodes listed.
fn challenge_coverage_json(request: &WrapperRequest) -> Value {
    let coverage: BTreeMap<&crate::model::ChallengeTargetId, BTreeSet<&NodeId>> = request
        .configured_challenge_targets
        .keys()
        .map(|id| {
            let covering = request
                .current_challenge_claims
                .iter()
                .filter(|(node, claims)| {
                    request.current_present_nodes.contains(*node) && claims.contains(id)
                })
                .map(|(node, _)| node)
                .collect();
            (id, covering)
        })
        .collect();
    json!(coverage)
}

/// The claim rules the kernel enforces at acceptance, stated where the
/// schema names the field (the consume-schema convention: the string
/// names what the kernel enforces).
const CHALLENGE_CLAIM_RULES: &str = "a challenge claim is exclusive (at most one challenge target per node); the claiming node is named exactly the target's prescribed declaration name; the kernel byte-compares the claiming node's FILESPEC slice against the prescribed text at every acceptance in every mode, proof_coarse_restructure included (theorem: the slice between the tablet-node marker and `-- BODY` must equal the prescription, imports above the marker stay free; def: the slice extends through end of file). Coverage of every configured challenge target is required before AdvancePhase/Done.";

/// PV stuck-audit source-of-truth fragment. PV campaigns always read GOAL.md,
/// the source crate, and the generated extraction model.
fn pv_stuck_audit_source_of_truth(_request: &WrapperRequest) -> &'static str {
    "pv/stuck_audit/02_source_of_truth.md"
}

include!(concat!(env!("OUT_DIR"), "/isabelle_fragment_twins.rs"));

/// Route a fragment id to its `_isabelle` sibling on an Isabelle run.
///
/// The table is regenerated from disk by `build.rs` on every build, so adding a
/// variant FILE is the only step: it routes automatically, and a fragment with
/// no sibling keeps the shared text. Variants used to be selected by a
/// hand-written conditional per call site, so a forgotten conditional shipped
/// Lean text to an Isabelle run with nothing failing — the Lean fragment exists
/// and renders. `tests/isabelle_prompt_decontamination.rs` now fails if a Lean
/// variant is ever emitted while its sibling exists.
fn route_fragment(id: &'static str, target: crate::backend::BackendId) -> &'static str {
    if target != crate::backend::BackendId::IsabelleHol {
        return id;
    }
    match ISABELLE_FRAGMENT_TWINS.binary_search_by(|(lean, _)| (*lean).cmp(id)) {
        Ok(idx) => ISABELLE_FRAGMENT_TWINS[idx].1,
        Err(_) => id,
    }
}

/// Route a whole fragment list. Applied where a contract emits its list, so
/// every producer is covered without per-producer edits.
fn route_fragments(
    fragments: Vec<&'static str>,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    fragments
        .into_iter()
        .map(|id| route_fragment(id, target))
        .collect()
}

fn scheme_fragment_path(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> &'static str {
    if request.is_pv {
        // PV runs are substitutive: the orientation scheme is the PV variant
        // (no paper-faithfulness lane, model leaves byte-pinned). There is no
        // PV "brief" variant — the brief path is not exercised for PV, so the
        // full PV scheme covers both.
        return "pv/common/TRELLIS_FORMALIZATION_SCHEME.md";
    }
    let isabelle = target == crate::backend::BackendId::IsabelleHol;
    if request_uses_full_scheme(request) {
        if isabelle {
            "common/TRELLIS_FORMALIZATION_SCHEME_isabelle.md"
        } else {
            "common/TRELLIS_FORMALIZATION_SCHEME.md"
        }
    } else if isabelle {
        "common/00_trellis_scheme_brief_isabelle.md"
    } else {
        "common/00_trellis_scheme_brief.md"
    }
}

fn verifier_scheme_fragment_path(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> &'static str {
    if request.is_pv {
        return "pv/common/TRELLIS_FORMALIZATION_SCHEME_verifier.md";
    }
    // B6: verifiers always get the trim "verifier reference" version that
    // omits reviewer-only mode-machinery (TheoremStating Global/Targeted,
    // ProofFormalization Easy/Hard, end-to-end reviewer step).
    if target == crate::backend::BackendId::IsabelleHol {
        "common/TRELLIS_FORMALIZATION_SCHEME_verifier_isabelle.md"
    } else {
        "common/TRELLIS_FORMALIZATION_SCHEME_verifier.md"
    }
}

fn request_uses_full_scheme(request: &WrapperRequest) -> bool {
    match request.kind {
        RequestKind::Paper | RequestKind::Corr | RequestKind::Sound => true,
        RequestKind::Worker | RequestKind::Review => request.fresh_context,
        RequestKind::HumanGate => false,
        // Cleanup-v2 audit gets its own scheme treatment in the audit
        // contract payload (added later); for now, mirror the verifier
        // policy (trim scheme reference) since the audit is a one-shot
        // structured-output role rather than a stateful worker/reviewer
        // continuation.
        RequestKind::Audit | RequestKind::StuckMathAudit => true,
    }
}

/// Verifier housekeeping fields scrubbed from the inline prompt contract
/// JSON. Mirrors `bridge_prompts._VERIFIER_HOUSEKEEPING_FIELDS`. These
/// fields either are kernel render-machinery (prompt_fragments,
/// artifact_prompt_view), are rendered separately via dedicated
/// placeholders (request_summary, previous_own_findings_*), or are
/// kernel-only flags the prompt text already explains
/// (issue_reporting_policy, fixed_item_reporting_policy).
const PROMPT_FACING_VERIFIER_HOUSEKEEPING_DROP: &[&str] = &[
    "prompt_fragments",
    "request_summary",
    "artifact_prompt_view",
    "issue_reporting_policy",
    "fixed_item_reporting_policy",
    "previous_own_findings_by_lane",
    "previous_own_findings",
    "previous_own_findings_for_lane",
];

/// Build a paper-contract prompt-facing view. Drops verifier housekeeping
/// fields + paper-specific duplicates (`target_covering_nodes`,
/// `node_paper_basis_inputs`) that are rendered separately via dedicated
/// placeholders. Drops null Option<> fields. Mirrors
/// `bridge_prompts._prompt_facing_paper_contract` exactly.
///
/// Note: `target_issue_scope` and `node_issue_scope` are NOT emitted by
/// `paper_contract_payload` (strategy (a): don't construct what we'd just
/// scrub), so they need no entry here.
fn paper_prompt_facing_view(payload: &Value) -> Value {
    let mut view = payload.clone();
    if let Some(map) = view.as_object_mut() {
        for field in PROMPT_FACING_VERIFIER_HOUSEKEEPING_DROP {
            map.remove(*field);
        }
        for field in ["target_covering_nodes", "node_paper_basis_inputs"] {
            map.remove(field);
        }
    }
    drop_null_keys(view)
}

/// Recursively drop `null` map entries from a JSON value, mirroring
/// `bridge_prompts._drop_null_keys`. Used by `prompt_facing_view` builders
/// to keep the inline prompt contract JSON free of `"key": null` noise
/// without affecting the on-disk structured request.
fn drop_null_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter_map(|(k, v)| {
                    if v.is_null() {
                        None
                    } else {
                        Some((k, drop_null_keys(v)))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(drop_null_keys).collect()),
        other => other,
    }
}

fn artifact_prompt_view_payload() -> Value {
    json!({
        "raw_output_format": "json_only",
        "escape_json_backslashes": true,
        "done_marker_contract": "write_done_after_json_check_passes",
        "checker_authority": "exact_command_is_authoritative",
        "json_check_command_template": [],
        "acceptance_check_command_template": [],
        "failure_recovery": "json_check_required_acceptance_check_best_effort",
        "stdout_policy": "do_not_print_json_to_stdout",
    })
}

fn verifier_common_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    // B6: route verifiers to the trim verifier-only scheme reference.
    vec![
        verifier_scheme_fragment_path(request, target),
        "verifier/common/00_intro.md",
    ]
}

fn verifier_shared_prompt_fragments(target: crate::backend::BackendId) -> Vec<&'static str> {
    vec![
        "shared/10_repository_root.md",
        "verifier/common/10_lane_id.md",
        "verifier/common/15_previous_findings.md",
        // GAP 2: the read-files + FILESPEC fragments are Lean-bearing
        // (`-- BODY`, `theorem Foo := by`, `law1 := by sorry`); on an isabelle
        // run route to the `_isabelle` siblings so the verifier reads .thy
        // structure, not Lean. Lean arg unchanged ⇒ Lean prompts byte-identical.
        isa_or_lean(
            target,
            "shared/20_read_files.md",
            "shared/20_read_files_isabelle.md",
        ),
        isa_or_lean(
            target,
            "shared/25_filespec.md",
            "shared/25_filespec_isabelle.md",
        ),
        "shared/30_project_invariants.md",
    ]
}

/// Always-last prompt fragment that points the agent at the on-disk
/// structured request file the bridge writes alongside the raw output
/// path. Every kernel-authored fragment list ends with this entry;
/// keeping it kernel-side eliminates the matching bridge-side append
/// (`_augment_fragments_with_request_pointer`).
const STRUCTURED_REQUEST_POINTER_FRAGMENT: &str = "shared/91_structured_request_pointer.md";

/// Always-last structured-request pointer fragment, PV-substitutive. The
/// all-math variant teaches a Paper-lane schema map + `jq '.paper_verify_targets'`
/// recipe that has no live lane in PV; the PV variant drops the paper rows/jq
/// and relabels the substantiveness frontier. Gated on `is_pv` ⇒ all-math byte
/// identical.
fn structured_request_pointer_fragment(request: &WrapperRequest) -> &'static str {
    if request.is_pv {
        "pv/shared/91_structured_request_pointer.md"
    } else {
        STRUCTURED_REQUEST_POINTER_FRAGMENT
    }
}

/// Stuck-audit scratch fragment, PV-substitutive. The all-math variant lists a
/// `paper/` avoid-dir absent in PV and hands a host `lake env` probe recipe that
/// violates the no-host-lake invariant; the PV variant drops both. Gated on
/// `is_pv` ⇒ all-math byte identical.
fn stuck_audit_scratchpad_fragment(request: &WrapperRequest) -> &'static str {
    if request.is_pv {
        "pv/stuck_audit/04_scratchpad.md"
    } else {
        "stuck_math_audit/common/04_scratchpad.md"
    }
}

/// Stuck-audit history-access fragment, PV-substitutive. The all-math variant
/// prefers "the paper" on disagreement; the PV variant re-points to GOAL.md +
/// crate + pinned model. Gated on `is_pv`.
fn stuck_audit_history_access_fragment(request: &WrapperRequest) -> &'static str {
    if request.is_pv {
        "pv/stuck_audit/03_history_access.md"
    } else {
        "stuck_math_audit/common/03_history_access.md"
    }
}

/// Stuck-audit output-contract fragment, PV-substitutive. The all-math variant
/// asks for "paper citations" and a "paper-faithful" strategy; the PV variant
/// re-points to crate / GOAL.md / pinned-model citations. Gated on `is_pv`.
fn stuck_audit_output_contract_fragment(request: &WrapperRequest) -> &'static str {
    if request.is_pv && request.trust_base_required_v1 {
        // Trust protocol v1 closes the assumption-authoring lane; its output
        // contract routes a dropped guarantee to the legal trust-mode menu
        // instead of the retired pinned-goal candidate-field instruction.
        "pv/stuck_audit/05_output_contract_trust_v1.md"
    } else if request.is_pv {
        "pv/stuck_audit/05_output_contract.md"
    } else {
        "stuck_math_audit/common/05_output_contract.md"
    }
}

fn has_blocker_kind(request: &WrapperRequest, kind: BlockerKind) -> bool {
    request.blockers.iter().any(|blocker| blocker.kind == kind)
}

fn task_mode_snake(mode: &TaskMode) -> &'static str {
    match mode {
        TaskMode::Global => "global",
        TaskMode::Targeted => "targeted",
        TaskMode::Local => "local",
        TaskMode::Restructure => "restructure",
        TaskMode::CoarseRestructure => "coarse_restructure",
        TaskMode::Cleanup => "cleanup",
    }
}

fn reset_choice_snake(reset: &crate::ResetChoice) -> &'static str {
    use crate::ResetChoice::*;
    match reset {
        None => "none",
        LastCommit => "last_commit",
        LastClean => "last_clean",
        TheoremStatingNode => "theorem_stating_node",
    }
}

fn review_decision_snake(decision: &crate::model::ReviewDecisionKind) -> &'static str {
    use crate::model::ReviewDecisionKind::*;
    match decision {
        Continue => "continue",
        AdvancePhase => "advance_phase",
        NeedInput => "need_input",
        Done => "done",
    }
}

/// Mirrors `WrapperRequest::review_response_audit_plan_rejection_reason`
/// in model.rs:3087-3115. Reviewers may dismiss audit-plan tasks only when
/// (a) an audit plan exists, (b) StuckMathAudit is active, and
/// (c) the phase admits dismissal (ProofFormalization / TheoremStating /
/// any need_input_audit plan).
fn review_audit_dismissal_legal(request: &WrapperRequest) -> bool {
    let Some(plan) = request.audit_plan.as_ref() else {
        return false;
    };
    if !request.stuck_math_audit.active {
        return false;
    }
    matches!(
        request.phase,
        Phase::ProofFormalization | Phase::TheoremStating | Phase::RevisionStating
    ) || plan.need_input_audit
}

fn nonempty_decision_set<'a, I>(values: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = &'a str>,
{
    values
        .into_iter()
        .filter_map(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_ascii_uppercase())
        })
        .collect()
}

fn paper_review_is_split(request: &WrapperRequest) -> bool {
    nonempty_decision_set(
        request
            .review_verifier_evidence
            .paper
            .values()
            .map(|lane| lane.paper_faithfulness.decision.as_str()),
    )
    .len()
        > 1
}

fn corr_review_is_split(request: &WrapperRequest) -> bool {
    // Corr evidence is now nested by node then lane (mirroring sound); split
    // detection still asks "did any pair of lane verdicts disagree anywhere
    // in this cycle's evidence?".
    nonempty_decision_set(
        request
            .review_verifier_evidence
            .corr
            .values()
            .flat_map(|by_lane| by_lane.values())
            .map(|lane| lane.correspondence.decision.as_str()),
    )
    .len()
        > 1
}

fn sound_review_is_split(request: &WrapperRequest) -> bool {
    // Audit Finding 3: sound evidence is now nested by node then lane;
    // split detection still asks "did any pair of lane verdicts disagree
    // anywhere in this cycle's accumulated evidence?".
    nonempty_decision_set(
        request
            .review_verifier_evidence
            .sound
            .values()
            .flat_map(|by_lane| by_lane.values())
            .map(|lane| lane.soundness.decision.as_str()),
    )
    .len()
        > 1
}

fn has_deterministic_worker_rejection_reasons(request: &WrapperRequest) -> bool {
    !request.deterministic_worker_rejection_reasons.is_empty()
}

fn has_review_verifier_evidence(request: &WrapperRequest) -> bool {
    !request.review_verifier_evidence.paper.is_empty()
        || !request.review_verifier_evidence.deviation.is_empty()
        || !request.review_verifier_evidence.substantiveness.is_empty()
        || !request.review_verifier_evidence.corr.is_empty()
        || !request.review_verifier_evidence.sound.is_empty()
}

fn paper_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    if request.deviation_verify_id.is_some() {
        // Stage 7 (plan doc 32, N3): a trust-run PV deviation is a Lane-1
        // seam repair, judged against the seam-repair rubric; math keeps
        // the paper-deviation scenario byte-identical.
        return if request.is_pv && request.trust_base_required_v1 {
            vec!["pv/verifier/deviation/05_seam_repair_trust_v1.md"]
        } else {
            vec!["verifier/deviation/05_single_file.md"]
        };
    }
    // Per-node scenario: `substantiveness_verify_nodes` non-empty AND
    // `paper_verify_targets` empty (kernel cycle scheduler enforces
    // exactly one frontier active per request). Fall through to the
    // target-package scenario otherwise.
    if !request.substantiveness_verify_nodes.is_empty() && request.paper_verify_targets.is_empty() {
        // PV substitutive: the fresh-frontier scenario fragment carries the
        // "tex paper being formalized" referent; the PV variant re-points it to
        // GOAL.md + the byte-pinned model and the "another Paper request"
        // re-issue phrasing to "another substantiveness request". The revisit
        // variant is PV-neutral (it only references the node's own `.tex`).
        let mut fragments = if request.previous_substantiveness_lane_findings.is_empty() {
            if request.is_pv {
                vec!["pv/verifier/substantiveness/05_fresh_node_frontier.md"]
            } else {
                vec!["verifier/substantiveness/05_fresh_node_frontier.md"]
            }
        } else if request.is_pv {
            vec!["pv/verifier/substantiveness/05_revisit_node_frontier.md"]
        } else {
            vec!["verifier/substantiveness/05_revisit_node_frontier.md"]
        };
        if request
            .substantiveness_verify_nodes
            .iter()
            .any(|n| n.as_str() == "Preamble")
        {
            // PV substitutive: the preamble note frames each item against the
            // paper; the PV variant frames it against the function's intended
            // property + the pinned model.
            if request.is_pv {
                fragments.push("pv/verifier/substantiveness/06_with_preamble.md");
            } else {
                fragments.push("verifier/substantiveness/06_with_preamble.md");
            }
        }
        // PV Phase 7 — Slice 2: a Spec node would get the vacuous/weakened-spec
        // rubric (Spec-Critic).
        //
        // RETIRED BY DECISION (D5) — not by unreachability. Since the D6
        // waiver re-key, a worker-authored statement node (prose mode,
        // `StatementProvenance::WorkerAuthored`) is NOT
        // `substantiveness_waived` and genuinely reaches the
        // substantiveness frontier; the old "Spec role is only ever
        // assigned to pinned challenge targets, so this guard can never be
        // true" justification no longer holds. Per D5 the Spec-Critic
        // fragment stays deleted regardless: authored statements are judged
        // by the standard substantiveness rubric, and the admits-extra-
        // models residue is substantiveness + gate territory, not a
        // dedicated critic's. The condition below stays forced false so it
        // can never push.
        if false
            && verify_set_has_pv_role(request, &request.substantiveness_verify_nodes, PvRole::Spec)
        {
            // fragments.push("pv/verifier/substantiveness/16_spec_critic.md");
        }
        // PV Phase 7 — Slice 3: an ExternalModel node would get the
        // External-Model-Critic rubric (a hand-supplied model declaration judged
        // for faithful, non-vacuous modeling).
        //
        // DORMANT — `PvRole::ExternalModel` is never assigned in production (no
        // config section, no role string; only tests construct it), so this
        // guard can never be true and
        // `pv/verifier/substantiveness/17_external_model_critic.md` is deleted.
        // Re-enabling requires wiring the role + this push + the per-node
        // APPROVED_AXIOMS.json widening + the External-Model-Critic verification
        // TOGETHER (an ExternalModel's axioms join the trusted axiom surface, so
        // they must not enter unverified). Kept as documented dead code; the
        // condition below is forced false so it can never push.
        if false
            && verify_set_has_pv_role(
                request,
                &request.substantiveness_verify_nodes,
                PvRole::ExternalModel,
            )
        {
            // fragments.push("pv/verifier/substantiveness/17_external_model_critic.md");
        }
        // PV substitutive: the deviation lane is dropped (byte-pinned model)
        // — except on trust-required runs, where Stage 7 re-instantiates it
        // as the seam-repair / claim-B lane (plan doc 32, N3).  The math
        // branch is byte-identical.
        if !request.is_pv {
            fragments.push("verifier/substantiveness/15_deviations.md");
        } else if request.trust_base_required_v1 {
            fragments.push("pv/verifier/substantiveness/15_deviations_trust_v1.md");
        }
        // Reference papers: pushed only when a FRONTIER node actually
        // claims one, so claim-free substantiveness prompts (including
        // every empty-registry run) are byte-identical.
        if reference_papers_fragments_active(request)
            && request
                .substantiveness_verify_nodes
                .iter()
                .any(|node| {
                    request
                        .node_reference_grounds
                        .get(node)
                        .is_some_and(|claims| !claims.is_empty())
                })
        {
            fragments.push("verifier/substantiveness/16_reference_grounds.md");
        }
        return fragments;
    }
    // Faithfulness target-package fall-through. In PV the referent is
    // `GOAL.md`, not a paper, so PV gets its own goal-faithfulness fragments.
    // The `else` arm is the
    // byte-identical all-math math-paper selection.
    if request.previous_paper_lane_findings.is_empty() {
        if request.is_pv {
            vec!["pv/verifier/paper_faithfulness/05_fresh_target_package.md"]
        } else {
            vec!["verifier/paper_faithfulness/05_fresh_target_package.md"]
        }
    } else if request.is_pv {
        vec!["pv/verifier/paper_faithfulness/05_revisit_target_package.md"]
    } else {
        vec!["verifier/paper_faithfulness/05_revisit_target_package.md"]
    }
}

/// PV Phase 7: true iff any node in `nodes` carries `role` on the
/// request's `node_role` map. Empty for all-math / non-PV tablets ⇒ always
/// false ⇒ the PV fragment pushes below are no-ops and the contract bytes
/// are identical. The full role map stays OFF the verifier wire: the KERNEL
/// reads it here to select additional verifier contract surfaces.
fn verify_set_has_pv_role<'a>(
    request: &WrapperRequest,
    nodes: impl IntoIterator<Item = &'a NodeId>,
    role: PvRole,
) -> bool {
    nodes
        .into_iter()
        .any(|node| request.node_role.get(node) == Some(&role))
}

fn pv_under_model_assumptions_corr_nodes(request: &WrapperRequest) -> Vec<NodeId> {
    if !request.is_pv {
        return Vec::new();
    }
    request
        .verify_nodes
        .iter()
        .filter(|node| request.node_role.get(*node) == Some(&PvRole::UnderModelAssumptions))
        .cloned()
        .collect()
}

/// PV Phase 8 monotonicity gate prose, the human-facing
/// strengthening-vs-weakening framing for the ProtectedReapproval gate.
/// `include_str!` keeps `pv/human_gate/05_pv_monotonicity.md` the single
/// versioned source of truth: the kernel renders that file's bytes verbatim
/// into the gate's `request_summary` surface (the slot that otherwise carries
/// the one-line `protected_reapproval_status`). Inert for all-math — the gate
/// block only fires when `protected_reapproval_nodes` is non-empty.
const PV_MONOTONICITY_GATE_PROSE: &str =
    include_str!("../../trellis/prompt_fragments/pv/human_gate/05_pv_monotonicity.md");

/// Render the monotonicity gate's human-facing review text: the versioned
/// monotonicity prose followed by the per-node `diff_corr_fingerprint_axes`
/// bullets for each reopened PV spec node. Falls back to the one-line status
/// when no fingerprint pairs were carried (the legacy / fail-closed reopen
/// paths surface the node list without an axis diff).
fn pv_monotonicity_gate_status(request: &WrapperRequest) -> String {
    let mut text = PV_MONOTONICITY_GATE_PROSE.trim_end().to_owned();
    text.push_str(
        "\n\nThis stays pending human reapproval after normal verifier blockers drain; it is \
         not a blocker-action item.",
    );
    for node in &request.protected_reapproval_nodes {
        let Some(pair) = request.protected_reapproval_corr_fingerprint_pairs.get(node) else {
            continue;
        };
        let bullets = crate::runtime_cli_observations::diff_corr_fingerprint_axes(
            &pair.approved,
            &pair.current,
        );
        text.push_str(&format!("\n\n{node}:"));
        if bullets.is_empty() {
            text.push_str(
                "\n  - (fingerprint differs but no specific axis could be identified)",
            );
        } else {
            for bullet in bullets {
                text.push_str(&format!("\n  - {bullet}"));
            }
        }
    }
    text
}

fn corr_scenario_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    if request.conditional_theorem_correspondence.is_some() {
        return vec!["pv/verifier/correspondence/11_conditional_theorem.md"];
    }
    if request.rust_witness_artifact_correspondence.is_some() {
        return vec!["pv/verifier/correspondence/10_rust_witness_artifact.md"];
    }
    // PV substitutive: the fresh-frontier corr fragment carries the
    // "not the paper-faithfulness task" framing; the PV variant re-points it to
    // "not the substantiveness task". The revisit variant is PV-neutral.
    let mut fragments = if request.previous_corr_lane_findings.is_empty() {
        if request.is_pv {
            vec!["pv/verifier/correspondence/05_frontier.md"]
        } else {
            vec!["verifier/correspondence/05_frontier.md"]
        }
    } else {
        vec!["verifier/correspondence/05_revisit_frontier.md"]
    };
    if request.corr_verify_nodes.contains("Preamble") || request.verify_nodes.contains("Preamble") {
        fragments.push("verifier/correspondence/06_with_preamble.md");
    }
    // D7 merge: ONE unconditional PV correspondence rubric, carried on every
    // PV corr contract (the 05_frontier/07_scratchpad discipline). It
    // replaces the two role-gated fragments (09_spec_correspondence for
    // Spec, 10_model_correspondence for Correctness/Safety): the role
    // gating was a routing defect — the Correctness/Safety fragment fired
    // only when such a node reached the corr frontier, which never happened
    // on the live run, and the Spec fragment's closing deferral pointed at
    // a fragment possibly absent from the prompt. The merged rubric is
    // role-independent: it teaches the NL-about-Rust vs Lean-about-model
    // vocabulary gap itself, with the panic-freedom and model-type-domain-
    // width cases as instances.
    if request.is_pv {
        fragments.push("pv/verifier/correspondence/09_pv_correspondence.md");
    }
    // PV Phase 7 — Slice 3: an ExternalModel node is a hand-supplied model
    // declaration (no extraction byte-pin); correspondence would read it against
    // its own NL gloss under the External-Model-Critic rubric.
    //
    // DORMANT — `PvRole::ExternalModel` is never assigned in production (no
    // config section, no role string; only tests construct it), so this guard
    // can never be true and `pv/verifier/correspondence/11_external_model_critic.md`
    // is deleted. Re-enabling requires wiring the role + this push + the per-node
    // APPROVED_AXIOMS.json widening + the External-Model-Critic verification
    // TOGETHER (an ExternalModel's axioms join the trusted axiom surface, so they
    // must not enter unverified). Kept as documented dead code; the condition
    // below is forced false so it can never push.
    if false && verify_set_has_pv_role(request, &request.verify_nodes, PvRole::ExternalModel) {
        // fragments.push("pv/verifier/correspondence/11_external_model_critic.md");
    }
    // PV substitutive: the all-math corr scratch fragment hands a host
    // `cd {{repo_path}}` + `lake env lean` probe recipe that violates the
    // no-host-lake invariant; the PV variant drops the host-lake invocation.
    //
    // The scratchpad fragment is an operative instruction: it tells the corr
    // verifier to run a probe in the request-local scratch dir. The Lean arm
    // drives `lake env lean …probe.lean`; on isabelle_hol there is no lake, so
    // route to the `_isabelle` sibling (scratch `.thy` checked via `isabelle`).
    // PV is Lean-only, so the two branches never overlap.
    fragments.push(if request.is_pv {
        "pv/verifier/correspondence/07_scratchpad.md"
    } else {
        isa_or_lean(
            target,
            "verifier/correspondence/07_scratchpad.md",
            "verifier/correspondence/07_scratchpad_isabelle.md",
        )
    });
    fragments
}

fn sound_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = vec![match request.phase {
        Phase::TheoremStating | Phase::RevisionStating => {
            "verifier/soundness/05_theorem_target.md"
        }
        Phase::ProofFormalization | Phase::Cleanup | Phase::Complete => {
            "verifier/soundness/05_proof_node.md"
        }
    }];
    if !request.previous_sound_lane_findings.is_empty() {
        fragments.push("verifier/soundness/06_revisit_target.md");
    }
    // PV substitutive: the missing soundness lane — the detail floor is what the
    // Lean formalization needs; cited support includes the pinned ExtractionModel
    // defs + Spec nodes. No-op for all-math (is_pv false).
    if request.is_pv {
        fragments.push("pv/verifier/soundness/05_pv_floor.md");
    }
    fragments
}

fn paper_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    let mut fragments = verifier_common_prompt_fragments(request, target);
    fragments.extend(paper_scenario_prompt_fragments(request));
    fragments.extend(verifier_shared_prompt_fragments(target));
    let is_per_node_scenario =
        !request.substantiveness_verify_nodes.is_empty() && request.paper_verify_targets.is_empty();
    let is_deviation_scenario = request.deviation_verify_id.is_some();
    if is_deviation_scenario {
        if request.is_pv && request.trust_base_required_v1 {
            // Stage 7 (N3): the seam-repair authorization surface — the
            // deviation-under-review block is scenario-neutral and shared;
            // the contract + rubric are the trust-run seam-repair ones.
            fragments.extend([
                "verifier/deviation/20_file.md",
                "pv/verifier/deviation/30_contract_trust_v1.md",
                "shared/90_artifact_delivery.md",
                "pv/canonical/DEVIATIONS_trust_v1.md",
            ]);
        } else {
            fragments.extend([
                "verifier/deviation/20_file.md",
                "verifier/deviation/30_contract.md",
                "shared/90_artifact_delivery.md",
                "canonical/DEVIATIONS.md",
            ]);
        }
    } else if is_per_node_scenario {
        // PV substitutive: judge the node's `.tex` spec against the function's
        // intended property + the byte-pinned model (no paper); PV substantiveness
        // def.
        let (authority_fragment, substantiveness_def) = if request.is_pv {
            (
                "pv/verifier/substantiveness/50_authority_prose.md",
                "pv/canonical/SUBSTANTIVENESS.md",
            )
        } else {
            (
                "verifier/substantiveness/50_authority.md",
                "canonical/SUBSTANTIVENESS.md",
            )
        };
        // PV substitutive: the node-frontier inputs fragment carries the
        // "substantiveness paper-basis inputs" header and the
        // "immediately-preceding Paper request" phrasing; the PV variant
        // re-words both (the `{{node_paper_basis_inputs_json}}` placeholder name
        // is a schema/mechanism token and is preserved).
        let node_frontier_fragment = if request.is_pv {
            "pv/verifier/substantiveness/20_node_frontier.md"
        } else {
            "verifier/substantiveness/20_node_frontier.md"
        };
        let prose_false_outcome = request.is_pv;
        let contract_fragment = if prose_false_outcome {
            "pv/verifier/substantiveness/30_contract.md"
        } else {
            "verifier/substantiveness/30_contract.md"
        };
        let rubric_fragment = if prose_false_outcome {
            "pv/verifier/substantiveness/40_rubric.md"
        } else {
            "verifier/substantiveness/40_rubric.md"
        };
        fragments.extend([
            node_frontier_fragment,
            contract_fragment,
            rubric_fragment,
            authority_fragment,
            "shared/90_artifact_delivery.md",
            substantiveness_def,
        ]);
        if request.is_pv {
            // PV: the model is inviolate ground truth; the verifier judges every
            // node against its pinned identity (slice ∪ namespace context).
            fragments.push("pv/canonical/EXTRACTION_MODEL.md");
        }
    } else if request.is_pv {
        // PV prose goals: a goal-faithfulness target package against
        // `GOAL.md` (not a paper). The referent fragments and canonical are
        // the PV variants.
        fragments.extend([
            "pv/verifier/paper_faithfulness/20_targets.md",
            "pv/verifier/paper_faithfulness/30_contract.md",
            "pv/verifier/paper_faithfulness/40_rubric.md",
            "pv/verifier/paper_faithfulness/50_authority.md",
            "shared/90_artifact_delivery.md",
            "pv/canonical/FAITHFULNESS.md",
        ]);
    } else {
        fragments.extend([
            "verifier/paper_faithfulness/20_targets.md",
            "verifier/paper_faithfulness/30_contract.md",
            "verifier/paper_faithfulness/40_rubric.md",
            "verifier/paper_faithfulness/50_authority.md",
            "shared/90_artifact_delivery.md",
            "canonical/FAITHFULNESS.md",
        ]);
    }
    fragments.push(structured_request_pointer_fragment(request));
    fragments
}

fn paper_target_covering_nodes(request: &WrapperRequest) -> BTreeMap<TargetId, BTreeSet<NodeId>> {
    request
        .paper_verify_targets
        .iter()
        .cloned()
        .map(|target| {
            let covering_nodes = request
                .current_target_claims
                .iter()
                .filter(|(node, claims)| {
                    request.current_present_nodes.contains(*node) && claims.contains(&target)
                })
                .map(|(node, _)| node.clone())
                .collect();
            (target, covering_nodes)
        })
        .collect()
}

fn correspondence_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    let mut fragments = verifier_common_prompt_fragments(request, target);
    fragments.extend(corr_scenario_prompt_fragments(request, target));
    // Ground-truth sentence for challenge-covering nodes. The lane is
    // otherwise unchanged: the registry never reaches verifier requests. The
    // correspondence anchor is the node's NL gloss (and, for PV nodes, the
    // bound ExtractionModel declaration via the role overlays above), not a
    // paper.
    if request.challenge_targets_configured {
        fragments.push("verifier/correspondence/08_challenge_covering_nodes.md");
    }
    fragments.extend(verifier_shared_prompt_fragments(target));
    fragments.extend([
        "verifier/correspondence/20_frontier.md",
        "verifier/correspondence/30_contract.md",
        "verifier/correspondence/40_rubric.md",
        "verifier/correspondence/50_authority.md",
        "shared/90_artifact_delivery.md",
        // CORRESPONDENCE is inlined for the verifier on BOTH backends (not
        // routed through `isa_or_lean` elsewhere), so the isabelle variant
        // neutralizes the Lean phrasing while the Lean branch stays verbatim.
        isa_or_lean(
            target,
            "canonical/CORRESPONDENCE.md",
            "canonical/CORRESPONDENCE_isabelle.md",
        ),
    ]);
    if request.is_pv {
        // PV: the model is inviolate ground truth; the correspondence subject
        // resolves to the model's pinned identity (slice ∪ namespace context).
        fragments.push("pv/canonical/EXTRACTION_MODEL.md");
    }
    fragments.push(structured_request_pointer_fragment(request));
    fragments
}

fn soundness_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    let mut fragments = verifier_common_prompt_fragments(request, target);
    fragments.extend(sound_scenario_prompt_fragments(request));
    fragments.extend(verifier_shared_prompt_fragments(target));
    // Re-verification context fragment: included only when the kernel
    // has computed per-target dep-drift / own-tex-drift context for
    // this Sound request (i.e. the target was previously approved and
    // is now being re-verified because of a fingerprint change). The
    // bridge provides the `reverification_context_json` placeholder
    // only when this fragment is present, so unconditional inclusion
    // would break prompt rendering for fresh-Unknown targets.
    if request.sound_reverification_context.is_some() {
        fragments.push("verifier/common/15a_reverification_context.md");
    }
    // Process memory: bridge-rendered block (Sound verifier reads the
    // settled constraints / refuted routes; no challenge channel on
    // verifier lanes). Renders empty (and drops out of the prompt) on
    // runs without a `process-memory/` directory.
    fragments.push("verifier/common/16_process_memory.md");
    // PV substitutive: the soundness detail-floor def is the PV variant. Mode B
    // (Lean goals): the GOAL.md bounds become the pinned goal bounds.
    // SOUNDNESS is inlined for the verifier on BOTH backends; the isabelle
    // variant drops the "in Lean" tail, the Lean branch is verbatim.
    let soundness_def = if request.is_pv {
        "pv/canonical/SOUNDNESS.md"
    } else {
        isa_or_lean(
            target,
            "canonical/SOUNDNESS.md",
            "canonical/SOUNDNESS_isabelle.md",
        )
    };
    fragments.extend([
        "verifier/soundness/20_target.md",
        "verifier/soundness/30_contract.md",
        "verifier/soundness/40_rubric.md",
        "verifier/soundness/50_authority.md",
        "shared/90_artifact_delivery.md",
        soundness_def,
    ]);
    if request.is_pv {
        // PV: the model is inviolate ground truth; soundness reasons from the
        // model's pinned identity (slice ∪ namespace context) as ground facts.
        fragments.push("pv/canonical/EXTRACTION_MODEL.md");
    }
    fragments.push(structured_request_pointer_fragment(request));
    fragments
}

fn worker_intro_fragment(request: &WrapperRequest) -> &'static str {
    match request.worker_context.worker_profile {
        crate::model::WorkerProfile::Theorem => "worker/theorem_stating/00_intro.md",
        crate::model::WorkerProfile::ProofEasy | crate::model::WorkerProfile::ProofHard => {
            "worker/proof_formalization/00_intro.md"
        }
        crate::model::WorkerProfile::Cleanup => "worker/cleanup/00_intro.md",
        crate::model::WorkerProfile::FinalCleanup => "worker/final_cleanup/00_intro.md",
        crate::model::WorkerProfile::None => "worker/generic/00_intro.md",
    }
}

fn theorem_worker_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    // PV substitutive: the faithfulness lane is goal-faithfulness against
    // GOAL.md, so the PV variant re-points "paper target" to goal target.
    // Substantiveness and correspondence are live PV lanes and get
    // their PV after-review variants too (the deviation bullet / "paper's proof"
    // wording is dropped / re-pointed).
    if has_blocker_kind(request, BlockerKind::PaperFaithfulness) {
        if request.is_pv {
            vec!["pv/worker/theorem_stating/05_after_paper_faithfulness_review.md"]
        } else {
            vec!["worker/theorem_stating/05_after_paper_faithfulness_review.md"]
        }
    } else if has_blocker_kind(request, BlockerKind::Substantiveness) {
        if request.is_pv {
            vec!["pv/worker/theorem_stating/05_after_substantiveness_review.md"]
        } else {
            vec!["worker/theorem_stating/05_after_substantiveness_review.md"]
        }
    } else if has_blocker_kind(request, BlockerKind::NodeCorr) {
        vec!["worker/theorem_stating/05_after_correspondence_review.md"]
    } else if has_blocker_kind(request, BlockerKind::Soundness) {
        vec!["worker/theorem_stating/05_after_soundness_review.md"]
    } else if request.is_pv {
        vec!["pv/worker/theorem_stating/05_frontier_work.md"]
    } else {
        vec!["worker/theorem_stating/05_frontier_work.md"]
    }
}

fn theorem_worker_first_request_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    if request.id == 1 {
        if request.is_pv {
            vec!["pv/worker/theorem_stating/12_first_request_decomposition.md"]
        } else {
            vec!["worker/theorem_stating/12_first_request_dag_decomposition.md"]
        }
    } else {
        Vec::new()
    }
}

/// Challenge-target framing for workers: how to claim a target and
/// that the covering declaration's text is prescribed verbatim
/// (theorem-stating) / frozen with decomposition beneath it
/// (proof-formalization). Included only when the registry is
/// configured, so paper-only runs see no challenge copy.
fn worker_challenge_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    if request.configured_challenge_targets.is_empty() {
        return Vec::new();
    }
    match request.worker_context.worker_profile {
        WorkerProfile::Theorem if request.is_pv => {
            vec!["pv/worker/theorem_stating/18_targets.md"]
        }
        WorkerProfile::Theorem => vec!["worker/theorem_stating/18_challenge_targets.md"],
        WorkerProfile::ProofEasy | WorkerProfile::ProofHard => {
            vec![isa_or_lean(
                target,
                "worker/proof_formalization/09_challenge_target_frozen.md",
                "worker/proof_formalization/09_challenge_target_frozen_isabelle.md",
            )]
        }
        WorkerProfile::Cleanup | WorkerProfile::FinalCleanup | WorkerProfile::None => Vec::new(),
    }
}

fn proof_worker_scope_prompt_fragment(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> &'static str {
    match request.worker_context.validation_kind {
        WorkerValidationKind::ProofRestructure => isa_or_lean(
            target,
            "worker/proof_formalization/05_scope_restructure.md",
            "worker/proof_formalization/05_scope_restructure_isabelle.md",
        ),
        WorkerValidationKind::ProofCoarseRestructure => {
            "worker/proof_formalization/05_scope_coarse_restructure.md"
        }
        WorkerValidationKind::ProofEasy
        | WorkerValidationKind::ProofLocal
        | WorkerValidationKind::None
        | WorkerValidationKind::TheoremGlobal
        | WorkerValidationKind::TheoremTargeted
        | WorkerValidationKind::TheoremRestructure
        | WorkerValidationKind::Cleanup
        | WorkerValidationKind::FinalCleanup => isa_or_lean(
            target,
            "worker/proof_formalization/05_scope_local.md",
            "worker/proof_formalization/05_scope_local_isabelle.md",
        ),
    }
}

fn proof_worker_gate_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    let mut fragments = vec![
        if request.worker_context.allow_new_obligations {
            isa_or_lean(
                target,
                "worker/proof_formalization/06_gate_allow_new_obligations.md",
                "worker/proof_formalization/06_gate_allow_new_obligations_isabelle.md",
            )
        } else {
            isa_or_lean(
                target,
                "worker/proof_formalization/06_gate_no_new_obligations.md",
                "worker/proof_formalization/06_gate_no_new_obligations_isabelle.md",
            )
        },
        if request.worker_context.must_close_active {
            isa_or_lean(
                target,
                "worker/proof_formalization/07_gate_must_close_active.md",
                "worker/proof_formalization/07_gate_must_close_active_isabelle.md",
            )
        } else {
            isa_or_lean(
                target,
                "worker/proof_formalization/07_gate_active_may_remain_open.md",
                "worker/proof_formalization/07_gate_active_may_remain_open_isabelle.md",
            )
        },
    ];
    // Proposal v32: surface the active-coarse-anchor framing whenever
    // an anchor is set (which itself implies `coarse_dag_nodes` is
    // non-empty since the kernel only seeds the field then). The
    // fragment is identity-only when `coarse_repair_mode` is false;
    // it switches its framing in the repair-mode branch via the
    // prompt-side request fields.
    if request.active_coarse_node.is_some() {
        fragments.push("worker/proof_formalization/08_coarse_anchor.md");
    }
    fragments
}

fn proof_worker_scenario_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    // Substantiveness blockers in proof-formalization arise from
    // helper nodes added during proof-formalization. Include the same
    // substantiveness-after-review fragment as theorem-stating; the
    // rubric and remediation are phase-neutral.
    //
    // NodeCorr blockers in proof-formalization arise from helper nodes
    // whose Lean signature drifted from the TeX statement during helper
    // authoring. The proof-formalization-specific fragment frames repair
    // as helper-node alignment (rather than principal-statement repair,
    // which is the theorem-stating wording).
    //
    // Soundness blockers in proof-formalization are NL-soundness repair
    // tasks selected by the reviewer/kernel. Triggered on
    // `BlockerKind::Soundness` in `request.blockers` rather than on
    // editable scope, because authorized-nodes is broader than assigned
    // task intent. Substantiveness, correspondence, and soundness
    // fragments compose additively when multiple blockers are present.
    let mut fragments = Vec::new();
    if has_blocker_kind(request, BlockerKind::Substantiveness) {
        // PV substitutive: the after-substantiveness fragment re-points "the
        // paper's proof" → the function's intended behavior over the pinned
        // model and drops the deviation bullet (no Deviation lane in PV) —
        // except on trust-required runs, whose Stage-7 variant restores the
        // rejected-claim bullet for the re-enabled lane.
        fragments.push(if request.is_pv && request.trust_base_required_v1 {
            "pv/worker/theorem_stating/05_after_substantiveness_review_trust_v1.md"
        } else if request.is_pv {
            "pv/worker/theorem_stating/05_after_substantiveness_review.md"
        } else {
            "worker/theorem_stating/05_after_substantiveness_review.md"
        });
    }
    if request.phase == Phase::ProofFormalization
        && has_blocker_kind(request, BlockerKind::NodeCorr)
    {
        // PV substitutive: the after-correspondence fragment re-points the
        // "nodes covering paper targets / paper-faithful" carve-out to
        // goal-covering nodes / goal-faithful over the pinned model.
        fragments.push(if request.is_pv {
            "pv/worker/proof_formalization/05_after_correspondence_review.md"
        } else {
            "worker/proof_formalization/05_after_correspondence_review.md"
        });
    }
    if has_blocker_kind(request, BlockerKind::Soundness) {
        fragments.push(isa_or_lean(
            target,
            "worker/proof_formalization/05_after_soundness_review.md",
            "worker/proof_formalization/05_after_soundness_review_isabelle.md",
        ));
    }
    fragments.push(proof_worker_scope_prompt_fragment(request, target));
    fragments.extend(proof_worker_gate_prompt_fragments(request, target));
    fragments
}

/// PV worker critics, keyed on the ACTIVE node's `node_role`. Unlike the
/// verifier side (which keys on "any node in the review SET has role R"), the
/// worker authors exactly ONE active node, so selection reads
/// `request.node_role.get(active_node)`. Non-PV requests return before any
/// selector runs; a PV request with `active_node` unset or absent from
/// `node_role` receives no role fragment. The role stays a kernel-only read
/// here; it never reaches the worker wire.
///
/// `00_intro` orients any PV active role. `Spec` (authored in theorem-stating)
/// pulls the spec-authoring critic; `Safety` / `Invariant` / `Correctness`
/// (authored in proof-formalization) pull their role critic plus the shared
/// proof-method strategy. `ExtractionModel` (byte-pinned), `ExternalModel`
/// (verifier-critic surface), and `LibraryLemma` (standard lanes) carry no
/// worker overlay.
/// True iff the active node is the refutation (`__Refutation`) node of a
/// configured `Decide` pair — i.e. the worker is on the Disprove side. The
/// refutation node only enters the live worker frontier under Disprove
/// polarity, so its being active is the precise polarity signal (no
/// `pv_live_polarity` field needed on the request). False for non-Decide runs.
fn is_decide_refutation_node_active(request: &WrapperRequest) -> bool {
    let Some(active) = request.active_node.as_ref() else {
        return false;
    };
    let active = active.as_str();
    active.ends_with(crate::model::REFUTATION_NAME_SUFFIX)
        && request.configured_challenge_targets.values().any(|spec| {
            spec.kind == crate::model::ChallengeTargetKind::Theorem
                && spec.resolution == crate::model::ChallengeResolution::Decide
                && crate::model::refutation_node_name(&spec.name).as_str() == active
        })
}

fn active_disprove_primary(
    request: &WrapperRequest,
) -> Option<&crate::model::ChallengeTargetId> {
    let active = request.active_node.as_ref()?;
    request
        .configured_challenge_targets
        .iter()
        .find(|(target, spec)| {
            spec.kind == crate::model::ChallengeTargetKind::Theorem
                && spec.resolution == crate::model::ChallengeResolution::Decide
                && crate::model::refutation_node_name(&spec.name) == active.as_str()
                && request.pv_live_polarity.get(*target)
                    == Some(&crate::model::ChallengePolarity::Disprove)
        })
        .map(|(target, _)| target)
}

/// W6 (D1): a decide target the audit lane can ACT on: an authored primary
/// whose statement has BOUND (`lean` non-empty post-acceptance). An unbound
/// primary is a decide target with nothing to decide yet: advertising
/// `set_live_polarity`/witness/give-up for it would burn audit rounds on
/// a guaranteed `statement_unbound` bounce.
fn has_actionable_decide_target(request: &WrapperRequest) -> bool {
    request.configured_challenge_targets.values().any(|spec| {
        spec.kind == crate::model::ChallengeTargetKind::Theorem
            && spec.resolution == crate::model::ChallengeResolution::Decide
            && (spec.statement_provenance
                != crate::model::StatementProvenance::WorkerAuthored
                || !spec.lean.trim().is_empty())
    })
}

fn conditional_worker_carrier_available(request: &WrapperRequest) -> bool {
    request.is_pv
        && request.trust_base_required_v1
        && has_actionable_decide_target(request)
        && matches!(
            request.worker_context.worker_profile,
            WorkerProfile::Theorem | WorkerProfile::ProofEasy | WorkerProfile::ProofHard
        )
}

/// The EFFECTIVE live polarity of EVERY configured `Decide` primary:
/// `<primary_target_id> -> "prove" | "disprove"`. An absent
/// `pv_live_polarity` entry is the `Prove` default, materialized here so
/// the reader never has to infer a pair's polarity — in particular NOT
/// from node liveness: a pair's refutation node can be live under `Prove`
/// polarity (e.g. authored directly into Tablet), so "refutation node
/// live ⇒ disprove" is not a valid inference. BTreeMap ⇒ deterministic
/// ordering. Empty object for non-Decide runs (callers gate insertion).
fn decide_live_polarity_json(request: &WrapperRequest) -> Value {
    let polarity: BTreeMap<&crate::model::ChallengeTargetId, crate::model::ChallengePolarity> =
        request
            .configured_challenge_targets
            .iter()
            .filter(|(_, spec)| {
                spec.kind == crate::model::ChallengeTargetKind::Theorem
                    && spec.resolution == crate::model::ChallengeResolution::Decide
            })
            .map(|(id, _)| {
                (
                    id,
                    request.pv_live_polarity.get(id).copied().unwrap_or_default(),
                )
            })
            .collect();
    json!(polarity)
}

fn pv_worker_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    if !request.is_pv {
        return Vec::new();
    }

    // PV under-model (Slice 2): an ASSUMPTION-AUTHORING burst. The auditor ruled
    // `deviation` and named candidate `C`; this worker AUTHORS `C` into
    // Tablet/Assumptions.{lean,tex} under the language-guarantee-only
    // eligibility rule. It is not proving the assumption, but it still edits a
    // real staged node that will pass ordinary correspondence.
    if request.assumption_authoring.is_some() {
        return vec![
            "pv/worker/common/00_intro.md",
            "pv/worker/common/71_author_assumption.md",
        ];
    }
    // Disprove direction: the refutation node mirrors the primary's `PvRole`, so
    // the role match below would otherwise hand the worker the prove-direction
    // overlay. When the active node is the refutation node of a `Decide` pair
    // replace that overlay with the disprove guidance. The refutation node's
    // statement is kernel-pinned (computed ¬T at binding).
    if request.is_pv && is_decide_refutation_node_active(request) {
        let mut fragments = vec!["pv/worker/common/00_intro.md"];
        if request.phase.is_theorem_stating_like() {
            fragments.push(
                "pv/worker/theorem_stating/60_disprove_direction.md",
            );
        } else if request.phase == Phase::ProofFormalization {
            fragments.push(
                "pv/worker/proof_formalization/60_disprove_direction.md",
            );
            if request.trust_base_required_v1
                && request.work_kind == WorkerWorkKind::Standard
                && active_disprove_primary(request).is_some()
            {
                fragments.push("pv/worker/common/24_rust_witness_artifact.md");
            }
        }
        // Stage 7 (N8, C3): refutation work on a trust run carries the
        // side-by-side probe rule.
        if request.trust_base_required_v1 {
            fragments.push("pv/worker/common/72_side_by_side_probes_trust_v1.md");
        }
        return fragments;
    }
    let role = match request
        .active_node
        .as_ref()
        .and_then(|n| request.node_role.get(n))
    {
        Some(role) => *role,
        None => {
            // Stage 7 (N8): the target-false outcome fragment reaches the
            // TheoremStating worker profiles too — TS bursts carry no
            // role-bearing active node, so the tail push below never ran
            // for them.  Same gate as the tail (spec mode + a configured
            // Decide target); trust runs add the C3 probe rule.
            let mut fragments = Vec::new();
            // W6 (D1): the target-false outcome reaches prose workers too,
            // once a decide pair is actionable (bound or pinned). The
            // fragment text is mode-neutral: the byte-pinned model it
            // references is byte-pinned in both PV modes.
            if request.is_pv && has_actionable_decide_target(request) {
                fragments.push(if request.trust_base_required_v1 {
                    "pv/worker/common/71_target_false_trust_v1.md"
                } else {
                    "pv/worker/common/70_target_false_under_model.md"
                });
                if request.trust_base_required_v1 {
                    fragments.push("pv/worker/common/72_side_by_side_probes_trust_v1.md");
                }
            }
            return fragments;
        }
    };
    let mut fragments = vec!["pv/worker/common/00_intro.md"];
    match role {
        PvRole::Spec => {
            if request.phase.is_theorem_stating_like() {
                fragments.push("pv/worker/theorem_stating/10_spec_authoring.md");
            }
        }
        PvRole::Safety => {
            if request.phase == Phase::ProofFormalization {
                fragments.push("pv/worker/proof_formalization/20_safety.md");
                fragments.push("pv/worker/proof_formalization/50_proof_method.md");
            }
        }
        PvRole::Invariant => {
            if request.phase == Phase::ProofFormalization {
                fragments.push("pv/worker/proof_formalization/30_invariant.md");
                fragments.push("pv/worker/proof_formalization/50_proof_method.md");
            }
        }
        PvRole::Correctness => {
            if request.phase == Phase::ProofFormalization {
                fragments.push("pv/worker/proof_formalization/40_correctness.md");
                fragments.push("pv/worker/proof_formalization/50_proof_method.md");
            }
        }
        PvRole::ExtractionModel
        | PvRole::ExternalModel
        | PvRole::LibraryLemma
        | PvRole::UnderModelAssumptions => {}
    }
    // PV under-model (Slice 1): when a `Decide` target is configured and the
    // worker is proving in the PROVE direction (the refutation-node case
    // returned early above), tell it about the `target_false_under_model`
    // outcome — the way to report "T is false under the Aeneas model" without
    // authoring a tablet edit or minting an assumption.
    // W6 (D1): both PV modes, once a decide pair is actionable.
    if request.is_pv && has_actionable_decide_target(request) {
        fragments.push(if request.trust_base_required_v1 {
            "pv/worker/common/71_target_false_trust_v1.md"
        } else {
            "pv/worker/common/70_target_false_under_model.md"
        });
        // Stage 7 (N8, C3): the side-by-side probe rule rides with the
        // trust-run target-false framing.
        if request.trust_base_required_v1 {
            fragments.push("pv/worker/common/72_side_by_side_probes_trust_v1.md");
        }
    }
    fragments
}

fn worker_scenario_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    let mut fragments = pv_worker_prompt_fragments(request);
    fragments.extend(match request.worker_context.worker_profile {
        WorkerProfile::Theorem => theorem_worker_scenario_prompt_fragments(request),
        WorkerProfile::ProofEasy | WorkerProfile::ProofHard => {
            proof_worker_scenario_prompt_fragments(request, target)
        }
        WorkerProfile::Cleanup => vec!["worker/cleanup/05_orphan_cleanup_task.md"],
        // Cleanup-v2 Step 16: branch FinalCleanup task fragment on the
        // active task's kind. Substitution tasks see the substitution-
        // specific fragment (deletion + \noderef sweep + importer
        // rewrites); LintFix tasks see the lintfix fragment
        // (single-node scope). Legacy lint-only mode (no active task)
        // keeps the generic 05_task fragment.
        WorkerProfile::FinalCleanup => {
            // A correspondence-repair burst carries no active task — it is
            // a reviewer dispatch, not audit-proposed work — so
            // `cleanup_active_task_kind_view` is None for it and the
            // task-kind match below would hand it the generic task
            // fragment. Key off the validation plan instead, which is
            // where the repair is actually distinguishable, and do it
            // ahead of the task-kind match since the two are mutually
            // exclusive.
            if request
                .worker_acceptance
                .validation_execution_plan
                .iter()
                .any(|step| {
                    matches!(
                        step,
                        crate::model::WorkerValidationExecutionPlanStep::FinalCleanupCorrRepair {
                            ..
                        }
                    )
                })
            {
                vec![
                    "worker/final_cleanup/05_task.md",
                    "worker/final_cleanup/06_corr_repair_task.md",
                ]
            } else {
                // A batch dispatch carries no active task, so its kind
                // travels in `cleanup_active_batch_kind_view` (the batch is
                // kind-homogeneous by dispatch legality); the single-task
                // path branches on `cleanup_active_task_kind_view`.
                match request
                    .worker_context
                    .cleanup_active_batch_kind_view
                    .as_deref()
                {
                    Some("lint_fix") => vec![
                        "worker/final_cleanup/05_task.md",
                        "worker/final_cleanup/06_lintfix_task.md",
                    ],
                    Some("extract_helper") => vec![
                        "worker/final_cleanup/05_task.md",
                        "worker/final_cleanup/06_extract_helper_task.md",
                    ],
                    _ => match request
                        .worker_context
                        .cleanup_active_task_kind_view
                        .as_ref()
                    {
                        Some(crate::model::CleanupTaskKind::Substitution { .. }) => {
                            vec![
                                "worker/final_cleanup/05_task.md",
                                "worker/final_cleanup/06_substitution_task.md",
                            ]
                        }
                        Some(crate::model::CleanupTaskKind::LintFix { .. }) => {
                            vec![
                                "worker/final_cleanup/05_task.md",
                                "worker/final_cleanup/06_lintfix_task.md",
                            ]
                        }
                        Some(crate::model::CleanupTaskKind::DeadCodeElim { .. }) => {
                            vec![
                                "worker/final_cleanup/05_task.md",
                                "worker/final_cleanup/06_dead_code_task.md",
                            ]
                        }
                        Some(crate::model::CleanupTaskKind::ExtractHelper { .. }) => {
                            vec![
                                "worker/final_cleanup/05_task.md",
                                "worker/final_cleanup/06_extract_helper_task.md",
                            ]
                        }
                        Some(crate::model::CleanupTaskKind::ExtractShared { .. }) => {
                            vec![
                                "worker/final_cleanup/05_task.md",
                                "worker/final_cleanup/06_extract_shared_task.md",
                            ]
                        }
                        None => vec!["worker/final_cleanup/05_task.md"],
                    },
                }
            }
        }
        WorkerProfile::None => vec!["worker/generic/05_task.md"],
    });
    fragments
}

fn worker_profile_guidance_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    match request.worker_context.worker_profile {
        WorkerProfile::Theorem => {
            // PV substitutive: the mode-guidance fragment frames structural work
            // as "paper-faithful reshaping" / "paper-faithfulness repair"; the
            // PV variant re-points it to goal-coverage / substantiveness.
            let mut fragments = if request.is_pv {
                vec!["pv/worker/theorem_stating/10_mode_guidance.md"]
            } else {
                vec!["worker/theorem_stating/10_mode_guidance.md"]
            };
            fragments.extend(theorem_worker_first_request_prompt_fragments(request));
            // PV substitutive: DAG-size sizing, helper policy, and the
            // inverted-DAG failure-mode guidance are PV variants. The DAG-size
            // sizing and failure-mode guidance are PV-specific.
            if request.is_pv {
                fragments.extend([
                    "pv/worker/theorem_stating/15_initial_dag_size.md",
                    "pv/worker/theorem_stating/17_helper_policy.md",
                    "pv/worker/theorem_stating/20_common_failure_modes.md",
                ]);
            } else {
                fragments.extend([
                    "worker/theorem_stating/15_initial_dag_size.md",
                    "worker/theorem_stating/17_helper_policy.md",
                    "worker/theorem_stating/20_common_failure_modes.md",
                ]);
            }
            // TheoremStating Restructure scope guidance: included only when
            // the reviewer dispatched a Restructure task, so global/targeted
            // workers see no extra copy.
            if request.worker_context.validation_kind == WorkerValidationKind::TheoremRestructure {
                fragments.push("worker/theorem_stating/19_restructure_scope.md");
            }
            // RevisionStating workers run the Theorem profile but need
            // revision-specific scope prose (editable envelope, frozen nodes,
            // removed targets). Gated on the phase so ordinary TheoremStating
            // workers see no extra copy.
            if request.phase == crate::model::Phase::RevisionStating {
                fragments.push("worker/revision_stating/05_revision_scope.md");
            }
            fragments
        }
        WorkerProfile::ProofEasy | WorkerProfile::ProofHard => vec![
            isa_or_lean(
                target,
                "worker/proof_formalization/10_operational_guidance.md",
                "worker/proof_formalization/10_operational_guidance_isabelle.md",
            ),
            isa_or_lean(
                target,
                "worker/proof_formalization/15_failure_triage.md",
                "worker/proof_formalization/15_failure_triage_isabelle.md",
            ),
            isa_or_lean(
                target,
                "worker/proof_formalization/20_helper_decomposition.md",
                "worker/proof_formalization/20_helper_decomposition_isabelle.md",
            ),
        ],
        WorkerProfile::Cleanup | WorkerProfile::FinalCleanup | WorkerProfile::None => Vec::new(),
    }
}

fn cleanup_like_worker(request: &WrapperRequest) -> bool {
    matches!(
        request.worker_context.worker_profile,
        WorkerProfile::Cleanup | WorkerProfile::FinalCleanup
    )
}

fn worker_field_guidance_fragment(request: &WrapperRequest) -> &'static str {
    if cleanup_like_worker(request) {
        "worker/cleanup/37_field_guidance.md"
    } else if request.is_pv {
        // PV substitutive: "configured verification target" in place of
        // "configured paper target".
        "pv/worker/common/37_field_guidance.md"
    } else {
        "worker/common/37_field_guidance.md"
    }
}

fn worker_reviewer_comments_fragment(request: &WrapperRequest) -> &'static str {
    if cleanup_like_worker(request) {
        "worker/cleanup/35_reviewer_comments.md"
    } else {
        "worker/common/35_reviewer_comments.md"
    }
}

fn worker_outcomes_fragment(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> &'static str {
    if cleanup_like_worker(request) {
        "worker/cleanup/45_outcomes.md"
    } else {
        isa_or_lean(
            target,
            "worker/common/45_outcomes.md",
            "worker/common/45_outcomes_isabelle.md",
        )
    }
}

fn worker_new_nodes_allowed(request: &WrapperRequest) -> bool {
    // This is an acceptance-envelope property, so derive it from the
    // acceptance contract. State-generated requests deliberately mirror the
    // same validation kind into `worker_context`, but the two fields are
    // independently defaulted during deserialization. Reading the context
    // copy here can therefore make the rendered scope contradict the
    // acceptance contract on an otherwise valid request.
    let validation_kind = request.worker_acceptance.validation_kind;
    matches!(
        validation_kind,
        WorkerValidationKind::TheoremGlobal
            | WorkerValidationKind::TheoremTargeted
            | WorkerValidationKind::TheoremRestructure
            | WorkerValidationKind::ProofEasy
            | WorkerValidationKind::ProofLocal
            | WorkerValidationKind::ProofRestructure
            | WorkerValidationKind::ProofCoarseRestructure
    ) || (validation_kind == WorkerValidationKind::FinalCleanup
        && (matches!(
            request.worker_context.cleanup_active_task_kind_view,
            Some(CleanupTaskKind::ExtractHelper { .. } | CleanupTaskKind::ExtractShared { .. })
        ) || matches!(
            request
                .worker_context
                .cleanup_active_batch_kind_view
                .as_deref(),
            Some("extract_helper")
        )))
}

fn worker_has_last_invalid_snapshot(request: &WrapperRequest) -> bool {
    // Mirror runtime::worker_response_should_preserve_attempt: include any
    // retry whose previous attempt left a last_invalid sidecar. After the
    // 2026-04-25 broadening, all four non-Valid outcomes (Invalid,
    // Malformed, Stuck, NeedsRestructure) have both a Tablet snapshot AND
    // metadata.json — the kernel rolls the worktree back unconditionally
    // and preserves the worker's WIP at the sidecar location.
    matches!(
        request.retry_outcome_kind,
        RetryOutcomeKind::Invalid
            | RetryOutcomeKind::Stuck
            | RetryOutcomeKind::NeedsRestructure
            // PV under-model (Slice 1): mirrors
            // `worker_response_should_preserve_attempt` — the reject path rolls
            // the worktree back and writes a last_invalid sidecar.
            | RetryOutcomeKind::TargetFalseUnderModel
    )
}

/// Per the canonical-def inlining matrix
/// (memory: project_canonical_def_inlining_plan.md): inline the lane
/// def files that are relevant to what the worker can author under
/// its current validation_kind. Cleanup phases don't reopen tablet
/// structure and get an empty list. Helper-allowing
/// proof-formalization kinds get substantiveness + correspondence +
/// soundness (no faithfulness — paper-target coverage is locked once
/// theorem-stating clears). Difficulty is advisory; proof scope and
/// helper-obligation gates come from explicit reviewer controls.
/// TheoremGlobal/Targeted authors all four kinds of content, so all
/// four defs.
fn canonical_def_fragments_for_worker(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    use crate::model::WorkerValidationKind::*;
    if request.is_pv {
        // PV is substitutive: drop the FAITHFULNESS and DEVIATIONS lanes
        // (structurally absent / byte-pinned model), swap SUBSTANTIVENESS +
        // SOUNDNESS to the PV variants, and keep CORRESPONDENCE (PV-neutral).
        let (subst_def, sound_def) =
            ("pv/canonical/SUBSTANTIVENESS.md", "pv/canonical/SOUNDNESS.md");
        return match request.worker_context.validation_kind {
            TheoremGlobal | TheoremTargeted | TheoremRestructure | ProofEasy | ProofLocal
            | ProofRestructure | ProofCoarseRestructure => vec![
                "pv/canonical/EXTRACTION_MODEL.md",
                subst_def,
                "canonical/CORRESPONDENCE.md",
                sound_def,
            ],
            Cleanup | FinalCleanup | None => vec![],
        };
    }
    // CORRESPONDENCE/SOUNDNESS are inlined for the worker on BOTH backends
    // (not routed through `isa_or_lean` elsewhere); the isabelle variants
    // neutralize the Lean phrasing, the Lean branch stays verbatim.
    let correspondence = isa_or_lean(
        target,
        "canonical/CORRESPONDENCE.md",
        "canonical/CORRESPONDENCE_isabelle.md",
    );
    let soundness = isa_or_lean(
        target,
        "canonical/SOUNDNESS.md",
        "canonical/SOUNDNESS_isabelle.md",
    );
    match request.worker_context.validation_kind {
        TheoremGlobal | TheoremTargeted | TheoremRestructure => vec![
            "canonical/DEVIATIONS.md",
            "canonical/FAITHFULNESS.md",
            "canonical/SUBSTANTIVENESS.md",
            correspondence,
            soundness,
        ],
        ProofEasy | ProofLocal | ProofRestructure | ProofCoarseRestructure => vec![
            "canonical/DEVIATIONS.md",
            "canonical/SUBSTANTIVENESS.md",
            correspondence,
            soundness,
        ],
        Cleanup | FinalCleanup | None => vec![],
    }
}

/// Per the inlining matrix: reviewer scope follows phase. TheoremStating
/// reviewer adjudicates all four lanes; ProofFormalization reviewer
/// adjudicates substantiveness (helper nodes from Hard mode),
/// correspondence, and soundness. Cleanup is dormant.
fn canonical_def_fragments_for_reviewer(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    if request.is_pv {
        // PV substitutive: drop FAITHFULNESS + DEVIATIONS, swap
        // SUBSTANTIVENESS + SOUNDNESS to the PV variants, and keep CORRESPONDENCE.
        let (subst_def, sound_def) =
            ("pv/canonical/SUBSTANTIVENESS.md", "pv/canonical/SOUNDNESS.md");
        return match request.phase {
            Phase::TheoremStating | Phase::RevisionStating | Phase::ProofFormalization => vec![
                "pv/canonical/EXTRACTION_MODEL.md",
                subst_def,
                "canonical/CORRESPONDENCE.md",
                sound_def,
            ],
            Phase::Cleanup | Phase::Complete => vec![],
        };
    }
    // CORRESPONDENCE/SOUNDNESS are inlined for the reviewer on BOTH backends;
    // the isabelle variants neutralize the Lean phrasing, the Lean branch is
    // verbatim.
    let correspondence = isa_or_lean(
        target,
        "canonical/CORRESPONDENCE.md",
        "canonical/CORRESPONDENCE_isabelle.md",
    );
    let soundness = isa_or_lean(
        target,
        "canonical/SOUNDNESS.md",
        "canonical/SOUNDNESS_isabelle.md",
    );
    match request.phase {
        Phase::TheoremStating | Phase::RevisionStating => vec![
            "canonical/DEVIATIONS.md",
            "canonical/FAITHFULNESS.md",
            "canonical/SUBSTANTIVENESS.md",
            correspondence,
            soundness,
        ],
        Phase::ProofFormalization => vec![
            "canonical/DEVIATIONS.md",
            "canonical/SUBSTANTIVENESS.md",
            correspondence,
            soundness,
        ],
        // Correspondence blockers now surface to the Cleanup reviewer: a
        // Cleanup state may carry an open corr blocker on a non-protected
        // node (see `ProtocolState::formalization_valid`), and the
        // reviewer's move on one is to dispatch the repair route. It needs
        // the lane definition to judge that. No other lane can raise a
        // blocker in Cleanup, so no other canonical def is added.
        Phase::Cleanup => vec!["canonical/CORRESPONDENCE.md"],
        Phase::Complete => vec![],
    }
}

fn worker_has_meaningful_routing_hints(request: &WrapperRequest) -> bool {
    // B7: render the routing-hints fragment only when at least one hint
    // is set to a non-default value. Defaults are: next_context_mode =
    // Resume, paper_focus_ranges = [], work_style_hint = None.
    !request.worker_context.paper_focus_ranges.is_empty()
        || request.worker_context.next_context_mode != crate::model::WorkerContextMode::Resume
        || request.worker_context.work_style_hint != crate::model::WorkerWorkStyleHint::None
}

fn prompt_json_value_meaningful(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

fn worker_has_stuck_math_reviewer_lean_product(request: &WrapperRequest) -> bool {
    request
        .stuck_math_audit
        .last_reviewer_lean_product
        .as_ref()
        .is_some_and(prompt_json_value_meaningful)
}

fn request_has_audit_plan(request: &WrapperRequest) -> bool {
    request.audit_plan.is_some()
}

fn request_has_need_input_audit_plan(request: &WrapperRequest) -> bool {
    request
        .audit_plan
        .as_ref()
        .is_some_and(|plan| plan.need_input_audit)
}

/// True when the live audit plan is planner-origin — a fresh-run initial plan
/// (`initial_plan`) or a revision-start plan (`revision_audit`). These plans are
/// authored before the first worker (or at revision start), not in response to
/// stagnation, so they render through the planner-framed 29b/34c variants
/// instead of the stagnation-framed audit-plan fragments. A need_input plan is
/// checked first at each selection site and is never routed here.
fn request_has_planner_audit_plan(request: &WrapperRequest) -> bool {
    request
        .audit_plan
        .as_ref()
        .is_some_and(|plan| plan.initial_plan || plan.revision_audit)
}

/// Option A: true when the historical-audit-plan-snapshot surface is
/// populated AND no live `audit_plan` is presented. Drives the
/// `29c_last_audit_plan.md` / `34d_last_audit_plan.md` prompt fragments
/// so the reviewer/worker reads the snapshot as advisory-only context.
fn request_has_only_snapshot_audit_plan(request: &WrapperRequest) -> bool {
    request.audit_plan.is_none() && request.previous_audit_plan_snapshot.is_some()
}

fn worker_post_initial_sketch_policy_applies(request: &WrapperRequest) -> bool {
    request.cycle > 1
        && matches!(
            request.worker_context.worker_profile,
            WorkerProfile::Theorem | WorkerProfile::ProofEasy | WorkerProfile::ProofHard
        )
}

/// Select the backend-specific worker fragment for a Lean-specific slot.
///
/// Returns the Isabelle sibling when the tablet's target is `IsabelleHol`,
/// and the Lean path VERBATIM for every other target (including `Lean` and
/// the no-repo / unresolved-config default). This is the Option-A selection
/// hook's only branch point: on the all-Lean live path every call returns its
/// `lean` argument unchanged, so the emitted fragment vec is byte-identical to
/// the pre-hook behavior.
fn isa_or_lean(
    target: crate::backend::BackendId,
    lean: &'static str,
    isabelle: &'static str,
) -> &'static str {
    match target {
        crate::backend::BackendId::IsabelleHol => isabelle,
        crate::backend::BackendId::Lean => lean,
    }
}

/// Resolve the tablet backend for a contract payload. With no repo path (or an
/// unresolved/absent config) this yields `Lean`, so the reviewer/verifier
/// prompt-fragment vecs stay byte-identical to the pre-hook all-Lean path.
fn contract_target(repo_path: Option<&Path>) -> crate::backend::BackendId {
    repo_path
        .map(crate::worker_normalization::tablet_target_for_repo)
        .unwrap_or(crate::backend::BackendId::Lean)
}

/// Reference-paper prompt fragments fire only when the run configures a
/// registry (and never in PV, which has no paper lane). An
/// empty-registry run's assembled prompts are byte-identical to the
/// pre-feature prompts.
fn reference_papers_fragments_active(request: &WrapperRequest) -> bool {
    !request.is_pv && !request.configured_reference_papers.is_empty()
}

fn worker_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    let mut fragments = vec![
        scheme_fragment_path(request, target),
        worker_intro_fragment(request),
    ];
    fragments.extend(worker_scenario_prompt_fragments(request, target));
    fragments.extend(worker_challenge_prompt_fragments(request, target));
    // PV substitutive: source-of-truth is the Rust crate + GOAL.md (no paper),
    // and the deviation lane is dropped (the model is byte-pinned).
    let source_of_truth_fragment = if request.is_pv {
        "pv/common/18_source_of_truth.md"
    } else {
        "worker/common/18_reference_paper.md"
    };
    fragments.push("shared/10_repository_root.md");
    fragments.push(isa_or_lean(
        target,
        "shared/20_read_files.md",
        "shared/20_read_files_isabelle.md",
    ));
    // Lean emits the loogle-helper + mathlib-conventions pair; the Isabelle
    // sibling folds both into the single library/fact-discovery fragment
    // (HOL/Main conventions + find_theorems/sledgehammer; no loogle server).
    match target {
        crate::backend::BackendId::IsabelleHol => {
            fragments.push("worker/common/17_isabelle_library.md");
        }
        crate::backend::BackendId::Lean => {
            fragments.push("worker/common/15_loogle.md");
            fragments.push("worker/common/17_mathlib.md");
        }
    }
    fragments.push(source_of_truth_fragment);
    fragments.push(isa_or_lean(
        target,
        "shared/25_filespec.md",
        "shared/25_filespec_isabelle.md",
    ));
    fragments.extend([
        "shared/30_project_invariants.md",
        isa_or_lean(
            target,
            "worker/common/20_authority.md",
            "worker/common/20_authority_isabelle.md",
        ),
    ]);
    if !request.is_pv {
        fragments.push("worker/common/21_deviations.md");
    } else if request.trust_base_required_v1 {
        // The one-file seam-repair lane remains available to every trust-run
        // PV worker. Conditional proposals are restricted to ordinary
        // theorem/proof workers.
        fragments.push("pv/worker/common/21_deviations_trust_v1.md");
        if conditional_worker_carrier_available(request) {
            fragments.push("pv/worker/common/22_conditional_theorem_proposal.md");
        }
    }
    // Process memory: the fragment is the bridge-rendered
    // `{{process_memory_block}}` alone, so it disappears from the prompt
    // (empty section) on runs without a `process-memory/` directory.
    fragments.push("worker/common/22_process_memory.md");
    fragments.push("worker/common/30_request.md");
    // Audit-ordered node retirement: frame the dedicated retirement task
    // (the bridge-rendered block carries the nodes + reason).
    if request.pending_node_retirement.is_some() {
        fragments.push("worker/common/30b_node_retirement_task.md");
    }
    if worker_has_meaningful_routing_hints(request) {
        // PV substitutive: the routing-hints fragment offers `paper_focus_ranges`
        // as line ranges into a paper. PV has no paper, so the PV variant
        // re-points the ranges at GOAL.md / crate / pinned model.
        fragments.push(if request.is_pv {
            "pv/worker/common/33_routing_hints.md"
        } else {
            "worker/common/33_routing_hints.md"
        });
    }
    fragments.extend([
        worker_reviewer_comments_fragment(request),
        "worker/common/36_recent_burst_history.md",
        worker_field_guidance_fragment(request),
    ]);
    if worker_post_initial_sketch_policy_applies(request) {
        fragments.push("worker/common/39_post_initial_sketch_policy.md");
    }
    fragments.extend([
        "worker/common/40_contract.md",
        worker_outcomes_fragment(request, target),
    ]);
    // On-demand audit (advisory): theorem/proof workers may ask the
    // reviewer to call a fresh StuckMathAudit. Gated on
    // `phase_admits_stuck_math_audit` because `record_latest_worker_rationale`
    // drops requests raised in non-admitting phases — only tell workers they
    // can request where it actually works. Cleanup workers use the
    // `cleanup_request_reaudit` channel instead (Cleanup is non-admitting).
    if request.phase_admits_stuck_math_audit() {
        // PV substitutive: re-points "the verification" / the "paper reference"
        // locus to a goal reference (no paper in PV).
        fragments.push(if request.is_pv {
            "pv/worker/common/46_audit_request.md"
        } else {
            "worker/common/46_audit_request.md"
        });
    }
    fragments.extend([
        "worker/common/50_acceptance.md",
        "shared/90_artifact_delivery.md",
        "worker/common/95_gate_authority.md",
    ]);
    fragments.splice(12..12, worker_profile_guidance_prompt_fragments(request, target));
    if worker_new_nodes_allowed(request) {
        fragments.insert(12, "worker/common/38_new_node_difficulty.md");
    }
    if has_review_verifier_evidence(request) {
        // PV substitutive: the math variant enumerates a "paper-faithfulness"
        // lane that is absent in PV; the PV variant lists only the live PV
        // lanes (substantiveness, correspondence, soundness).
        fragments.insert(12, if request.is_pv {
            "pv/worker/common/34_verifier_evidence.md"
        } else {
            "worker/common/34_verifier_evidence.md"
        });
    }
    if worker_has_stuck_math_reviewer_lean_product(request) {
        // PV substitutive: re-points "against the paper" → crate / GOAL.md /
        // pinned model and drops the host-`lake` inspection invocation.
        fragments.insert(
            12,
            if request.is_pv {
                "pv/worker/common/34b_stuck_math_reviewer_lean_product.md"
            } else {
                isa_or_lean(
                    target,
                    "worker/common/34b_stuck_math_reviewer_lean_product.md",
                    "worker/common/34b_stuck_math_reviewer_lean_product_isabelle.md",
                )
            },
        );
    }
    if request_has_audit_plan(request) {
        let fragment = if request_has_need_input_audit_plan(request) {
            // PV substitutive: "paper-faithful repair route" → goal-faithful.
            if request.is_pv {
                "pv/worker/common/34c_need_input_audit_plan.md"
            } else {
                "worker/common/34c_need_input_audit_plan.md"
            }
        } else if request_has_planner_audit_plan(request) {
            // Planner-origin (fresh-run initial plan or revision plan): the
            // stagnation-framed variant misdescribes provenance.
            "worker/common/34c_planner_plan.md"
        } else {
            "worker/common/34c_audit_plan.md"
        };
        fragments.insert(12, fragment);
    }
    if request_has_only_snapshot_audit_plan(request) {
        fragments.insert(12, "worker/common/34d_last_audit_plan.md");
    }
    if has_deterministic_worker_rejection_reasons(request) {
        fragments.insert(12, "worker/common/32_deterministic_worker_rejection.md");
    }
    if worker_has_last_invalid_snapshot(request) {
        fragments.insert(12, "worker/common/31_last_invalid.md");
    }
    fragments.insert(
        12,
        isa_or_lean(
            target,
            "worker/common/31_scratchpad.md",
            "worker/common/31_scratchpad_isabelle.md",
        ),
    );
    fragments.extend(canonical_def_fragments_for_worker(request, target));
    // Paper-grounding fragment: appended last so its presence does
    // not shift the hardcoded position-12 splice/insert targets above.
    // The fragment self-collapses to empty when the reviewer attached
    // no `paper_focus_ranges` (the bridge's `_paper_focus_fragments_block`
    // returns "" in that case), so unconditional inclusion is safe.
    // PV substitutive: the paper-focus lane is dropped entirely (no paper).
    if !request.is_pv {
        fragments.push("worker/common/19_paper_focus_fragments.md");
    }
    // Reference papers: appended (like the paper-focus fragment above)
    // so its conditional presence never shifts the position-12
    // splice/insert targets.
    if reference_papers_fragments_active(request) {
        fragments.push("worker/common/19b_reference_papers.md");
    }
    fragments.push(structured_request_pointer_fragment(request));
    fragments
}

fn review_primary_scenario_prompt_fragment(request: &WrapperRequest) -> &'static str {
    if request.post_advance_routing {
        return "review/common/05_post_advance_routing.md";
    }
    match request.retry_outcome_kind {
        RetryOutcomeKind::Invalid => "review/common/05_after_worker_invalid.md",
        RetryOutcomeKind::Stuck => "review/common/05_after_worker_stuck.md",
        RetryOutcomeKind::NeedsRestructure => "review/common/05_after_worker_needs_restructure.md",
        // PV under-model (Slice 1): the worker reported `T` false under the
        // Aeneas model. The reviewer forwards the disproof to the auditor via
        // a NeedInput escalation (the auditor is the sole adjudicator + sole
        // flipper). PV-only outcome; this fragment is never selected in math
        // mode.
        RetryOutcomeKind::TargetFalseUnderModel => {
            "review/common/05_after_worker_target_false_under_model.md"
        }
        // Bug X principled fix: a transport-failure escalation reaches the
        // reviewer when the bridge could not get any meaningful output from
        // the worker after `transport_invalid_review_threshold` retries.
        // Reuse the after-invalid fragment for now — the reviewer's
        // adjudication options (continue, give up, advance phase) are the
        // same as for an invalid worker; the comments will explain it was a
        // transport failure rather than bad work.
        RetryOutcomeKind::Transport => "review/common/05_after_worker_invalid.md",
        RetryOutcomeKind::None => {
            // PV substitutive: the paper-faithfulness lane is gated off (no
            // paper), and the deviation lane is gated off EXCEPT on
            // trust-required runs, where Stage 7 re-enables it as the
            // seam-repair lane with PV replacement fragments. The `!is_pv`
            // guards keep the drop structural — a leaked status cannot
            // route a PV review to a paper/deviation fragment that has no
            // PV replacement.
            if !request.is_pv && has_blocker_kind(request, BlockerKind::PaperFaithfulness) {
                if paper_review_is_split(request) {
                    "review/common/05_after_split_paper_faithfulness.md"
                } else if request.paper_blocker_adjudicable {
                    // A real paper Fail: either reviewer-adjudicable evidence,
                    // or a definite `current_paper_fail` (which includes the
                    // empty-coverage Fail — a worker-repairable definite
                    // failure, NOT an Unknown). Keep the failed framing.
                    "review/common/05_after_failed_paper_faithfulness.md"
                } else {
                    // PaperFaithfulness blocker present but genuinely
                    // Unknown-because-unverified (gated off the paper frontier)
                    // — there is NO failure to review. Frame for re-verification.
                    "review/common/05_after_unverified_paper_faithfulness.md"
                }
            } else if (!request.is_pv || request.trust_base_required_v1)
                && has_blocker_kind(request, BlockerKind::Deviation)
            {
                if request.deviation_blocker_adjudicable {
                    // A real deviation Fail (`current_deviation_fail`): keep the
                    // failed-deviation framing.
                    if request.is_pv {
                        "pv/review/05_after_failed_deviation_trust_v1.md"
                    } else {
                        "review/common/05_after_failed_deviation.md"
                    }
                } else {
                    // Deviation blocker present but Unknown / not-yet-verified
                    // (gated off the deviation frontier) — there is NO failure
                    // to review. Frame for re-verification.
                    if request.is_pv {
                        "pv/review/05_after_unverified_deviation_trust_v1.md"
                    } else {
                        "review/common/05_after_unverified_deviation.md"
                    }
                }
            } else if has_blocker_kind(request, BlockerKind::Substantiveness) {
                if request.substantiveness_blocker_adjudicable {
                    // A real substantiveness Fail (adjudicable evidence or a
                    // definite `current_substantiveness_fail`).
                    if request.phase.is_theorem_stating_like() {
                        "review/common/05_after_failed_substantiveness.md"
                    } else {
                        // Proof phase framing: worker `.tex` repair is the
                        // dominant move, with the Substantiveness-only reset
                        // `request_allowed_reset_blockers` offers here as the
                        // remedy for a stale verdict.
                        "review/common/05_proof_phase_substantiveness.md"
                    }
                } else {
                    // Substantiveness blocker present but Unknown /
                    // not-yet-verified (gated off the substantiveness frontier)
                    // — there is NO failure to review. Frame for re-verification.
                    "review/common/05_after_unverified_substantiveness.md"
                }
            } else if has_blocker_kind(request, BlockerKind::NodeCorr) {
                if corr_review_is_split(request) {
                    "review/common/05_after_split_correspondence.md"
                } else if request.corr_blocker_adjudicable {
                    // A real corr Fail the reviewer was handed evidence for:
                    // keep the failed-correspondence framing.
                    "review/common/05_after_failed_correspondence.md"
                } else {
                    // NodeCorr blocker present but Unknown / not-yet-verified
                    // (gated off the corr frontier by topological dispatch-
                    // eligibility) — there is NO failure to review. Frame the
                    // reviewer for re-verification, not a nonexistent failure,
                    // so the prompt does not contradict the run state.
                    "review/common/05_after_unverified_correspondence.md"
                }
            } else if has_blocker_kind(request, BlockerKind::Soundness) {
                if sound_review_is_split(request) {
                    "review/common/05_after_split_soundness.md"
                } else if request.sound_blocker_adjudicable {
                    // A real soundness Fail (adjudicable evidence or a definite
                    // `current_sound_fail`): keep the failed-soundness framing.
                    "review/common/05_after_failed_soundness.md"
                } else {
                    // Soundness blocker present but Unknown / not-yet-verified
                    // (gated off the soundness frontier — Sound is dispatched
                    // only once every other lane is Pass) — there is NO failure
                    // to review. Frame for re-verification.
                    "review/common/05_after_unverified_soundness.md"
                }
            } else if request.is_pv {
                // PV substitutive: the clean-verification fragment lists
                // "paper-faithfulness" among the inactive lanes; the PV variant
                // drops it (the lane is structurally absent).
                "pv/review/05_after_clean_verification.md"
            } else {
                "review/common/05_after_clean_verification.md"
            }
        }
    }
}

fn review_scenario_prompt_fragments(request: &WrapperRequest) -> Vec<&'static str> {
    let mut fragments = vec![review_primary_scenario_prompt_fragment(request)];
    if request.human_input_outstanding {
        fragments.push("review/common/06_with_outstanding_human_input.md");
    }
    fragments
}

fn reviewer_source_recourse_available() -> bool {
    #[cfg(test)]
    if let Some(value) = *SOURCE_RECOURSE_AVAILABLE_OVERRIDE
        .lock()
        .unwrap_or_else(|err| err.into_inner())
    {
        return value;
    }
    // Defense-in-depth: the reviewer can consult a read-only snapshot of
    // the trellis source tree when process semantics seem to block
    // progress. The snapshot is materialized by `scripts/trellis.sh` at
    // run startup; if both env vars are set, we add the
    // `05_source_recourse.md` fragment (and the Python bridge populates
    // matching context keys). If either is unset (no snapshot this run),
    // we silently omit the fragment — no broken-template artifact.
    let snapshot_set = std::env::var("TRELLIS_REVIEWER_SOURCE_SNAPSHOT")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let sha_set = std::env::var("TRELLIS_REVIEWER_SOURCE_SHA")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    snapshot_set && sha_set
}

#[cfg(test)]
static SOURCE_RECOURSE_AVAILABLE_OVERRIDE: std::sync::Mutex<Option<bool>> =
    std::sync::Mutex::new(None);

fn review_prompt_fragments(
    request: &WrapperRequest,
    target: crate::backend::BackendId,
) -> Vec<&'static str> {
    // B1: target-orientation fragment is phase-conditional. TheoremStating
    // gets the Global-authorizes-everything variant; ProofFormalization
    // gets the Restructure/CoarseRestructure variant. Cleanup/Complete
    // omit the fragment (no relevant levers to discuss).
    let target_orientation_fragment = match (request.phase, request.is_pv) {
        (Phase::TheoremStating | Phase::RevisionStating, true) => {
            Some("pv/review/33b_theorem_target_orientation.md")
        }
        (Phase::TheoremStating | Phase::RevisionStating, false) => {
            Some("review/common/33b_theorem_target_orientation.md")
        }
        (Phase::ProofFormalization, true) => Some("pv/review/33b_proof_target_orientation.md"),
        (Phase::ProofFormalization, false) => Some("review/common/33b_proof_target_orientation.md"),
        (Phase::Cleanup | Phase::Complete, _) => None,
    };
    let mut fragments = vec![
        scheme_fragment_path(request, target),
        "review/common/00_intro.md",
    ];
    fragments.extend(review_scenario_prompt_fragments(request));
    // PV substitutive: the verifier-reasoning header lists the three live PV
    // lanes, and source-of-truth is the Rust crate + GOAL.md (no paper). Both
    // are in-place single-entry swaps ⇒ the position-keyed inserts below are
    // unaffected.
    let verifier_reasoning_fragment = if request.is_pv {
        "pv/review/25_verifier_reasoning.md"
    } else {
        "review/common/25_verifier_reasoning.md"
    };
    let reviewer_source_of_truth_fragment = if request.is_pv {
        "pv/review/27_source_of_truth.md"
    } else {
        "review/common/27_reference_paper.md"
    };
    // PV substitutive: the NEED_INPUT guidance frames a "fundamental gap" as a
    // defect in "the paper being formalized"; the PV variant re-points it to
    // "the goal in GOAL.md is unprovable against the pinned model".
    let need_input_fragment = if request.is_pv {
        "pv/review/31_need_input.md"
    } else {
        "review/common/31_need_input.md"
    };
    // PV substitutive: the Sound-gate enumeration names the paper + deviation
    // lanes that never exist in PV ("every other verifier lane (paper,
    // correspondence, substantiveness, deviation)"); the PV variant names only
    // the live PV lanes (correspondence, substantiveness). In-place swap.
    let blocker_actions_fragment = if request.is_pv {
        "pv/review/30a_blocker_actions.md"
    } else {
        "review/common/30a_blocker_actions.md"
    };
    fragments.extend([
        "shared/10_repository_root.md",
        // GAP 2: route the Lean-bearing read-files + FILESPEC pair to the
        // `_isabelle` siblings on an isabelle run (Lean arg unchanged ⇒ Lean
        // reviewer prompt byte-identical). Array length is preserved so the
        // `insert(10, …)` index below stays valid.
        isa_or_lean(
            target,
            "shared/20_read_files.md",
            "shared/20_read_files_isabelle.md",
        ),
        isa_or_lean(
            target,
            "shared/25_filespec.md",
            "shared/25_filespec_isabelle.md",
        ),
        "shared/30_project_invariants.md",
        "review/common/10_request.md",
        "review/common/12_deterministic_worker_rejection.md",
        "review/common/20_blocker_choices.md",
        verifier_reasoning_fragment,
    ]);
    // Provenance note for kernel-auto-scheduled Sound results: rendered
    // directly after the verifier-reasoning block, and only when this
    // cycle's sound results include a kernel-scheduled dispatch
    // (`request_summary.kernel_scheduled_sound_nodes` non-empty).
    if !request.kernel_scheduled_sound_review_nodes.is_empty() {
        fragments.push("review/common/25a_kernel_scheduled_sound.md");
    }
    fragments.extend([
        "review/common/26_recent_burst_history.md",
        reviewer_source_of_truth_fragment,
    ]);
    // Reference papers: right after the source-of-truth fragment so the
    // registry reads as an annex to the primary-paper authority note.
    // (27b, not 28: `review/common/28_scratchpad.md` already owns 28.)
    if reference_papers_fragments_active(request) {
        fragments.push("review/common/27b_reference_papers.md");
    }
    fragments.extend([
        "review/common/28_scratchpad.md",
        "review/common/30_contract.md",
        blocker_actions_fragment,
        need_input_fragment,
        "review/common/32_revert.md",
    ]);
    // `32a_revert_last_clean.md` carves out the `reset = last_clean`
    // guidance from the always-shown `32_revert.md`. The kernel's
    // mandatory threshold rule is ProofFormalization-only, so showing its
    // language during TheoremStating/RevisionStating would contradict the
    // legal non-rewind Continue path. Gate by phase.
    if !request.phase.is_theorem_stating_like() {
        fragments.push("review/common/32a_revert_last_clean.md");
    }
    // PV substitutive: the all-math routing-hints fragment offers
    // `paper_focus_ranges` as line ranges into a paper. PV has no paper, but the
    // field is still kernel-consumed (and required on friction reviews), so the
    // PV variant re-points the ranges at GOAL.md / crate / pinned model instead.
    let routing_hints_fragment = if request.is_pv {
        "pv/review/33_routing_hints.md"
    } else {
        "review/common/33_routing_hints.md"
    };
    fragments.extend([routing_hints_fragment]);
    if !request.latest_review_rejection_reasons.is_empty() {
        fragments.insert(10, "review/common/13_review_response_rejection.md");
    }
    // Process memory: bridge-rendered block + challenge-channel
    // directive; renders empty (and drops out of the prompt) on runs
    // without a `process-memory/` directory.
    fragments.push("review/common/29d_process_memory.md");
    if let Some(fragment) = target_orientation_fragment {
        fragments.push(fragment);
    }
    if request.phase.is_theorem_stating_like() {
        fragments.push("review/common/33c_theorem_helper_policy.md");
    }
    // Challenge registry + coverage view and the routing duty for
    // uncovered targets. Skipped when no challenge targets are
    // configured (paper-only runs keep the existing prompt surface).
    if !request.configured_challenge_targets.is_empty() {
        // PV substitutive: the all-math variant routes uncovered targets "the
        // same way you route uncovered paper coverage" — no paper coverage
        // mechanism in PV; the PV variant makes the routing instruction
        // self-contained.
        fragments.push(if request.is_pv {
            "pv/review/33d_challenge_targets.md"
        } else {
            "review/common/33d_challenge_targets.md"
        });
    }
    fragments.extend([
        "review/common/34_worker_context_strategy.md",
        "review/common/35_comments.md",
    ]);
    if request.phase == Phase::ProofFormalization {
        // PV substitutive: the all-math variant names a "paper-faithfulness
        // blocker" target-bound carve-out absent in PV; the PV variant re-keys
        // it to the `ChallengeCoverage` blocker (the live PV target-bound class).
        fragments.push(if request.is_pv {
            "pv/review/36_authorized_nodes.md"
        } else {
            "review/common/36_authorized_nodes.md"
        });
    }
    // B5: include the early-build-out 15-50 proof-bearing-nodes guidance
    // only during theorem-stating with no held target. Once a target is
    // held, the DAG size advice is not the right framing.
    if request.phase == Phase::TheoremStating && request.held_target.is_none() {
        fragments.push("review/common/35b_initial_dag_size_comment.md");
    }
    // PV substitutive: the paper-focus strategy lane is dropped (no paper).
    if !request.is_pv {
        fragments.push("review/common/38_paper_focus_strategy.md");
    }
    fragments.extend([
        "review/common/39_revert_strategy.md",
        "review/common/40_authority.md",
        "shared/90_artifact_delivery.md",
    ]);
    if request.phase == Phase::ProofFormalization {
        // Position 19 anchors the restructure-strategy insert relative to the
        // (math) fragment prefix; dropping `38_paper_focus_strategy.md` for PV
        // would shift it, so resolve the insert point by content for PV and
        // swap to the PV restructure variant.
        let restructure_fragment = if request.is_pv {
            "pv/review/37_restructure_strategy.md"
        } else {
            "review/common/37_restructure_strategy.md"
        };
        if request.is_pv {
            let insert_at = fragments
                .iter()
                .position(|fragment| *fragment == "review/common/39_revert_strategy.md")
                .unwrap_or(fragments.len());
            fragments.insert(insert_at, restructure_fragment);
        } else {
            fragments.insert(19, restructure_fragment);
        }
    }
    // `32b` is the mandatory-LastClean rule itself, split off from `32a`'s
    // discretionary guidance so the reviewer is told "you must rewind at N"
    // exactly when `request_allowed_resets` will in fact withdraw the other
    // choices at N. The mandate is opt-in
    // (`TRELLIS_CSC_LAST_CLEAN_THRESHOLD`), so by default this fragment is
    // absent and 32a stands alone. Inserted here, by content, rather than
    // pushed alongside 32a: the restructure-strategy insert above is
    // position-anchored at 19, so growing the list before it runs would
    // land that fragment mid-32x-block.
    if !request.phase.is_theorem_stating_like()
        && crate::model::csc_last_clean_threshold().is_some()
    {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/32a_revert_last_clean.md")
            .map(|idx| idx + 1)
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/32b_revert_last_clean_mandate.md");
    }
    // Proposal v32: surface the active-coarse-anchor framing during
    // ProofFormalization Review, both when an anchor is locked (the
    // common case) and when the lock is open (kernel hints non-empty).
    // The fragment text covers both branches via the surfaced request
    // fields. Skipped when the mechanism is dormant (coarse DAG empty),
    // since the fragment would mislead the reviewer about state that
    // doesn't exist.
    if request.phase == Phase::ProofFormalization && !request.coarse_dag_nodes.is_empty() {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == blocker_actions_fragment)
            .map(|idx| idx + 1)
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/30b_coarse_anchor.md");
    }
    // On-demand audit: surface the `audit_request` reviewer fragment only
    // when an on-demand audit can fire right now (the same gate that adds
    // `audit_request` to optional_fields), so the instructions never
    // contradict the contract.
    if request.audit_request_admissible {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30a_blocker_actions.md")
            .map(|idx| idx + 1)
            .unwrap_or(fragments.len());
        // PV substitutive: re-points the "paper reference" locus to a goal
        // reference (no paper in PV).
        fragments.insert(insert_at, if request.is_pv {
            "pv/review/30d_audit_request.md"
        } else {
            "review/common/30d_audit_request.md"
        });
    }
    // Audit-ordered node retirement: surface the pending order + the
    // dispatch/decline directive only while an order is outstanding.
    if request.pending_node_retirement.is_some() {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30a_blocker_actions.md")
            .map(|idx| idx + 1)
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/30e_node_retirement.md");
    }
    // Sidecar grunt queue (Q6): the queue-management fragment appears
    // exactly when the queue fields are advertised (the same
    // runtime-resolved flag that gates optional_fields), so the
    // instructions never contradict the contract. Lands after
    // 30e/30a in the 30x block.
    if request.sidecar_advertise_queue_fields {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30e_node_retirement.md")
            .or_else(|| {
                fragments
                    .iter()
                    .position(|fragment| *fragment == "review/common/30a_blocker_actions.md")
            })
            .map(|idx| idx + 1)
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/30g_sidecar_grunts.md");
    }
    if request.phase == Phase::Cleanup {
        // Mirror of the worker's `final_cleanup/05_task.md` aimed at the
        // reviewer, plus the explicit "declare done when no more
        // meaningful cleanup is happening" rule.
        fragments.insert(2, "review/common/05_cleanup_phase.md");
    }
    if reviewer_source_recourse_available() {
        // Recourse fragment lives near the other `05_*` after-context
        // fragments. We append at the end of the leading scenario block
        // (after any `05_cleanup_phase.md` insertion above) so its
        // ordering stays stable regardless of phase.
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "shared/10_repository_root.md")
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/05_source_recourse.md");
    }
    if request.stuck_math_audit.active {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30_contract.md")
            .unwrap_or(fragments.len());
        // PV substitutive: the math variants frame "mathematical blockage" and
        // hand a host `lake env lean` recipe that violates the no-host-lake
        // invariant; the PV variants reframe as verification blockage and drop
        // the host-lake invocation.
        let fragment = if request_has_need_input_audit_plan(request) {
            if request.is_pv {
                "pv/review/29_need_input_auditor.md"
            } else {
                "review/common/29_need_input_auditor.md"
            }
        } else if request.is_pv {
            "pv/review/29_stuck_math_audit.md"
        } else {
            "review/common/29_stuck_math_audit.md"
        };
        fragments.insert(insert_at, fragment);
    }
    if request_has_audit_plan(request) {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30_contract.md")
            .unwrap_or(fragments.len());
        let fragment = if request_has_need_input_audit_plan(request) {
            // PV substitutive: "paper-faithful path" → goal-faithful path.
            if request.is_pv {
                "pv/review/29b_need_input_audit_plan.md"
            } else {
                "review/common/29b_need_input_audit_plan.md"
            }
        } else if request_has_planner_audit_plan(request) {
            // Planner-origin (fresh-run initial plan or revision plan): the
            // stagnation-framed variant misdescribes provenance.
            "review/common/29b_planner_plan.md"
        } else {
            "review/common/29b_audit_plan.md"
        };
        fragments.insert(insert_at, fragment);
    }
    if request_has_only_snapshot_audit_plan(request) {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30_contract.md")
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "review/common/29c_last_audit_plan.md");
    }
    // PV under-model (approach-audit route): shown exactly when an
    // auditor-named candidate `C` is enactable this turn (mirrors the
    // `assumption_authoring_request` optional-field gate).
    if !request.pending_authoring_candidate.trim().is_empty() {
        let insert_at = fragments
            .iter()
            .position(|fragment| *fragment == "review/common/30_contract.md")
            .unwrap_or(fragments.len());
        fragments.insert(insert_at, "pv/review/29d_assumption_authoring_enact.md");
    }
    fragments.extend(canonical_def_fragments_for_reviewer(request, target));
    fragments.push(structured_request_pointer_fragment(request));
    fragments
}

fn checker_command_template(parts: &[&str]) -> Value {
    Value::Array(
        parts
            .iter()
            .map(|part| Value::String((*part).to_owned()))
            .collect(),
    )
}

fn artifact_prompt_view_with_commands(json_parts: &[&str], acceptance_parts: &[&str]) -> Value {
    let mut value = artifact_prompt_view_payload();
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "json_check_command_template".to_owned(),
            checker_command_template(json_parts),
        );
        // Trim 13: emit `null` for the acceptance-check command when no
        // parts are supplied (corr/paper/sound contracts call this with
        // an empty slice). The bridge-side null-drop helper then strips
        // the `"acceptance_check_command_template": null` line so the
        // verifier prompt does not include an empty-array placeholder.
        let acceptance_value = if acceptance_parts.is_empty() {
            Value::Null
        } else {
            checker_command_template(acceptance_parts)
        };
        map.insert(
            "acceptance_check_command_template".to_owned(),
            acceptance_value,
        );
    }
    value
}

fn preamble_contract_payload(request: &WrapperRequest, repo_path: Option<&Path>) -> Value {
    if !request.verify_nodes.contains("Preamble") {
        return json!({
            "mode": "none",
            "item_ids": [],
            "empty_items_vacuously_supported": true,
        });
    }
    let items = repo_path
        .map(|repo| repo.join("Tablet").join("Preamble.tex"))
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|content| extract_tex_statement_items(&content, true))
        .unwrap_or_default();
    let item_ids: Vec<String> = items.iter().map(|item| item.id.clone()).collect();
    json!({
        "mode": "one_way_support",
        "item_ids": item_ids,
        "empty_items_vacuously_supported": true,
    })
}

pub fn project_invariants_payload(is_pv: bool) -> Value {
    // PV substitutive: the DAG-improvement progress mode is goal-coverage, not
    // paper-faithfulness (the worker, reviewer, AND audit agent all render this
    // payload). All-math keeps `paper_faithful_dag_improvement` byte-identical.
    let dag_improvement_mode = if is_pv {
        "goal_coverage_dag_improvement"
    } else {
        "paper_faithful_dag_improvement"
    };
    json!({
        "node_pair_contract": "every_present_node_has_lean_and_nl_statement",
        "proof_bearing_contract": "proof_nodes_need_closed_lean_or_rigorous_nl",
        "node_file_contract": "tablet_node_files_must_follow_filespec",
        "filespec_reference": "FILESPEC.md",
        "progress_modes": [
            "close_proof",
            dag_improvement_mode,
        ],
        "role_authority": {
            "worker": "writes_repository_content_only",
            "reviewer": "chooses_next_step_and_guidance",
            "verifier": "checks_invariants_without_choosing_work",
        }
    })
}

fn no_paper_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "targets": [],
            "blocked_targets": [],
        },
        "target_covering_nodes": {},
        "previous_own_findings_by_lane": {},
        "issue_reporting_policy": "none",
        "fixed_item_reporting_policy": "none",
        "target_issue_scope": [],
        "rubric": {
            "paper_statement_authority": "none",
            "covering_set_authority": "none",
            "definition_dependency_authority": "none",
            "faithfulness_standard": "none",
        },
        "artifact_contract": {
            "result_type": "paper_faithfulness_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "paper_faithfulness": {"decision": "PASS or FAIL", "issues": []},
                "overall": "APPROVE or REJECT",
                "summary": "",
                "comments": "",
            },
            "phase_blocks": {
                "paper_faithfulness": {
                    "decision_values": [],
                    "issue_subject_kind": "none",
                },
            },
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
    })
}

fn no_corr_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "nodes": [],
            "blocked_targets": [],
        },
        "previous_own_findings_by_lane": {},
        "issue_reporting_policy": "none",
        "fixed_item_reporting_policy": "none",
        "node_issue_scope": [],
        "rubric": {
            "statement_alignment_checks": [],
            "project_definition_policy": "none",
            "definition_hygiene": [],
            "duplicate_mathlib_definition_policy": "none",
            "preamble_item_issue_policy": "none",
        },
        "artifact_contract": {
            "result_type": "correspondence_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "correspondence": {"decision": "PASS or FAIL", "verdicts": []},
                "overall": "APPROVE or REJECT",
                "summary": "",
                "comments": "",
            },
            "phase_blocks": {
                "correspondence": {
                    "decision_values": [],
                    "verdict_values": [],
                    "comment_required_on_fail": true,
                },
            },
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
        "preamble_contract": {
            "mode": "none",
            "item_ids": [],
            "empty_items_vacuously_supported": true,
        },
    })
}

fn no_sound_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "node": "",
            "active_node": "",
            "held_target": "",
        },
        "previous_own_findings": {},
        "target_nodes": [],
        "evaluation_basis": "none",
        "detail_floor": "none",
        "rubric": {
            "proof_standard": "none",
            "reject_sketches": false,
            "detail_floor": "none",
            "lean_code_relevance": "none",
        },
        "artifact_contract": {
            "result_type": "soundness_result_v1",
            "decision_values": [],
            "overall_rule": "approve_iff_sound",
            "prompt_schema_example": {
                "node": "",
                "soundness": {"decision": "", "explanation": ""},
                "overall": "APPROVE or REJECT",
                "summary": "",
                "comments": "",
            },
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
    })
}

fn no_worker_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "mode": "",
            "active_node": "",
            "held_target": "",
            "worker_context": {
                "enabled": false,
                "active_difficulty": "hard",
                "active_easy_attempts": 0,
                "worker_profile": "none",
                "validation_kind": "none",
                "authorized_nodes": [],
                "allow_new_obligations": true,
                "must_close_active": false,
            },
            "blockers": [],
            "current_present_nodes": [],
            "current_definition_nodes": [],
            "current_preamble_nodes": [],
            "current_deps_scoped": {},
            "current_deps_scope_nodes": [],
            "current_target_claims_nonempty": {},
        },
        "reviewer_comments": "",
        "result_type": "worker_result_v1",
        "kernel_derives_structural_snapshot": true,
        "allowed_outcomes": [],
        "reported_delta_fields": [],
        "prompt_schema_example": {
            "outcome": "",
            "summary": "",
            "comments": "",
            "target_claim_updates": {"node_id": []},
            "difficulty_updates": {"node_id": ""},
        },
        "scope_contract": {
            "existing_node_scope_mode": "none",
            "authorized_existing_nodes": [],
            "configured_targets": [],
            "pending_targets": [],
            "pending_targets_meaning": "none",
            "new_nodes_allowed": false,
            "allow_new_obligations": true,
            "must_close_active": false,
        },
        "stuck_contract": {
            "allowed": false,
            "forbid_tablet_changes_when_stuck": false,
            "meaning": "none",
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
    })
}

fn no_review_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "mode": "",
            "active_node": "",
            "held_target": "",
            "invalid_attempt": false,
            "human_input_outstanding": false,
            "blocked_targets": [],
            "protected_nodes": [],
            "latest_worker_rationale": {
                "summary": "",
                "comments": "",
            },
        },
        "artifact_contract": {
            "result_type": "review_result_v1",
            "required_fields": [],
            "optional_fields": [],
            "prompt_schema_example": {
                "decision": [],
                "reason": "",
                "comments": "",
                "task_blocker_ids": [],
                "reset_blocker_ids": [],
                "request_sound_verifier_node_ids": [],
                "next_active": "",
                "next_mode": [],
                "reset": [],
                "difficulty_updates": {"node_id": ""},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": "",
            },
        },
        "verifier_evidence": {
            "paper": {},
            "corr": {},
            "sound": {},
        },
        "blocker_actions": {
            "required": false,
            "action_fields": [],
            "choices": [],
            "allowed_reset_ids": [],
            "sound_verifier_requestable_nodes": [],
            "reset_semantics": "none",
        },
        "blocker_partition": {
            "required": false,
            "action_fields": [],
            "choices": [],
            "allowed_reset_ids": [],
            "sound_verifier_requestable_nodes": [],
            "reset_semantics": "none",
        },
        "need_input_contract": {
            "meaning": "none",
            "blocker_partition_required": false,
            "task_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "",
            "next_mode": "",
            "next_worker_context_mode": "resume",
            "paper_focus_ranges": [],
            "work_style_hint": "none",
            "allow_new_obligations": true,
            "must_close_active": false,
        },
        "next_active_contract": {
            "kernel_hinted_nodes": [],
            "targeted_allowed_nodes": [],
            "allow_targeted_without_next_active": false,
        },
        "difficulty_update_contract": {
            "allowed_nodes": [],
        },
        "clear_human_input_contract": {
            "allowed_when_outstanding": false,
            "omit_when_not_allowed": true,
        },
        "comments_contract": {
            "field": "comments",
            "semantics": "non_authoritative_guidance_forwarded_to_future_workers",
            "empty_string_means_no_comments": true,
        },
        "artifact_prompt_view": artifact_prompt_view_payload(),
    })
}

pub fn correspondence_contract_payload(
    request: &WrapperRequest,
    repo_path: Option<&Path>,
) -> Value {
    if !matches!(request.kind, crate::model::RequestKind::Corr) {
        return no_corr_contract_payload();
    }
    // A9: drop kernel housekeeping fields (prompt_fragments,
    // artifact_prompt_view, issue/fixed_item_reporting_policy) from the
    // verifier-rendered contract via `_prompt_facing_corr_contract` on the
    // Python side, but they remain on the kernel-side contract for the
    // bridge's lane-scoping helpers. Below we still emit the kernel
    // structure; trimming for verifier prompts happens in bridge_prompts.
    // A10: drop duplicate `node_issue_scope` (redundant with
    // request_summary.nodes) — still emit at kernel level for legacy
    // consumers but the prompt-facing contract drops it.
    // A11: omit `preamble_contract` when Preamble is not in verify_nodes.
    let target = contract_target(repo_path);
    if let Some(conditional_request) = &request.conditional_theorem_correspondence {
        return with_prompt_facing_view(json!({
            "prompt_fragments": route_fragments(correspondence_prompt_fragments(request, target), target),
            "request_summary": {
                "phase": request.phase,
                "scenario": "conditional_theorem",
                "conditional_theorem_correspondence": conditional_request,
            },
            "artifact_contract": {
                "result_type": "correspondence_result_v1",
                "prompt_schema_example": {
                    "correspondence": {"decision": "PASS or FAIL", "verdicts": []},
                    "conditional_theorem": {
                        "request_sha256": conditional_request.request_sha256,
                        "proposal_sha256": conditional_request.proposal.proposal_sha256,
                        "boundary_expression_correct": true,
                        "condition_relevant": true,
                        "realizable_non_vacuous": true,
                        "obligation_preserved_on_domain": true,
                        "rust_axioms_backed_by_approved_assumptions": true,
                        "findings": "concise independent findings"
                    },
                    "overall": "APPROVE or REJECT",
                    "summary": "brief overall summary",
                    "comments": "optional short note"
                }
            },
            "artifact_prompt_view": artifact_prompt_view_with_commands(
                &["python3", "{{check_script_path}}", "correspondence-result", "{{raw_output_path}}"],
                &[],
            ),
        }));
    }
    if let Some(artifact_request) = &request.rust_witness_artifact_correspondence {
        return with_prompt_facing_view(json!({
            "prompt_fragments": route_fragments(correspondence_prompt_fragments(request, target), target),
            "request_summary": {
                "phase": request.phase,
                "scenario": "rust_witness_artifact",
                "rust_witness_artifact_correspondence": artifact_request,
            },
            "artifact_contract": {
                "result_type": "correspondence_result_v1",
                "prompt_schema_example": {
                    "correspondence": {"decision": "PASS or FAIL", "verdicts": []},
                    "rust_witness_artifact": {
                        "request_sha256": artifact_request.request_sha256,
                        "same_witness_as_lean_disproof": true,
                        "invokes_pinned_crate_operation": true,
                        "observation_contradicts_goal_obligation": true,
                        "reviewed_digests": artifact_request.reviewed_digests,
                        "decision": "pass or fail",
                        "reason": "independent correspondence judgment",
                    },
                    "overall": "APPROVE or REJECT",
                    "summary": "brief overall summary",
                    "comments": "optional short note",
                },
            },
            "artifact_prompt_view": artifact_prompt_view_with_commands(
                &["python3", "{{check_script_path}}", "correspondence-result", "{{raw_output_path}}"],
                &[],
            ),
        }));
    }
    let under_model_assumptions_nodes = pv_under_model_assumptions_corr_nodes(request);
    let has_under_model_assumptions = !under_model_assumptions_nodes.is_empty();
    let mut contract = serde_json::Map::new();
    contract.insert(
        "prompt_fragments".to_owned(),
        json!(route_fragments(correspondence_prompt_fragments(request, target), target)),
    );
    let mut request_summary = json!({
        "phase": request.phase,
        "nodes": request.verify_nodes,
        "blocked_targets": request.blocked_targets,
    });
    if has_under_model_assumptions {
        let request_summary = request_summary
            .as_object_mut()
            .expect("request_summary is object");
        request_summary.insert(
            "under_model_assumptions".to_owned(),
            json!({
                "nodes": under_model_assumptions_nodes,
                "lean_form": "axiom",
                "correspondence": "lean_axiom_proposition_to_tex_statement",
                "lean_axiom_policy": UNDER_MODEL_VALIDITY_HOOK_POLICY,
            }),
        );
        request_summary.insert(
            "tooling_hints".to_owned(),
            json!({
                "aeneas_lean_sources": "/path/to/trellis/pv_spike/tools/aeneas/backends/lean/Aeneas",
            }),
        );
    }
    contract.insert("request_summary".to_owned(), request_summary);
    contract.insert(
        "previous_own_findings_by_lane".to_owned(),
        json!(request.previous_corr_lane_findings),
    );
    contract.insert(
        "issue_reporting_policy".to_owned(),
        json!("explicit_per_node_verdicts"),
    );
    contract.insert(
        "fixed_item_reporting_policy".to_owned(),
        json!("summary_only"),
    );
    contract.insert("node_issue_scope".to_owned(), json!(request.verify_nodes));
    let mut rubric = json!({
        "statement_alignment_checks": [
            "quantifier_scope",
            "type_constraints",
            "implicit_assumptions",
            "domain_context",
        ],
        "project_definition_policy": "expand_project_definitions_but_trust_mathlib",
        "definition_hygiene": [
            "reject_opaque",
            "reject_axiom",
            "reject_constant",
            "reject_sorry_in_definition",
        ],
        "duplicate_mathlib_definition_policy": "reject_project_duplicates",
        "preamble_item_issue_policy": "use_exact_item_id",
    });
    if has_under_model_assumptions {
        rubric
            .as_object_mut()
            .expect("rubric is object")
            .insert(
                "definition_hygiene_exception_roles".to_owned(),
                json!(["under_model_assumptions"]),
            );
    }
    contract.insert("rubric".to_owned(), rubric);
    contract.insert(
        "artifact_contract".to_owned(),
        json!({
            "result_type": "correspondence_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "correspondence": {
                    "decision": "PASS or FAIL",
                    "verdicts": [
                        {"node": "node_id", "verdict": "Pass"},
                        {"node": "node_id", "verdict": "Fail",
                         "comment": "concrete recommendation for the worker"},
                    ],
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
            "phase_blocks": {
                "correspondence": {
                    "decision_values": ["PASS", "FAIL"],
                    "verdict_values": ["Pass", "Fail"],
                    "comment_required_on_fail": true,
                }
            }
        }),
    );
    contract.insert(
        "artifact_prompt_view".to_owned(),
        artifact_prompt_view_with_commands(
            &[
                "python3",
                "{{check_script_path}}",
                "correspondence-result",
                "{{raw_output_path}}",
            ],
            &[],
        ),
    );
    if request.verify_nodes.contains("Preamble") {
        contract.insert(
            "preamble_contract".to_owned(),
            preamble_contract_payload(request, repo_path),
        );
    }
    Value::Object(contract)
}

pub fn paper_contract_payload(request: &WrapperRequest, repo_path: Option<&Path>) -> Value {
    if !matches!(request.kind, crate::model::RequestKind::Paper) {
        return no_paper_contract_payload();
    }
    let target = contract_target(repo_path);
    let is_per_node_scenario =
        !request.substantiveness_verify_nodes.is_empty() && request.paper_verify_targets.is_empty();
    if let Some(deviation_id) = request.deviation_verify_id.as_ref() {
        let mut payload = json!({
            "prompt_fragments": route_fragments(paper_prompt_fragments(request, target), target),
            "request_summary": {
                "phase": request.phase,
                "scenario": "deviation_authorization",
                "deviation_id": deviation_id,
                "deviation_path": request.deviation_verify_path,
            },
            // Always emit (paper_verify_targets is empty in this scenario,
            // so the map is empty); kernel-side guarantee lets the bridge
            // consume the field unconditionally without a fallback shim.
            "target_covering_nodes": paper_target_covering_nodes(request),
            "deviation": {
                "id": deviation_id,
                "path": request.deviation_verify_path,
            },
            "rubric": {
                "rubric_reference": "DEVIATIONS.md",
                "verdict": "pass_iff_deviation_is_tex_only_explicit_and_has_a_rigorous_return_to_paper_faithful_steps",
            },
            "artifact_contract": {
                "result_type": "deviation_authorization_result_v1",
                "overall_rule": "approve_iff_pass",
                "prompt_schema_example": {
                    "deviation_authorization": {
                        "id": deviation_id,
                        "decision": "PASS or FAIL",
                        "comment": "required on FAIL",
                    },
                    "overall": "APPROVE or REJECT",
                    "summary": "brief overall summary",
                    "comments": "optional short note",
                },
            },
            "artifact_prompt_view": artifact_prompt_view_with_commands(&[
                "python3",
                "{{check_script_path}}",
                "deviation-authorization-result",
                "{{raw_output_path}}",
            ], &[]),
        });
        let view = paper_prompt_facing_view(&payload);
        if let Some(map) = payload.as_object_mut() {
            map.insert("prompt_facing_view".to_string(), view);
        }
        return payload;
    }
    if is_per_node_scenario {
        // Substantiveness scenario. The verifier sees the
        // outstanding Unknown set and triages — Pass / Fail / NotDoneYet
        // per node, with each verdict carried explicitly in
        // `verdicts[]`. The kernel collects per-node evidence and
        // re-issues another Paper request for any NotDoneYet residual,
        // subject to a safety bound.
        let false_as_stated_available = request.is_pv;
        let verdict_values = if false_as_stated_available {
            json!(["Pass", "FalseAsStated", "Fail", "NotDoneYet"])
        } else {
            json!(["Pass", "Fail", "NotDoneYet"])
        };
        let mut verdict_examples = vec![
            json!({"node": "node_id", "verdict": "Pass"}),
            json!({
                "node": "node_id",
                "verdict": "Fail",
                "comment": "concrete recommendation: strengthen / merge / remove / etc.",
            }),
            json!({"node": "node_id", "verdict": "NotDoneYet"}),
            json!({
                "node": "node_id",
                "verdict": "NotDoneYet",
                "comment": "ran out of time on the case analysis",
            }),
        ];
        if false_as_stated_available {
            verdict_examples.insert(1, json!({
                "node": "node_id",
                "verdict": "FalseAsStated",
                "comment": "pinned-model refutation basis",
            }));
        }
        let mut payload = json!({
            "prompt_fragments": route_fragments(paper_prompt_fragments(request, target), target),
            "request_summary": {
                "phase": request.phase,
                "scenario": "substantiveness",
                "nodes": request.substantiveness_verify_nodes,
                "blocked_targets": request.blocked_targets,
            },
            // Always emit (paper_verify_targets is empty in this scenario,
            // so the map is empty); kernel-side guarantee lets the bridge
            // consume the field unconditionally without a fallback shim.
            "target_covering_nodes": paper_target_covering_nodes(request),
            "node_paper_basis_inputs": substantiveness_basis_inputs(request),
            "authorized_deviations": request.authorized_deviations,
            // Deviations the Deviation lane rejected. A node claiming one of
            // these reads substantiveness Fail (model.rs
            // `current_substantiveness_state`); surface the rejected set so
            // the verifier treats such a claim as an EXPECTED Fail it should
            // route to the worker (drop the claim or revise the deviation),
            // not a "claimed-but-unauthorized" system inconsistency worth
            // `system_feedback`.
            "rejected_deviations": request.rejected_deviations,
            "node_deviation_claims": request.node_deviation_claims,
            "previous_own_findings": request.previous_substantiveness_lane_findings,
            "issue_reporting_policy": "explicit_per_node_verdicts",
            "fixed_item_reporting_policy": "summary_only",
            "rubric": {
                "verdict": "pass_iff_valid_AND_meaningful_decomposition",
                "rubric_reference": "SUBSTANTIVENESS.md",
                "strengthening_allowed": true,
                "missing_node_default": "NotDoneYet",
                "triage_signal": "verdict: 'NotDoneYet' marks the node as not-yet-evaluated; missing nodes default to NotDoneYet",
            },
            "artifact_contract": {
                "result_type": "substantiveness_result_v1",
                "overall_rule": "approve_iff_pass",
                "prompt_schema_example": {
                    "substantiveness": {
                        "decision": "PASS or FAIL",
                        "verdicts": verdict_examples,
                    },
                    "overall": "APPROVE or REJECT",
                    "summary": "brief overall summary",
                    "comments": "optional short note",
                },
                "phase_blocks": {
                    "substantiveness": {
                        "decision_values": ["PASS", "FAIL"],
                        "verdict_values": verdict_values,
                        "comment_required_on_fail": true,
                        "comment_required_on_false_as_stated": false_as_stated_available,
                    }
                }
            },
            "artifact_prompt_view": artifact_prompt_view_with_commands(&[
                "python3",
                "{{check_script_path}}",
                "substantiveness-result",
                "{{raw_output_path}}",
            ], &[]),
        });
        // Reference papers (Amendment G5): the STATE-carried registry,
        // restricted to ids claimed by frontier nodes, plus the frontier
        // claims themselves — the bridge's sole path source for reference
        // documents (it never re-reads config). Inserted only when the
        // run configures a registry, so registry-free contracts stay
        // byte-identical; within a registry run an empty restriction is
        // emitted as Value::Null, which the bridge's _drop_null_keys
        // strips (the sound_reverification_context precedent).
        if !request.configured_reference_papers.is_empty() {
            let frontier_grounds: BTreeMap<&NodeId, &BTreeSet<RefPaperId>> = request
                .node_reference_grounds
                .iter()
                .filter(|(node, claims)| {
                    request.substantiveness_verify_nodes.contains(*node) && !claims.is_empty()
                })
                .collect();
            let claimed_ids: BTreeSet<&RefPaperId> =
                frontier_grounds.values().flat_map(|s| s.iter()).collect();
            let reference_papers: BTreeMap<&RefPaperId, Value> = request
                .configured_reference_papers
                .iter()
                .filter(|(id, _)| claimed_ids.contains(id))
                .map(|(id, spec)| {
                    (
                        id,
                        json!({"tex_path": spec.tex_path, "source_id": spec.source_id}),
                    )
                })
                .collect();
            if let Some(map) = payload.as_object_mut() {
                map.insert(
                    "reference_papers".to_string(),
                    if reference_papers.is_empty() {
                        Value::Null
                    } else {
                        json!(reference_papers)
                    },
                );
                map.insert(
                    "node_reference_grounds".to_string(),
                    if frontier_grounds.is_empty() {
                        Value::Null
                    } else {
                        json!(frontier_grounds)
                    },
                );
            }
        }
        let view = paper_prompt_facing_view(&payload);
        if let Some(map) = payload.as_object_mut() {
            map.insert("prompt_facing_view".to_string(), view);
        }
        return payload;
    }
    let mut payload = json!({
        "prompt_fragments": route_fragments(paper_prompt_fragments(request, target), target),
        "request_summary": {
            "phase": request.phase,
            "scenario": "target_package",
            "targets": request.paper_verify_targets,
            "blocked_targets": request.blocked_targets,
        },
        "target_covering_nodes": paper_target_covering_nodes(request),
        "previous_own_findings_by_lane": request.previous_paper_lane_findings,
        "issue_reporting_policy": "current_failures_only",
        "fixed_item_reporting_policy": "summary_only",
        "rubric": {
            "paper_statement_authority": "configured_target_ids_label_first",
            "covering_set_authority": "covering_nodes_for_target_claim",
            "definition_dependency_authority": "statement_level_definition_hashes_only",
            "faithfulness_standard": "covering_tex_statements_collectively_capture_target_statement",
        },
        "artifact_contract": {
            "result_type": "paper_faithfulness_result_v1",
            "overall_rule": "approve_iff_pass",
            "prompt_schema_example": {
                "paper_faithfulness": {
                    "decision": "PASS or FAIL",
                    "issues": [{"node": "target_id", "description": "..."}],
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
            "phase_blocks": {
                "paper_faithfulness": {
                    "decision_values": ["PASS", "FAIL"],
                    "issue_subject_kind": "target",
                }
            }
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "paper-faithfulness-result",
            "{{raw_output_path}}",
        ], &[]),
    });
    let view = paper_prompt_facing_view(&payload);
    if let Some(map) = payload.as_object_mut() {
        map.insert("prompt_facing_view".to_string(), view);
    }
    payload
}

/// Per-node paper-basis inputs surfaced in the contract for the per-node
/// scenario. For each node on the frontier we provide:
///   - `tex_path`: path to the node's `.tex` file under `Tablet/`.
///   - `lean_path`: path to the node's `.lean` file under `Tablet/`.
///   - `imported_by`: nodes that import this one (target reverse-deps),
///     so the verifier can judge how downstream usability is affected by
///     a weakened statement.
///   - `node_kind`: preamble / definition / proof.
fn substantiveness_basis_inputs(request: &WrapperRequest) -> Value {
    let mut by_node = serde_json::Map::new();
    for node in &request.substantiveness_verify_nodes {
        let imported_by: BTreeSet<NodeId> = request
            .current_deps
            .iter()
            .filter_map(|(parent, children)| {
                if children.contains(node) {
                    Some(parent.clone())
                } else {
                    None
                }
            })
            .collect();
        let node_kind = request
            .current_node_kinds
            .get(node)
            .copied()
            .unwrap_or_default();
        by_node.insert(
            node.as_str().to_string(),
            json!({
                "tex_path": format!("Tablet/{}.tex", node.as_str()),
                "lean_path": format!("Tablet/{}.lean", node.as_str()),
                "imported_by": imported_by,
                "node_kind": node_kind,
            }),
        );
    }
    Value::Object(by_node)
}

pub fn soundness_contract_payload(request: &WrapperRequest, repo_path: Option<&Path>) -> Value {
    if !matches!(request.kind, crate::model::RequestKind::Sound) {
        return no_sound_contract_payload();
    }
    let target = contract_target(repo_path);
    let reverification_context = request
        .sound_reverification_context
        .as_ref()
        .map(|ctx| {
            json!({
                "target": ctx.target,
                "prior_status": ctx.prior_status,
                "current_status": ctx.current_status,
                "own_tex_changed": ctx.own_tex_changed,
                "deps_changed": ctx.deps_changed,
                "prior_lane_evidence": ctx.prior_lane_evidence,
                "git_access_hint": "The repository (including .git) is mounted read-only inside this sandbox. You may inspect prior content with `git -C <repo_path> show <cycle-tag>:Tablet/<Dep>.tex` or `git -C <repo_path> log -- Tablet/<Dep>.tex`. Tags of the form `cycle-N` exist for every committed cycle.",
            })
        })
        .unwrap_or(Value::Null);
    // PV substitutive: the soundness detail floor is the paper's detail level
    // for all-math, but PV has no paper — its floor is what a Lean formalizer
    // needs (see pv/canonical/SOUNDNESS.md + pv/verifier/soundness/05_pv_floor.md).
    // Emit a PV-coherent value so the structured JSON does not contradict the
    // inlined PV floor prose. Gated on is_pv ⇒ math byte-identical.
    let detail_floor_value = if request.is_pv {
        "lean_formalization_floor"
    } else {
        "paper_floor"
    };
    json!({
        "prompt_fragments": route_fragments(soundness_prompt_fragments(request, target), target),
        "request_summary": {
            "phase": request.phase,
            "node": request.sound_verify_node,
            "active_node": request.active_node,
            "held_target": request.held_target,
        },
        "previous_own_findings": request.previous_sound_lane_findings,
        "reverification_context": reverification_context,
        "target_nodes": request.sound_verify_nodes,
        "evaluation_basis": "nl_only",
        "detail_floor": detail_floor_value,
        "rubric": {
            "proof_standard": "line_by_line_rigorous",
            "reject_sketches": true,
            "detail_floor": detail_floor_value,
            "lean_code_relevance": "ignore_lean_check_nl_only",
            "dependency_citation_rule": "every_cross_node_nl_dependency_must_use_noderef",
            "dependency_citation_syntax": "\\noderef{NodeName}",
            "shared_background_exception": "Preamble.tex items are shared context, not cross-node dependencies; a proof uses them without \\noderef{Preamble}",
        },
        "artifact_contract": {
            "result_type": "soundness_result_v1",
            "decision_values": ["SOUND", "UNSOUND", "STRUCTURAL"],
            "overall_rule": "approve_iff_sound",
            "prompt_schema_example": {
                "node": "target_node",
                "soundness": {
                    "decision": "SOUND, UNSOUND, or STRUCTURAL",
                    "explanation": "brief explanation",
                },
                "overall": "APPROVE or REJECT",
                "summary": "brief overall summary",
                "comments": "optional short note",
            },
        }
        ,
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "soundness-result",
            "{{raw_output_path}}",
            "--node",
            "{{node_name}}",
        ], &[
        ]),
    })
}

/// Worker-prompt blocker-status block, rendered by the kernel and spliced
/// into the worker prompt by the bridge.
///
/// `md` is the Markdown body. When the live blocker count overflows the
/// inline limit, the body contains the literal placeholder `{sidecar_path}`
/// (three-char prefix + suffix) that the bridge substitutes with the
/// concrete sidecar path on disk before splicing. `sidecar_payload` carries
/// the structured payload the bridge writes to that sidecar; `None` means
/// inline-only and no sidecar I/O is needed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerBlockerStatusBlock {
    pub md: String,
    pub sidecar_payload: Option<Value>,
}

/// Default inline limit for the worker blocker-status table.
///
/// Mirrors the bridge's `BLOCKER_INLINE_LIMIT_DEFAULT`. When the
/// `TRELLIS_BLOCKER_INLINE_LIMIT` env var is set (and parseable), it
/// overrides this default.
pub const WORKER_BLOCKER_INLINE_LIMIT_DEFAULT: usize = 8;
/// Fallback K when the actionable filter returns nothing; first K blockers
/// by stable alphabetical label order are shown.
pub const WORKER_BLOCKER_ACTIONABLE_FALLBACK_K: usize = 5;
/// Env var that overrides the inline limit at runtime.
pub const WORKER_BLOCKER_INLINE_LIMIT_ENV: &str = "TRELLIS_BLOCKER_INLINE_LIMIT";
/// Literal placeholder inside `WorkerBlockerStatusBlock::md` that the
/// bridge replaces with the concrete on-disk sidecar path before splicing.
pub const WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER: &str = "{sidecar_path}";

fn worker_blocker_inline_limit() -> usize {
    if let Ok(raw) = std::env::var(WORKER_BLOCKER_INLINE_LIMIT_ENV) {
        if let Ok(value) = raw.trim().parse::<usize>() {
            return value;
        }
    }
    WORKER_BLOCKER_INLINE_LIMIT_DEFAULT
}

/// Snake-case JSON tag for a `BlockerKind` variant (matches Serde's default
/// derive output: variant name as-is, e.g. "PaperFaithfulness").
fn blocker_kind_str(kind: BlockerKind) -> &'static str {
    match kind {
        BlockerKind::PaperFaithfulness => "PaperFaithfulness",
        BlockerKind::Deviation => "Deviation",
        BlockerKind::NodeCorr => "NodeCorr",
        BlockerKind::Soundness => "Soundness",
        BlockerKind::Substantiveness => "Substantiveness",
        BlockerKind::ChallengeCoverage => "ChallengeCoverage",
    }
}

/// "otype:body" label used in worker-facing blocker rows.
fn worker_blocker_object_label(blocker: &Blocker) -> String {
    match &blocker.object {
        BlockerObject::Node { node } => format!("node:{}", node.as_str()),
        BlockerObject::Target { target } => format!("target:{}", target.as_str()),
        BlockerObject::Deviation { deviation } => format!("deviation:{}", deviation.as_str()),
    }
}

/// Worker-row format `"%5d | %-16s | %s"`, mirroring the bridge formatter.
fn worker_blocker_format_row(index: usize, blocker: &Blocker) -> String {
    let kind = blocker_kind_str(blocker.kind);
    let label = worker_blocker_object_label(blocker);
    format!("{:5} | {:16} | {}", index, kind, label)
}

/// "k1=n1, k2=n2, ..." counts-by-kind line, sorted by kind label.
fn worker_blocker_kind_counts_line(blockers: &[&Blocker]) -> String {
    if blockers.is_empty() {
        return "(none)".to_owned();
    }
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for b in blockers {
        *counts.entry(blocker_kind_str(b.kind)).or_insert(0) += 1;
    }
    let mut parts = Vec::with_capacity(counts.len());
    for (k, v) in counts.iter() {
        parts.push(format!("{}={}", k, v));
    }
    parts.join(", ")
}

/// Select actionable blocker indices using the same heuristic as the
/// bridge's `_select_actionable_blocker_indices`. Returns
/// `(indices, reason_phrase)` where the reason phrase is rendered inline.
///
/// A blocker is actionable when its node referent lives in
/// `{active_node} ∪ deps_neighborhood`, or its target referent equals
/// `held_target`. On empty actionable set we fall back to first K by
/// stable alphabetical label order (matching the Python's tuple sort).
fn worker_blocker_select_actionable(
    blockers: &[&Blocker],
    active_node: Option<&str>,
    held_target: Option<&str>,
    deps_neighborhood: &BTreeSet<String>,
) -> (Vec<usize>, String) {
    let mut neighborhood: BTreeSet<&str> = BTreeSet::new();
    if let Some(n) = active_node {
        neighborhood.insert(n);
    }
    for n in deps_neighborhood {
        neighborhood.insert(n.as_str());
    }
    let target_focus = held_target;

    let mut matched: Vec<usize> = Vec::new();
    for (index, blocker) in blockers.iter().enumerate() {
        match &blocker.object {
            BlockerObject::Node { node } => {
                if neighborhood.contains(node.as_str()) {
                    matched.push(index);
                }
            }
            BlockerObject::Target { target } => {
                if let Some(t) = target_focus {
                    if target.as_str() == t {
                        matched.push(index);
                    }
                }
            }
            BlockerObject::Deviation { .. } => {}
        }
    }

    if !matched.is_empty() {
        return (matched, "active node + direct-dep neighborhood".to_owned());
    }

    let fallback_count = std::cmp::min(WORKER_BLOCKER_ACTIONABLE_FALLBACK_K, blockers.len());
    if fallback_count == 0 {
        return (Vec::new(), "no live blockers".to_owned());
    }
    let mut sortable: Vec<(String, usize)> = blockers
        .iter()
        .enumerate()
        .map(|(i, b)| (worker_blocker_object_label(b), i))
        .collect();
    sortable.sort();
    let indices: Vec<usize> = sortable
        .into_iter()
        .take(fallback_count)
        .map(|(_, i)| i)
        .collect();
    let note = format!(
        "fallback: no blockers touch active_node/held_target; showing \
first {} of {} by label order",
        fallback_count,
        blockers.len()
    );
    (indices, note)
}

/// Format the actionable-subset table (worker-facing, no `id` column).
fn worker_blocker_format_actionable_table(
    indices: &[usize],
    blockers: &[&Blocker],
    note: &str,
) -> String {
    if indices.is_empty() {
        return format!("(none) -- {}", note);
    }
    let rows: Vec<String> = indices
        .iter()
        .filter_map(|&i| blockers.get(i).map(|b| worker_blocker_format_row(i, b)))
        .collect();
    let header_lines = [
        "Index | Kind             | Object".to_owned(),
        "------|------------------|".to_owned() + &"-".repeat(32),
    ];
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Actionable subset ({} of {}): {}",
        rows.len(),
        blockers.len(),
        note
    );
    out.push('\n');
    for line in header_lines.iter() {
        out.push_str(line);
        out.push('\n');
    }
    for (i, row) in rows.iter().enumerate() {
        out.push_str(row);
        if i + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

/// Compute the bridge's `deps_neighborhood`: direct out-edges of
/// `active_node` plus reverse-edges (consumers of `active_node`),
/// projected through the worker-prompt DAG scope. Matches the bridge's
/// algorithm in `bridge_prompts.py`.
fn worker_blocker_deps_neighborhood(request: &WrapperRequest) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let active = match request.active_node.as_ref() {
        Some(n) => n,
        None => return out,
    };
    let dag_scope = request.worker_prompt_dag_scope();
    // Direct out-edges of active_node (only when active_node itself is in
    // the scoped view).
    if dag_scope.contains(active) {
        if let Some(direct) = request.current_deps.get(active) {
            for n in direct {
                out.insert(n.as_str().to_owned());
            }
        }
    }
    // Reverse-edges: any node in scope listing active_node in its deps.
    for (node, deps) in request.current_deps.iter() {
        if !dag_scope.contains(node) {
            continue;
        }
        if deps.contains(active) {
            out.insert(node.as_str().to_owned());
        }
    }
    out
}

/// Render the worker-facing blocker-status block.
///
/// Byte-equivalent to the bridge's `_worker_blocker_status_block`. In the
/// overflow case, `md` contains the literal `{sidecar_path}` placeholder
/// (see `WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER`) that the bridge
/// substitutes with the concrete sidecar path before splicing.
/// Named-reason footnote for any NodeCorr blocker whose node is a `True`
/// placeholder definition (`placeholder_definition_nodes`). These blockers
/// carry no verifier evidence — the kernel forced the correspondence Fail
/// deterministically (see `current_corr_state`) — so the worker must be
/// told the rule explicitly. Returns an empty string (no footnote) when no
/// live NodeCorr blocker names a placeholder definition, keeping the block
/// byte-identical to the pre-placeholder rendering on every other run.
fn worker_blocker_placeholder_definition_note(
    blockers: &[&Blocker],
    request: &WrapperRequest,
) -> String {
    if request.placeholder_definition_nodes.is_empty() {
        return String::new();
    }
    let mut named: BTreeSet<&str> = BTreeSet::new();
    for b in blockers {
        if b.kind != BlockerKind::NodeCorr {
            continue;
        }
        if let BlockerObject::Node { node } = &b.object {
            if request.placeholder_definition_nodes.contains(node) {
                named.insert(node.as_str());
            }
        }
    }
    if named.is_empty() {
        return String::new();
    }
    let nodes: Vec<&str> = named.into_iter().collect();
    format!(
        "Placeholder-definition correspondence auto-fail: {} carr{} a `True` \
placeholder Lean body, so the kernel forces a deterministic Correspondence \
Fail (no verifier ran). Correspondence cannot pass until the real definition \
is written in place of `:= True`.",
        nodes.join(", "),
        if nodes.len() == 1 { "ies" } else { "y" },
    )
}

pub fn worker_blocker_status_block(request: &WrapperRequest) -> WorkerBlockerStatusBlock {
    let blockers: Vec<&Blocker> = request.blockers.iter().collect();
    if blockers.is_empty() {
        return WorkerBlockerStatusBlock {
            md: "No live blockers.".to_owned(),
            sidecar_payload: None,
        };
    }
    let total = blockers.len();
    let counts_line = worker_blocker_kind_counts_line(&blockers);
    let deps_neighborhood = worker_blocker_deps_neighborhood(request);
    let active_node = request.active_node.as_ref().map(|n| n.as_str());
    let held_target = request.held_target.as_ref().map(|n| n.as_str());
    let (indices, note) =
        worker_blocker_select_actionable(&blockers, active_node, held_target, &deps_neighborhood);
    let actionable_table = worker_blocker_format_actionable_table(&indices, &blockers, &note);
    let header = format!(
        "{} live blocker(s). Counts by kind: {}. Reviewer comments above \
describe what to repair; this list shows the live verifier blockers for \
situational awareness.",
        total, counts_line
    );

    let inline_limit = worker_blocker_inline_limit();
    if total <= inline_limit {
        let rows: Vec<String> = blockers
            .iter()
            .enumerate()
            .map(|(i, b)| worker_blocker_format_row(i, b))
            .collect();
        let mut md = String::new();
        md.push_str(&header);
        md.push('\n');
        md.push('\n');
        md.push_str("Index | Kind             | Object");
        md.push('\n');
        md.push_str("------|------------------|");
        md.push_str(&"-".repeat(32));
        md.push('\n');
        for r in rows.iter() {
            md.push_str(r);
            md.push('\n');
        }
        md.push('\n');
        md.push_str(&actionable_table);
        let placeholder_note = worker_blocker_placeholder_definition_note(&blockers, request);
        if !placeholder_note.is_empty() {
            md.push('\n');
            md.push('\n');
            md.push_str(&placeholder_note);
        }
        return WorkerBlockerStatusBlock {
            md,
            sidecar_payload: None,
        };
    }

    // Overflow: emit the actionable subset inline with a `{sidecar_path}`
    // placeholder, and the structured payload the bridge writes to disk.
    let synth_choices: Vec<Value> = blockers
        .iter()
        .map(|b| {
            json!({
                "id": "(worker view: id only emitted in review contract)",
                "blocker": *b,
            })
        })
        .collect();
    let mut md = String::new();
    md.push_str(&header);
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "Live blocker count ({}) exceeds the inline limit ({}); full \
structured blocker list is on disk so the actionable subset stays visible \
inline.",
        total, inline_limit
    );
    md.push('\n');
    md.push('\n');
    md.push_str(&actionable_table);
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "Full blocker list sidecar: `{}`",
        WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER
    );
    md.push('\n');
    md.push('\n');
    md.push_str(
        "Sidecar shape: `{\"blocker_choices\": [{\"id\": ..., \"blocker\": ...}, ...]}`. \
Workers do not echo blocker `id`s back; this file exists so you can inspect any \
blocker beyond the inline actionable subset if the reviewer's comments reference it.",
    );
    let placeholder_note = worker_blocker_placeholder_definition_note(&blockers, request);
    if !placeholder_note.is_empty() {
        md.push('\n');
        md.push('\n');
        md.push_str(&placeholder_note);
    }
    WorkerBlockerStatusBlock {
        md,
        sidecar_payload: Some(json!({"blocker_choices": synth_choices})),
    }
}

// ----------------------------------------------------------------------------
// Reviewer-side blocker-choices block (Phase 2 of the 2026-06-04
// bridge-to-kernel migration). Mirrors the worker-side `_worker_blocker_status_block`
// shape (kernel emits `{md, sidecar_payload}`; the bridge writes the sidecar
// JSON to disk and substitutes `{sidecar_path}` plus `{context_json_path}`
// before splicing). Byte-equivalent to the bridge's
// `_format_blocker_choices_summary` for representative fixtures; see the
// snapshot tests under `kernel/tests/runtime_cli_snapshots.rs`.
// ----------------------------------------------------------------------------

/// Reviewer-side blocker choices block.
///
/// Same envelope as `WorkerBlockerStatusBlock` (the bridge writes the sidecar
/// JSON and substitutes placeholders before splicing). The reviewer-side md
/// additionally contains the `{context_json_path}` placeholder that the
/// bridge replaces with the kernel-emitted `<request_id>.context.json` path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewBlockerChoicesBlock {
    pub md: String,
    pub sidecar_payload: Option<Value>,
}

/// Literal placeholder inside `ReviewBlockerChoicesBlock::md` for the on-disk
/// `<request>.context.json` path. The bridge substitutes the real path before
/// splicing.
pub const REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER: &str = "{context_json_path}";

/// Format a reviewer-facing 4-column row (includes the fingerprint-encoded
/// blocker `id`). Mirrors the bridge's `_format_blocker_row(include_id=True)`.
fn review_blocker_format_row_with_id(index: usize, choice: &Value) -> String {
    let blocker = choice.get("blocker");
    let kind = blocker
        .and_then(|b| b.get("kind"))
        .and_then(|k| k.as_str())
        .unwrap_or("?");
    let label = blocker
        .map(|b| {
            let obj = b.get("object");
            let otype = obj
                .and_then(|o| o.get("otype"))
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            let body = obj
                .and_then(|o| {
                    o.get("node")
                        .or_else(|| o.get("target"))
                        .or_else(|| o.get("id"))
                        .or_else(|| o.get("name"))
                })
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            format!("{otype}:{body}")
        })
        .unwrap_or_else(|| "?:?".to_owned());
    let bid = choice.get("id").and_then(|s| s.as_str()).unwrap_or("?");
    format!("{:5} | {:16} | {:32} | id={}", index, kind, label, bid)
}

/// Format a reviewer-facing 3-column row (no `id` column).
/// Mirrors the bridge's `_format_blocker_row(include_id=False)`.
fn review_blocker_format_row_no_id(index: usize, choice: &Value) -> String {
    let blocker = choice.get("blocker");
    let kind = blocker
        .and_then(|b| b.get("kind"))
        .and_then(|k| k.as_str())
        .unwrap_or("?");
    let label = blocker
        .map(|b| {
            let obj = b.get("object");
            let otype = obj
                .and_then(|o| o.get("otype"))
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            let body = obj
                .and_then(|o| {
                    o.get("node")
                        .or_else(|| o.get("target"))
                        .or_else(|| o.get("id"))
                        .or_else(|| o.get("name"))
                })
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            format!("{otype}:{body}")
        })
        .unwrap_or_else(|| "?:?".to_owned());
    format!("{:5} | {:16} | {}", index, kind, label)
}

/// Counts-by-kind line for the reviewer; mirrors the bridge's
/// `_kind_counts_line` (which operates on the `blocker_choices` list and
/// drills through to `choice["blocker"]["kind"]`).
fn review_blocker_kind_counts_line(choices: &[Value]) -> String {
    if choices.is_empty() {
        return "(none)".to_owned();
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for choice in choices {
        let kind = choice
            .get("blocker")
            .and_then(|b| b.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or("?")
            .to_owned();
        *counts.entry(kind).or_insert(0) += 1;
    }
    let parts: Vec<String> = counts.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    parts.join(", ")
}

/// Reviewer-side actionable selection — parallel to
/// `worker_blocker_select_actionable` but operating on the `blocker_choices`
/// list (the bridge passes `deps_neighborhood=None` at the reviewer call
/// site, so we accept the same `Option`-shaped input here for parity).
fn review_blocker_select_actionable(
    choices: &[Value],
    active_node: Option<&str>,
    held_target: Option<&str>,
) -> (Vec<usize>, String) {
    let mut neighborhood: BTreeSet<&str> = BTreeSet::new();
    if let Some(n) = active_node {
        neighborhood.insert(n);
    }
    let target_focus = held_target;

    let mut matched: Vec<usize> = Vec::new();
    for (index, choice) in choices.iter().enumerate() {
        let blocker = match choice.get("blocker") {
            Some(b) => b,
            None => continue,
        };
        let obj = match blocker.get("object") {
            Some(o) => o,
            None => continue,
        };
        let otype = obj.get("otype").and_then(|s| s.as_str()).unwrap_or("");
        match otype {
            "node" => {
                if let Some(node) = obj.get("node").and_then(|s| s.as_str()) {
                    if neighborhood.contains(node) {
                        matched.push(index);
                    }
                }
            }
            "target" => {
                if let (Some(target), Some(focus)) =
                    (obj.get("target").and_then(|s| s.as_str()), target_focus)
                {
                    if target == focus {
                        matched.push(index);
                    }
                }
            }
            _ => {}
        }
    }

    if !matched.is_empty() {
        return (matched, "active node + direct-dep neighborhood".to_owned());
    }

    let fallback_count = std::cmp::min(WORKER_BLOCKER_ACTIONABLE_FALLBACK_K, choices.len());
    if fallback_count == 0 {
        return (Vec::new(), "no live blockers".to_owned());
    }
    let mut sortable: Vec<(String, usize)> = choices
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let label = c
                .get("blocker")
                .map(|b| {
                    let obj = b.get("object");
                    let otype = obj
                        .and_then(|o| o.get("otype"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("?");
                    let body = obj
                        .and_then(|o| {
                            o.get("node")
                                .or_else(|| o.get("target"))
                                .or_else(|| o.get("id"))
                                .or_else(|| o.get("name"))
                        })
                        .and_then(|s| s.as_str())
                        .unwrap_or("?");
                    format!("{otype}:{body}")
                })
                .unwrap_or_else(|| "?:?".to_owned());
            (label, i)
        })
        .collect();
    sortable.sort();
    let indices: Vec<usize> = sortable
        .into_iter()
        .take(fallback_count)
        .map(|(_, i)| i)
        .collect();
    let note = format!(
        "fallback: no blockers touch active_node/held_target; showing \
first {} of {} by label order",
        fallback_count,
        choices.len()
    );
    (indices, note)
}

/// Render the reviewer-facing actionable-subset table (4-column with `id`).
fn review_blocker_format_actionable_table(
    indices: &[usize],
    choices: &[Value],
    note: &str,
) -> String {
    if indices.is_empty() {
        return format!("(none) -- {}", note);
    }
    let rows: Vec<String> = indices
        .iter()
        .filter_map(|&i| {
            choices
                .get(i)
                .map(|c| review_blocker_format_row_with_id(i, c))
        })
        .collect();
    let header_lines = [
        "Index | Kind             | otype:body                       | id".to_owned(),
        "------|------------------|----------------------------------|".to_owned()
            + &"-".repeat(40),
    ];
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Actionable subset ({} of {}): {}",
        rows.len(),
        choices.len(),
        note
    );
    out.push('\n');
    for line in header_lines.iter() {
        out.push_str(line);
        out.push('\n');
    }
    for (i, row) in rows.iter().enumerate() {
        out.push_str(row);
        if i + 1 < rows.len() {
            out.push('\n');
        }
    }
    out
}

/// Render the reviewer-facing blocker-choices block.
///
/// Byte-equivalent to the bridge's `_format_blocker_choices_summary` for
/// representative inputs. In the overflow case, `md` contains
/// `{sidecar_path}` (sidecar location) and in both cases it contains
/// `{context_json_path}` (the kernel-written context.json). The bridge
/// substitutes both placeholders with concrete on-disk paths before
/// splicing.
pub fn review_blocker_choices_block(request: &WrapperRequest) -> ReviewBlockerChoicesBlock {
    // Compute the choices list the same way `review_contract_payload` does
    // (so we work off the same `id`s the reviewer sees in the contract).
    let raw_choices = blocker_choices(&request.blockers);
    let choices: Vec<Value> = raw_choices.iter().map(|c| json!(c)).collect();
    let total = choices.len();
    let counts_line = review_blocker_kind_counts_line(&choices);
    let active_node = request.active_node.as_ref().map(|n| n.as_str());
    let held_target = request.held_target.as_ref().map(|n| n.as_str());
    let (indices, note) = review_blocker_select_actionable(&choices, active_node, held_target);
    let actionable_table = review_blocker_format_actionable_table(&indices, &choices, &note);
    let header = format!(
        "{} blocker choices total. Counts by kind: {}",
        total, counts_line
    );

    let inline_limit = worker_blocker_inline_limit();
    if total <= inline_limit {
        // Small enough -- inline everything (no sidecar needed).
        let rows: Vec<String> = choices
            .iter()
            .enumerate()
            .map(|(i, c)| review_blocker_format_row_no_id(i, c))
            .collect();
        let mut md = String::new();
        md.push_str(&header);
        md.push('\n');
        md.push('\n');
        md.push_str("Index | Kind             | Object");
        md.push('\n');
        md.push_str("------|------------------|");
        md.push_str(&"-".repeat(32));
        md.push('\n');
        for r in rows.iter() {
            md.push_str(r);
            md.push('\n');
        }
        if total > 0 {
            md.push('\n');
            md.push_str(&actionable_table);
            md.push('\n');
        }
        md.push('\n');
        md.push_str("Full structured blocker data (with the fingerprint-encoded `id`");
        md.push('\n');
        md.push_str("field that you must echo back verbatim if you select a blocker)");
        md.push('\n');
        let _ = write!(
            md,
            "lives at: {}",
            REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER
        );
        md.push('\n');
        md.push('\n');
        md.push_str("List every blocker `id`:");
        md.push('\n');
        md.push('\n');
        let _ = write!(
            md,
            "  jq -r '.review_blocker_choices[].id' {}",
            REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER
        );
        md.push('\n');
        md.push('\n');
        md.push_str("Read one full blocker by index:");
        md.push('\n');
        md.push('\n');
        let _ = write!(
            md,
            "  jq '.review_blocker_choices[N]' {}",
            REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER
        );
        return ReviewBlockerChoicesBlock {
            md,
            sidecar_payload: None,
        };
    }

    // Overflow path — emit actionable subset + sidecar pointer + context
    // pointer. The bridge writes the sidecar JSON and substitutes both
    // placeholders.
    let mut md = String::new();
    md.push_str(&header);
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "Live blocker count ({}) exceeds the inline limit ({}); full \
structured blocker list is moved to a sidecar so the actionable subset \
stays visible inline.",
        total, inline_limit
    );
    md.push('\n');
    md.push('\n');
    md.push_str(&actionable_table);
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "Full blocker list sidecar: `{}`",
        WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER
    );
    md.push('\n');
    md.push('\n');
    md.push_str(
        "The sidecar JSON has the shape \
`{\"blocker_choices\": [{\"id\": ..., \"blocker\": ...}, ...]}`. \
Use blocker `id`s for task/override/reset lists; use node ids for \
`request_sound_verifier_node_ids`.",
    );
    md.push('\n');
    md.push('\n');
    md.push_str("List every blocker `id` from the sidecar:");
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "  jq -r '.blocker_choices[].id' {}",
        WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER
    );
    md.push('\n');
    md.push('\n');
    md.push_str("Read one full blocker by index:");
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "  jq '.blocker_choices[N]' {}",
        WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER
    );
    md.push('\n');
    md.push('\n');
    let _ = write!(
        md,
        "The original kernel context.json also has the same data under \
`.review_blocker_choices`, mirrored at {}.",
        REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER
    );
    ReviewBlockerChoicesBlock {
        md,
        sidecar_payload: Some(json!({"blocker_choices": choices})),
    }
}

pub fn worker_contract_payload(request: &WrapperRequest, repo_path: Option<&Path>) -> Value {
    if request.kind != crate::model::RequestKind::Worker {
        return no_worker_contract_payload();
    }
    // Backend-selection hook (Option A). Resolve the tablet's target once and
    // thread it into the worker fragment selector so the Lean-specific slots
    // pick their Isabelle siblings under IsabelleHol. With no repo path (or an
    // unresolved/absent config), `tablet_target_for_repo` yields `Lean`, so the
    // emitted fragment vec stays byte-identical to the pre-hook Lean path.
    let target = repo_path
        .map(crate::worker_normalization::tablet_target_for_repo)
        .unwrap_or(crate::backend::BackendId::Lean);

    let validation_kind = request.worker_acceptance.validation_kind;
    let existing_node_scope_mode = match validation_kind {
        WorkerValidationKind::TheoremGlobal | WorkerValidationKind::Cleanup => "all_present",
        // Cleanup-v2 (audit Finding 5): FinalCleanup's worker-visible
        // scope is `pending_task.authorized_nodes ∪ {target_node}` for
        // Substitution and `{target_node}` for LintFix (see
        // `current_worker_authorized_nodes` at `model.rs:5898`). Both are
        // exactly-matching whitelists, not all_present. Map to
        // `authorized_existing_nodes` so the rendered scope contract
        // matches what the runtime validator enforces. Legacy lint-only
        // mode (no active cleanup task) still falls through the same
        // mode label — its scope is the active node only in practice,
        // which is a degenerate single-element whitelist.
        WorkerValidationKind::FinalCleanup => "authorized_existing_nodes",
        WorkerValidationKind::TheoremTargeted
        | WorkerValidationKind::TheoremRestructure
        | WorkerValidationKind::ProofRestructure
        | WorkerValidationKind::ProofCoarseRestructure => "authorized_existing_nodes",
        WorkerValidationKind::ProofEasy | WorkerValidationKind::ProofLocal => "active_node_only",
        WorkerValidationKind::None => "none",
    };
    let new_nodes_allowed = worker_new_nodes_allowed(request);
    let cleanup_worker = cleanup_like_worker(request);
    // A1/A2/A6/A7/A8: build worker_context_payload conditionally — drop
    // proof-formalization-only knobs (active_difficulty, active_easy_attempts)
    // during TheoremStating and drop the reviewer-to-runner directive
    // next_context_mode from the worker payload entirely (the worker can't
    // act on it).
    let worker_context_payload = {
        let mut map = serde_json::Map::new();
        map.insert("enabled".to_owned(), json!(request.worker_context.enabled));
        if !request.phase.is_theorem_stating_like() {
            map.insert(
                "active_difficulty".to_owned(),
                json!(request.worker_context.active_difficulty),
            );
            map.insert(
                "active_easy_attempts".to_owned(),
                json!(request.worker_context.active_easy_attempts),
            );
        }
        map.insert(
            "worker_profile".to_owned(),
            json!(request.worker_context.worker_profile),
        );
        map.insert(
            "validation_kind".to_owned(),
            json!(request.worker_context.validation_kind),
        );
        map.insert(
            "authorized_nodes".to_owned(),
            json!(request.worker_context.authorized_nodes),
        );
        map.insert(
            "allow_new_obligations".to_owned(),
            json!(request.worker_context.allow_new_obligations),
        );
        map.insert(
            "must_close_active".to_owned(),
            json!(request.worker_context.must_close_active),
        );
        if !request
            .worker_context
            .protected_semantic_change_nodes
            .is_empty()
        {
            map.insert(
                "protected_semantic_change_nodes".to_owned(),
                json!(request.worker_context.protected_semantic_change_nodes),
            );
        }
        // Trim 10: omit paper_focus_ranges / work_style_hint when they
        // hold their default values. Mirrors the existing
        // `worker_has_meaningful_routing_hints` predicate that already
        // gates the `worker/common/33_routing_hints.md` fragment — when
        // that fragment is absent, these two fields have no rendered
        // counterpart and shouldn't show up as default-valued JSON noise.
        // Inserted only when set to non-default values.
        if !request.worker_context.paper_focus_ranges.is_empty() {
            map.insert(
                "paper_focus_ranges".to_owned(),
                json!(request.worker_context.paper_focus_ranges),
            );
        }
        if request.worker_context.work_style_hint != crate::model::WorkerWorkStyleHint::None {
            map.insert(
                "work_style_hint".to_owned(),
                json!(request.worker_context.work_style_hint),
            );
        }
        // Cleanup-v2 (audit Finding 5): surface the active cleanup task's
        // view fields so the substitution / lintfix worker prompts can
        // render `target_node`, the task kind (with its embedded
        // replacement / warning_text payload), and the audit rationale.
        // Pre-fix these fields lived on `WorkerContext` but were never
        // serialized into the worker JSON, so the prompt fragments
        // referenced fields the worker couldn't see. Inserted only when
        // populated (None on non-cleanup-v2 / legacy lint-only workers).
        if let Some(kind) = &request.worker_context.cleanup_active_task_kind_view {
            map.insert("cleanup_active_task_kind".to_owned(), json!(kind));
        }
        if let Some(target) = &request.worker_context.cleanup_active_target_node_view {
            map.insert("cleanup_active_target_node".to_owned(), json!(target));
        }
        if !request
            .worker_context
            .cleanup_active_rationale_view
            .is_empty()
        {
            map.insert(
                "cleanup_active_rationale".to_owned(),
                json!(request.worker_context.cleanup_active_rationale_view),
            );
        }
        if !request.worker_context.cleanup_active_batch_view.is_empty() {
            map.insert(
                "cleanup_active_batch".to_owned(),
                json!(request.worker_context.cleanup_active_batch_view),
            );
        }
        if let Some(kind) = &request.worker_context.cleanup_active_batch_kind_view {
            map.insert("cleanup_active_batch_kind".to_owned(), json!(kind));
        }
        Value::Object(map)
    };
    let mut scope_contract = serde_json::Map::new();
    scope_contract.insert(
        "existing_node_scope_mode".to_owned(),
        json!(existing_node_scope_mode),
    );
    scope_contract.insert(
        "authorized_existing_nodes".to_owned(),
        json!(request.worker_acceptance.authorized_nodes),
    );
    scope_contract.insert(
        "configured_targets".to_owned(),
        json!(request.configured_targets),
    );
    if !request.configured_challenge_targets.is_empty() {
        scope_contract.insert(
            "configured_challenge_targets".to_owned(),
            challenge_registry_json(request),
        );
        scope_contract.insert(
            "challenge_coverage".to_owned(),
            challenge_coverage_json(request),
        );
        scope_contract.insert(
            "challenge_claim_rules".to_owned(),
            json!(CHALLENGE_CLAIM_RULES),
        );
    }
    if !request.blocked_targets.is_empty() {
        scope_contract.insert("pending_targets".to_owned(), json!(request.blocked_targets));
        scope_contract.insert(
            "pending_targets_meaning".to_owned(),
            json!("targets_lacking_current_approved_support"),
        );
    }
    scope_contract.insert("new_nodes_allowed".to_owned(), json!(new_nodes_allowed));
    scope_contract.insert(
        "allow_new_obligations".to_owned(),
        json!(request.worker_context.allow_new_obligations),
    );
    scope_contract.insert(
        "must_close_active".to_owned(),
        json!(request.worker_context.must_close_active),
    );
    scope_contract.insert(
        "proof_obligation_controls_meaning".to_owned(),
        json!("allow_new_obligations=false requires every new helper node to be mechanically closed (no `sorry`, no unapproved oracle or axiom); must_close_active=true requires the active node to be mechanically closed"),
    );
    if !request
        .worker_acceptance
        .protected_semantic_change_nodes
        .is_empty()
    {
        scope_contract.insert(
            "protected_semantic_change_nodes".to_owned(),
            json!(request.worker_acceptance.protected_semantic_change_nodes),
        );
        scope_contract.insert(
            "protected_semantic_change_nodes_meaning".to_owned(),
            json!("only_these_approved_target_or_protected_closure_nodes_may_have_correspondence_reopened"),
        );
    }
    if !request.phase.is_theorem_stating_like() {
        // Nodes present at the end of theorem-stating. Changing any of
        // their declaration signatures (hypotheses / return type) requires
        // `coarse_restructure` mode; plain `restructure` only unlocks
        // signatures of nodes added later in proof-formalization. Empty
        // on legacy runs — the checker then treats every node as coarse
        // to preserve prior behaviour. Omitted entirely during
        // theorem-stating: the coarse set is conceptually nonexistent
        // until the theorem-stating → proof-formalization transition
        // computes it.
        scope_contract.insert(
            "coarse_dag_nodes".to_owned(),
            json!(request.coarse_dag_nodes),
        );
        scope_contract.insert(
            "coarse_dag_nodes_meaning".to_owned(),
            json!("signature_edits_require_coarse_restructure"),
        );
    }
    let mut prompt_schema_example = serde_json::Map::new();
    prompt_schema_example.insert(
        "outcome".to_owned(),
        if cleanup_worker {
            json!("valid")
        } else if request.is_pv {
            // PV under-model (Slice 1): advertise the extra Decide-specific
            // outcome only for PV (math mode stays byte-identical).
            json!("valid / invalid / stuck / needs_restructure / target_false_under_model")
        } else {
            json!("valid / invalid / stuck / needs_restructure")
        },
    );
    prompt_schema_example.insert("summary".to_owned(), json!("brief summary"));
    prompt_schema_example.insert("comments".to_owned(), json!("optional short note"));
    prompt_schema_example.insert(
        "deleted_nodes".to_owned(),
        if cleanup_worker {
            json!([])
        } else {
            json!(["removed_tablet_node_id"])
        },
    );
    if !cleanup_worker || new_nodes_allowed {
        prompt_schema_example.insert(
            "target_claim_updates".to_owned(),
            if cleanup_worker {
                json!({"new_helper_node_id": []})
            } else {
                json!({"node_id": ["target_id"]})
            },
        );
        if !request.configured_challenge_targets.is_empty() {
            prompt_schema_example.insert(
                "challenge_claim_updates".to_owned(),
                if cleanup_worker {
                    json!({"new_helper_node_id": []})
                } else {
                    json!({"node_id": ["challenge_target_id"]})
                },
            );
        }
    }
    if !cleanup_worker {
        if request.trust_base_required_v1
            && request.phase == Phase::ProofFormalization
            && request.work_kind == WorkerWorkKind::Standard
        {
            if let Some(target) = active_disprove_primary(request) {
                prompt_schema_example.insert(
                    "rust_witness_artifact".to_owned(),
                    json!({
                        "target_id": target,
                        "relative_path": crate::trust_base::rust_witness_relative_path(target),
                    }),
                );
            }
        }
        prompt_schema_example.insert(
            "difficulty_updates".to_owned(),
            json!({"node_id": "easy or hard"}),
        );
        if request.is_pv && request.trust_base_required_v1 {
            prompt_schema_example.insert(
                "deviation_requests".to_owned(),
                json!({
                  "seam_repair_deviation_id": {
                    "path": "reference/seam_repair_deviation_id.tex",
                    "summary": "adapt the pinned crate and re-establish its contract",
                    "affected_nodes": ["node_id"],
                    "seam_repair": {
                        "seam_class": "i | ii | iii",
                        "paths": [{"path": "crate/src/lib.rs", "before_sha256": "hex64 (omit when the adaptation predates the pinned tree)", "after_sha256": "hex64"}],
                        "citation": {"kind": "language_guarantee | upstream_reference | harness_receipt", "statement": "seam (i) language guarantee"},
                        "executable_reattachment": {"description": "REQUIRED for seam (ii): the executable re-attachment", "harness_receipt_sha256": "hex64"},
                        "affected_targets": ["challenge_target_id"],
                        "tool_identity_effect": {"kind": "unaffected | repin | reattest_byte_identical", "runner_sha256": "hex64 (repin only: the rebuilt tool digest)"},
                    }
                  }
                }),
            );
        } else {
            prompt_schema_example.insert(
                "deviation_requests".to_owned(),
                json!({"deviation_id": {"path": "reference/path.tex", "summary": "departure and return argument", "affected_nodes": ["node_id"]}}),
            );
        }
        // Trust-required PV theorem/proof workers with an actionable Decide
        // target may propose or withdraw a conditional generation.
        if conditional_worker_carrier_available(request) {
            let example_target = request
                .configured_challenge_targets
                .iter()
                .find(|(_, spec)| spec.resolution == crate::model::ChallengeResolution::Decide)
                .map(|(id, _)| id.as_str())
                .expect("conditional carrier availability requires a Decide target");
            prompt_schema_example.insert(
                "conditional_theorem_proposal".to_owned(),
                json!({
                    "target_id": example_target,
                    "condition_lean": "complete Lean Prop under the registered target binders",
                    "condition_informal": "concise condition",
                    "rationale": "why this is the strongest established result",
                    "trigger": "model_counterexample_mismatch | assumptions_model_gap | unconditional_not_established",
                    "evidence": {
                        "disproof_sha256": "optional hex64",
                        "artifact_sha256": "optional hex64",
                        "assumption_ids": ["optional existing approved assumption id"]
                    },
                    "existing_under_model_assumption_id": null,
                    "concrete_counterexample_arguments": null
                }),
            );
            prompt_schema_example.insert(
                "conditional_theorem_withdrawals".to_owned(),
                json!([example_target]),
            );
        }
        prompt_schema_example.insert(
            "node_deviation_claims".to_owned(),
            json!({"node_id": ["authorized_deviation_id"]}),
        );
        prompt_schema_example.insert(
            "deviation_deletions".to_owned(),
            json!(["deviation_id_to_retire"]),
        );
        prompt_schema_example.insert(
            "needs_restructure_suggested_nodes".to_owned(),
            json!([
                "REQUIRED (non-empty) when outcome=needs_restructure: names of existing Tablet nodes the reviewer should consider authorizing on the next dispatch — i.e. the nodes you needed to edit but couldn't under the current scope. Empty/absent for other outcomes."
            ]),
        );
        // PV under-model (Slice 1): the carriers for the `target_false_under_model`
        // outcome. PV-only (math mode never advertises them).
        if request.is_pv {
            prompt_schema_example.insert(
                "under_model_disproof".to_owned(),
                json!("REQUIRED (non-empty) when outcome=target_false_under_model: the NL disproof — the witness x0 that falsifies the Decide target T against the Aeneas Lean model, plus why it breaks the spec. Empty/absent for other outcomes. You author NO tablet edit and CANNOT mint an assumption; you only report the witness."),
            );
            prompt_schema_example.insert(
                "under_model_route_opinion".to_owned(),
                json!("REQUIRED (non-empty) when outcome=target_false_under_model: your route opinion — \"flip\" (x0 is a real, constructible Rust input ⇒ T is genuinely false ⇒ disprove) vs \"model-deviation\" (x0 is only reachable by violating a Rust language invariant Aeneas dropped). Advisory; the auditor rules the route."),
            );
            prompt_schema_example.insert(
                "under_model_reasoning".to_owned(),
                json!("optional: your reasoning behind the disproof and route opinion."),
            );
        }
        // PV under-model (Slice 2): the authored-assumption carriers, advertised
        // ONLY on an authoring burst.
        if request.is_pv && request.assumption_authoring.is_some() {
            prompt_schema_example.insert(
                "authored_assumption_id".to_owned(),
                json!("REQUIRED on an assumption-authoring burst: a stable ASCII id used in the paired staged block markers in Tablet/Assumptions.lean and Tablet/Assumptions.tex."),
            );
            prompt_schema_example.insert(
                "authored_axiom_name".to_owned(),
                json!("REQUIRED on an assumption-authoring burst: the namespace-qualified Lean axiom name for the auditor-named candidate C."),
            );
            prompt_schema_example.insert(
                "authored_citation_locator".to_owned(),
                json!("REQUIRED: a RESOLVABLE locator into the fetched Rust-docs corpus that EXISTS and guarantees C."),
            );
            prompt_schema_example.insert(
                "authored_rust_justification".to_owned(),
                json!("REQUIRED: why that corpus section guarantees C as a Rust language/compiler guarantee and how it maps to the Lean statement."),
            );
            prompt_schema_example.insert(
                "authored_claim_class".to_owned(),
                json!("\"behavior\" (hook-conditioned facts about operations on valid values) or \"domain\" (language-guaranteed existence/coverage of valid values; the statement contains an existential hook-valid witness). Empty defaults to behavior."),
            );
        }
    }
    // On-demand audit (advisory): advertise `audit_request` exactly when
    // the worker `46_audit_request.md` fragment is shown — i.e. whenever
    // the phase admits a StuckMathAudit (`record_latest_worker_rationale`
    // drops it otherwise). Mirrors the reviewer contract surfacing
    // `audit_request` in its `optional_fields`.
    if request.phase_admits_stuck_math_audit() {
        prompt_schema_example.insert(
            "audit_request".to_owned(),
            json!({"reason_kind": "approach / suspect_report", "reason": "concise problem statement"}),
        );
    }
    // Process memory (spec §5): the challenge channel, advertised on the
    // same runtime-resolved terms as the reviewer's (only when
    // `process-memory/` holds an active entry). The worker contract has no
    // `optional_fields` vec — like `audit_request` above, the schema
    // example IS the advertisement.
    if request.process_memory_active {
        prompt_schema_example.insert(
            "memory_challenges".to_owned(),
            json!([{"entry_id": "active process-memory entry id your evidence contradicts", "reason": "what contradicts it; the next audit adjudicates"}]),
        );
    }
    // Trims for prompt-token economy. Worker prompts repeatedly emitted
    // ~75KB of structural JSON dominated by mostly-empty / out-of-scope
    // entries; these three filters typically remove ~50KB without
    // dropping any context the worker actually needs.
    //
    // 1) `current_target_claims_nonempty`: only nodes that cover at
    //    least one configured paper target. The full map's empty
    //    entries are noise.
    let target_claims_nonempty: BTreeMap<&NodeId, &BTreeSet<crate::model::TargetId>> = request
        .current_target_claims
        .iter()
        .filter(|(_, targets)| !targets.is_empty())
        .collect();
    // 2) `current_deps_scoped` is a partial view of `current_deps`,
    //    keyed by `WrapperRequest::worker_prompt_dag_scope` (the
    //    bidirectional closure of {active_node, authorized_nodes,
    //    blocker-referenced nodes}). The worker sees deps for nodes
    //    in their authorized region; whole-tablet validation kinds
    //    fall back to the full DAG via the helper. The companion
    //    field `current_deps_scope_nodes` lists exactly which nodes'
    //    deps were emitted, so the worker can tell at a glance
    //    whether a node they're considering is in the visible
    //    portion. NOTE: this is intentionally NOT named
    //    `current_deps` — that name implied the full DAG; readers
    //    should use the suffix to know the view is partial.
    let dag_scope = request.worker_prompt_dag_scope();
    let scoped_deps: BTreeMap<&NodeId, &BTreeSet<NodeId>> = request
        .current_deps
        .iter()
        .filter(|(node, _)| dag_scope.contains(*node))
        .collect();
    // 3) `current_definition_nodes` instead of `current_proof_nodes`:
    //    definitions are the minority kind in most projects; absent
    //    entries default to "proof". Preamble is named separately
    //    via `current_preamble_node` (typically a single name) so the
    //    worker can derive proof_nodes = present − definition − preamble.
    let definition_nodes: BTreeSet<&NodeId> = request
        .current_node_kinds
        .iter()
        .filter(|(_, kind)| **kind == crate::model::NodeKind::Definition)
        .map(|(node, _)| node)
        .collect();
    let preamble_nodes: BTreeSet<&NodeId> = request
        .current_node_kinds
        .iter()
        .filter(|(_, kind)| **kind == crate::model::NodeKind::Preamble)
        .map(|(node, _)| node)
        .collect();
    let mut payload = json!({
        "prompt_fragments": route_fragments(worker_prompt_fragments(request, target), target),
        "reviewer_lean_product": request.stuck_math_audit.last_reviewer_lean_product.clone(),
        "audit_plan": request.audit_plan.clone(),
        // Option A widening: historical audit-plan snapshot for the
        // worker. Present iff there is no live `audit_plan` and the
        // kernel has a stash (either an inactive `state.audit_plan` or
        // a `superseded_audit_plan`). Advisory-only; the worker has no
        // dismiss affordance. The `34d_last_audit_plan.md` prompt
        // fragment frames the snapshot as historical context, not as
        // an actionable plan.
        "previous_audit_plan_snapshot": request.previous_audit_plan_snapshot.clone(),
        // Worker-prompt blocker-status block (process issue 4, 2026-05-22).
        // Kernel-rendered Markdown spliced by the bridge into the worker
        // prompt's `{{blocker_status_block}}` context variable. The bridge
        // also writes `sidecar_payload` (when Some) to
        // `<raw_output_path>.blockers.json` and substitutes the
        // `{sidecar_path}` placeholder in `md` with the actual path.
        "blocker_status": worker_blocker_status_block(request),
        "request_summary": {
            "phase": request.phase,
            "mode": request.mode,
            "active_node": request.active_node,
            "held_target": request.held_target,
            "fresh_context": request.fresh_context,
            "worker_context": worker_context_payload,
            "blockers": request.blockers,
            "shallow_coarse_closed_count": request.shallow_coarse_closed_count,
            "cycles_since_shallow_coarse_closed_count_increase": request.cycles_since_shallow_coarse_closed_count_increase,
            "current_present_nodes": request.current_present_nodes,
            "current_definition_nodes": definition_nodes,
            "current_preamble_nodes": preamble_nodes,
            "current_deps_scoped": scoped_deps,
            "current_deps_scope_nodes": dag_scope,
            "current_target_claims_nonempty": target_claims_nonempty,
            "authorized_deviations": request.authorized_deviations,
            // Deviations the Deviation lane rejected. A node claiming one of
            // these carries a substantiveness Fail blocker the worker is
            // seated to repair; surface the rejected set explicitly so the
            // worker recovers by EITHER dropping the claim
            // (`node_deviation_claims`) OR revising the deviation `.tex`
            // (re-opening the Deviation lane), rather than only inferring the
            // rejection from `node_deviation_claims \ authorized_deviations`.
            "rejected_deviations": request.rejected_deviations,
            "current_deviation_files": request.current_deviation_files,
            "node_deviation_claims": request.node_deviation_claims,
            // Mirrors review_contract_payload's request_summary so the bridge
            // prompt's `{{deterministic_worker_rejection_reasons_json}}`
            // placeholder (filled from `request_summary.get(...)`) actually
            // surfaces the rejection text on retry. Without this field the
            // 32_deterministic_worker_rejection.md fragment renders to
            // `[]` and the retrying worker gets the pointer to last_invalid
            // but no content — the failure mode being a retrying worker
            // forced to read metadata.json off disk to learn why its prior
            // attempt was rejected, which it generally won't do.
            "deterministic_worker_rejection_reasons": request.deterministic_worker_rejection_reasons,
            "acceptance_logic_identity": request.acceptance_logic_identity,
        },
        "reviewer_comments": request.reviewer_comments,
        "result_type": "worker_result_v1",
        "kernel_derives_structural_snapshot": true,
        "allowed_outcomes": if cleanup_worker {
            json!(["valid", "invalid"])
        } else if request.is_pv {
            // PV under-model (Slice 1): the extra Decide-specific outcome is
            // PV-only — math mode keeps the byte-identical four-outcome list.
            json!(["valid", "invalid", "stuck", "needs_restructure", "target_false_under_model"])
        } else {
            json!(["valid", "invalid", "stuck", "needs_restructure"])
        },
        "invalid_outcome_contract": {
            "field": "invalid_kind",
            "allowed_values": [
                "implementation_invalid",
                "task_infeasible",
                "checker_rejected",
                "contract_conflict",
            ],
            "meaning": "classifies a worker-declared invalid result for the next reviewer",
        },
        "reported_delta_fields": if cleanup_worker && new_nodes_allowed && !request.configured_challenge_targets.is_empty() {
            json!(["target_claim_updates", "challenge_claim_updates", "deleted_nodes"])
        } else if cleanup_worker && new_nodes_allowed {
            json!(["target_claim_updates", "deleted_nodes"])
        } else if cleanup_worker {
            json!(["deleted_nodes"])
        } else if request.configured_challenge_targets.is_empty() {
            json!(["target_claim_updates", "difficulty_updates", "deviation_requests", "node_deviation_claims", "deviation_deletions", "deleted_nodes"])
        } else if active_disprove_primary(request).is_some() && request.trust_base_required_v1 {
            json!(["target_claim_updates", "challenge_claim_updates", "difficulty_updates", "deviation_requests", "rust_witness_artifact", "node_deviation_claims", "deviation_deletions", "deleted_nodes"])
        } else {
            json!(["target_claim_updates", "challenge_claim_updates", "difficulty_updates", "deviation_requests", "node_deviation_claims", "deviation_deletions", "deleted_nodes"])
        },
        // A6: forbidden_legacy_fields removed (2-year-old migration relic).
        // A7: next_context_mode removed from worker payload — it's a
        //     reviewer-to-runner directive the worker can't act on.
        "prompt_schema_example": Value::Object(prompt_schema_example),
        "scope_contract": Value::Object(scope_contract),
        "stuck_contract": {
            "allowed": !cleanup_worker,
            "forbid_tablet_changes_when_stuck": request.worker_acceptance.forbid_tablet_changes_when_stuck,
            "meaning": if cleanup_worker {
                json!("none")
            } else {
                json!("cannot_make_progress_on_pending_work_under_current_scope")
            },
        },
        "needs_restructure_contract": {
            "allowed": !cleanup_worker,
            // Post Stuck/NR rule removal: the kernel honours
            // NeedsRestructure regardless of tablet deltas — the
            // engine's restore_committed + RestoreWorktreeToActiveWorkerBase
            // path rolls any in-progress work back. The worker may still
            // bundle structural changes when reporting NR, but those
            // changes are discarded; the verdict signal is what matters.
            "forbid_tablet_changes_when_needs_restructure": false,
            "meaning": if cleanup_worker {
                json!("none")
            } else {
                json!("worker_can_name_broader_restructure_needed_but_current_scope_does_not_authorize_it")
            },
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-worker-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{acceptance_context_path}}",
            "--raw-only",
        ], &[
            "python3",
            "{{check_script_path}}",
            "trellis-worker-result",
            "{{raw_output_path}}",
            "--repo",
            "{{repo_path}}",
            "--context-json",
            "{{acceptance_context_path}}",
        ]),
    });
    if cleanup_worker {
        if let Some(summary) = payload
            .get_mut("request_summary")
            .and_then(Value::as_object_mut)
        {
            summary.insert(
                "cleanup_transition_obligation".to_owned(),
                json!({
                    "policy": "same_burst_responsibility",
                    "baseline_live_orphans": request.current_orphan_nodes,
                    "required_postcondition": "post live orphan nodes must be a subset of baseline live orphan nodes",
                    "baseline_formalization_valid": request.cleanup_baseline_formalization_valid,
                    "formalization_postcondition": "a valid baseline must remain valid",
                }),
            );
        }
    }
    if let Some(authoring) = request.assumption_authoring.as_ref() {
        if request.is_pv {
            if let Some(summary) = payload
                .get_mut("request_summary")
                .and_then(|v| v.as_object_mut())
            {
                summary.insert(
                    "assumption_authoring".to_owned(),
                    json!({
                        "candidate_invariant": authoring.candidate_invariant,
                        "needed_by": authoring.needed_by,
                        "lean_axiom_policy": UNDER_MODEL_VALIDITY_HOOK_POLICY,
                        "theorem_statement_policy": UNDER_MODEL_THEOREM_STATEMENT_POLICY,
                    }),
                );
            }
        }
    }
    // Proposal v32 audit-2 followup #2 (post-fix): only surface the
    // active-coarse-anchor framing to the worker when the explanatory
    // fragment `worker/proof_formalization/08_coarse_anchor.md` is also
    // assembled. The fragment chooser at `proof_worker_gate_prompt_fragments`
    // gates on `request.active_coarse_node.is_some()`; mirror that here
    // so the JSON and the prompt text agree. Without alignment, a
    // ProofFormalization request with `active_coarse_node = None` (boot,
    // post-cone-clean, post-engine-fallback) would still emit
    // `"active_coarse_node": null, "coarse_repair_mode": false` literals
    // in `request_summary` with no fragment to explain them. The
    // bridge's `_drop_null_keys` doesn't run on `request_summary`
    // (rendered separately via `_json_fence`), and even if it did the
    // bool `false` would survive.
    if request.active_coarse_node.is_some() {
        if let Some(summary) = payload
            .get_mut("request_summary")
            .and_then(|v| v.as_object_mut())
        {
            summary.insert(
                "active_coarse_node".to_owned(),
                json!(request.active_coarse_node),
            );
            summary.insert(
                "coarse_repair_mode".to_owned(),
                json!(request.coarse_repair_mode),
            );
        }
    }
    // Challenge claims view, mirroring `current_target_claims_nonempty`.
    // Gated on a configured registry so paper-only runs emit no
    // challenge JSON noise.
    if !request.configured_challenge_targets.is_empty() {
        if let Some(summary) = payload
            .get_mut("request_summary")
            .and_then(|v| v.as_object_mut())
        {
            let challenge_claims_nonempty: BTreeMap<
                &NodeId,
                &BTreeSet<crate::model::ChallengeTargetId>,
            > = request
                .current_challenge_claims
                .iter()
                .filter(|(_, targets)| !targets.is_empty())
                .collect();
            summary.insert(
                "current_challenge_claims_nonempty".to_owned(),
                json!(challenge_claims_nonempty),
            );
        }
    }
    // Reference papers: registry + live claims + the payload-field schema
    // hint, gated on a configured registry so registry-free runs emit
    // byte-identical worker JSON. The registry here is the bridge's sole
    // path source for reference documents (state-carried, Amendment G5).
    if !request.configured_reference_papers.is_empty() {
        if let Some(summary) = payload
            .get_mut("request_summary")
            .and_then(|v| v.as_object_mut())
        {
            summary.insert(
                "reference_papers".to_owned(),
                json!(request.configured_reference_papers),
            );
            summary.insert(
                "node_reference_grounds".to_owned(),
                json!(request.node_reference_grounds),
            );
        }
        if !cleanup_worker {
            if let Some(schema) = payload
                .get_mut("prompt_schema_example")
                .and_then(|v| v.as_object_mut())
            {
                schema.insert(
                    "node_reference_grounds".to_owned(),
                    json!({"node_id": ["reference_paper_id"]}),
                );
            }
            if let Some(fields) = payload
                .get_mut("reported_delta_fields")
                .and_then(|v| v.as_array_mut())
            {
                fields.push(json!("node_reference_grounds"));
            }
        }
    }
    // Revision worker scope: surface the editable envelope, frozen set, node
    // dispositions, and removed targets so the worker need not reverse-engineer
    // them from kernel source. Present only in RevisionStating.
    if let Some(ctx) = request.revision_context.as_ref() {
        if let Some(summary) = payload
            .get_mut("request_summary")
            .and_then(|v| v.as_object_mut())
        {
            let removed_targets: BTreeSet<&crate::model::TargetId> = ctx
                .target_deltas
                .values()
                .filter(|d| d.kind == crate::model::RevisionTargetDeltaKind::Removed)
                .map(|d| &d.target)
                .collect();
            summary.insert(
                "revision_scope".to_owned(),
                json!({
                    "editable_nodes": ctx.editable_nodes,
                    "frozen_nodes": ctx.frozen_nodes,
                    "node_dispositions": ctx.node_dispositions,
                    "removed_targets": removed_targets,
                    "meaning": "an edited existing node must be in (authorized_existing_nodes ∩ editable_nodes); frozen nodes may not be edited or deleted; no covering node may claim a removed target",
                }),
            );
        }
    }
    // PV under-model (Slice 1): the `target_false_under_model` contract block.
    // Post-inserted and PV-gated so math-mode worker payloads stay byte-
    // identical (the auto-flip removal + this outcome are PV/Decide-specific).
    if request.is_pv && !cleanup_worker {
        if let Some(obj) = payload.as_object_mut() {
            let meaning = if request.trust_base_required_v1 {
                "emit only when you have a concrete witness that falsifies the Decide target T in the extracted Lean model. Report the witness and formal reasoning. The kernel first routes T to Disprove so the unrestricted refutation is checked. Source-level validation happens later under the seed-frozen claim-shape contract; do not guess reachability, run an ad-hoc Rust proxy, or propose an assumption."
            } else {
                "emit when you conclude the Decide target T is FALSE under the Aeneas Lean model (not merely hard). You author NO tablet edit and CANNOT mint an assumption — report the falsifying witness (under_model_disproof), a route opinion (under_model_route_opinion: flip vs model-deviation), and reasoning. The auditor is the SOLE adjudicator + SOLE flipper: it verifies your disproof in-lane and rules bug→disprove vs model-deviation→assumption."
            };
            obj.insert(
                "target_false_under_model_contract".to_owned(),
                json!({
                    "allowed": true,
                    "meaning": meaning,
                    "authors_tablet_edit": false,
                    "can_mint_assumption": false,
                }),
            );
        }
    }
    // PV under-model (Slice 2): the assumption-AUTHORING contract block. Present
    // ONLY on an authoring burst (`assumption_authoring` live), so every other
    // worker payload — math mode included — stays byte-identical.
    if request.is_pv && !cleanup_worker {
        if let Some(authoring) = request.assumption_authoring.as_ref() {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "authored_assumption_contract".to_owned(),
                    json!({
                        "candidate_invariant": authoring.candidate_invariant,
                        "needed_by": authoring.needed_by,
                        "meaning": "AUTHOR the auditor-named candidate Rust guarantee C by editing Tablet/Assumptions.lean and Tablet/Assumptions.tex. The Lean block must be between `-- BEGIN UNDERMODEL ASSUMPTION <authored_assumption_id>` and `-- END UNDERMODEL ASSUMPTION <authored_assumption_id>` and contain the verbatim `axiom <name> : <type>`. The TeX block must be between `% BEGIN UNDERMODEL ASSUMPTION <authored_assumption_id>` and `% END UNDERMODEL ASSUMPTION <authored_assumption_id>` and contain the paired NL statement. Emit JSON metadata only: authored_assumption_id, authored_axiom_name, authored_citation_locator, authored_rust_justification, and authored_claim_class (behavior | domain). When worker_acceptance does not name the Assumptions node, this block is planning context only.",
                        "lean_axiom_policy": UNDER_MODEL_VALIDITY_HOOK_POLICY,
                        "theorem_statement_policy": UNDER_MODEL_THEOREM_STATEMENT_POLICY,
                        "eligibility": "C must be a Rust language/compiler guarantee.",
                        "authors_assumption": true,
                        "gates_assumption": false,
                    }),
                );
            }
        }
    }
    payload
}

pub fn review_contract_payload(request: &WrapperRequest, repo_path: Option<&Path>) -> Value {
    if request.kind != crate::model::RequestKind::Review {
        return no_review_contract_payload();
    }
    let target = contract_target(repo_path);
    // A3: omit `coarse_dag_nodes` from request_summary during TheoremStating —
    // the coarse-DAG concept does not exist until the
    // theorem-stating → proof-formalization transition computes it.
    let mut request_summary = serde_json::Map::new();
    request_summary.insert("phase".to_owned(), json!(request.phase));
    request_summary.insert("mode".to_owned(), json!(request.mode));
    request_summary.insert("active_node".to_owned(), json!(request.active_node));
    request_summary.insert("held_target".to_owned(), json!(request.held_target));
    // Proposal v32 audit-2 followup #2: surface every active-coarse-anchor
    // field referenced by the reviewer prompt fragment
    // `review/common/30b_coarse_anchor.md`. Without these, the fragment
    // tells the reviewer to inspect fields that the rendered payload
    // didn't expose. Gated on ProofFormalization + non-empty
    // `coarse_dag_nodes` (post-fix): outside that regime every field is
    // at its inert default (None / false / 0 / empty set) and emitting
    // them is just prompt noise.
    if request.phase == Phase::ProofFormalization && !request.coarse_dag_nodes.is_empty() {
        request_summary.insert(
            "active_coarse_node".to_owned(),
            json!(request.active_coarse_node),
        );
        request_summary.insert(
            "kernel_hinted_next_active_coarse_nodes".to_owned(),
            json!(request.kernel_hinted_next_active_coarse_nodes),
        );
        request_summary.insert(
            "coarse_repair_mode".to_owned(),
            json!(request.coarse_repair_mode),
        );
        request_summary.insert(
            "cycles_in_coarse_repair_mode".to_owned(),
            json!(request.cycles_in_coarse_repair_mode),
        );
        request_summary.insert(
            "coarse_anchor_starvation_unlocked".to_owned(),
            json!(request.coarse_anchor_starvation_unlocked),
        );
    }
    request_summary.insert("invalid_attempt".to_owned(), json!(request.invalid_attempt));
    request_summary.insert(
        "retry_outcome_kind".to_owned(),
        json!(request.retry_outcome_kind),
    );
    request_summary.insert("retry_attempt".to_owned(), json!(request.retry_attempt));
    request_summary.insert(
        "human_input_outstanding".to_owned(),
        json!(request.human_input_outstanding),
    );
    request_summary.insert("blocked_targets".to_owned(), json!(request.blocked_targets));
    // Kernel-auto-scheduled Sound provenance: names the nodes whose sound
    // results this cycle came from kernel verification cadence (frontier /
    // stability backlog) rather than a reviewer request. Gated on
    // non-empty so reviews without such a dispatch emit byte-identical
    // JSON; the companion prompt fragment
    // `review/common/25a_kernel_scheduled_sound.md` is gated identically.
    if !request.kernel_scheduled_sound_review_nodes.is_empty() {
        request_summary.insert(
            "kernel_scheduled_sound_nodes".to_owned(),
            json!(request.kernel_scheduled_sound_review_nodes),
        );
    }
    // Challenge registry + coverage so the reviewer can route remaining
    // challenge targets. Each uncovered target carries a
    // ChallengeCoverage blocker; the rule string names the kernel-side
    // enforcement so a rejection is never chased into the wrong rule.
    if !request.configured_challenge_targets.is_empty() {
        request_summary.insert(
            "configured_challenge_targets".to_owned(),
            challenge_registry_json(request),
        );
        request_summary.insert(
            "challenge_coverage".to_owned(),
            challenge_coverage_json(request),
        );
        request_summary.insert(
            "challenge_claim_rules".to_owned(),
            json!(CHALLENGE_CLAIM_RULES),
        );
    }
    // Reference-paper registry + live claims, gated on a configured
    // registry so registry-free runs emit byte-identical reviewer JSON.
    // The reviewer needs the registry to cite ranges into a reference
    // document (`paper_focus_ranges[].doc`).
    if !request.configured_reference_papers.is_empty() {
        request_summary.insert(
            "reference_papers".to_owned(),
            json!(request.configured_reference_papers),
        );
        request_summary.insert(
            "node_reference_grounds".to_owned(),
            json!(request.node_reference_grounds),
        );
    }
    request_summary.insert(
        "cycles_since_clean".to_owned(),
        json!(request.cycles_since_clean),
    );
    request_summary.insert(
        "no_sound_progress_window_cycles".to_owned(),
        json!(request.no_sound_progress_window_cycles),
    );
    request_summary.insert(
        "shallow_coarse_closed_count".to_owned(),
        json!(request.shallow_coarse_closed_count),
    );
    request_summary.insert(
        "cycles_since_shallow_coarse_closed_count_increase".to_owned(),
        json!(request.cycles_since_shallow_coarse_closed_count_increase),
    );
    request_summary.insert(
        "last_clean_rewind_count".to_owned(),
        json!(request.last_clean_rewind_count),
    );
    if request.stuck_math_audit.active {
        request_summary.insert(
            "stuck_math_audit".to_owned(),
            json!(request.stuck_math_audit.clone()),
        );
    }
    // A StuckMathAudit exit must tell the immediate decision-maker why the
    // prior response was not accepted. Gated on non-empty so ordinary Review
    // summaries are unchanged apart from the contract version.
    if !request
        .latest_stuck_math_audit_rejection_reason
        .trim()
        .is_empty()
    {
        request_summary.insert(
            "latest_stuck_math_audit_rejection_reason".to_owned(),
            json!(request.latest_stuck_math_audit_rejection_reason),
        );
    }
    // Sidecar queue redesign (Q6/A4): the VALIDATED queue view — the
    // ordered queue projection + the state-only window flag — gated on
    // the runtime-resolved advertisement flag so sidecar-less runs emit
    // byte-identical reviewer JSON. State-derived ⇒ identical bytes on
    // a Malformed reissue. Attempt history / in-flight grunt status is
    // NEVER here (advisory fresh-read surface only).
    if request.sidecar_advertise_queue_fields {
        request_summary.insert(
            "sidecar_queue".to_owned(),
            json!(request
                .sidecar_queue
                .iter()
                .map(|entry| json!({
                    "node": entry.node,
                    "entry_seq": entry.entry_seq,
                    "queued_at_cycle": entry.queued_at_cycle,
                }))
                .collect::<Vec<_>>()),
        );
        request_summary.insert(
            "sidecar_window_open".to_owned(),
            json!(request.sidecar_window_open),
        );
    }
    // Option A: surface the live audit_plan in request_summary iff
    // it is dismissable. Otherwise emit the historical snapshot under
    // a distinct key so the reviewer prompt can render it as
    // advisory-only context.
    if review_audit_dismissal_legal(request) {
        if let Some(plan) = request.audit_plan.as_ref() {
            request_summary.insert("audit_plan".to_owned(), json!(plan));
        }
    } else if let Some(snapshot) = request.previous_audit_plan_snapshot.as_ref() {
        request_summary.insert("previous_audit_plan_snapshot".to_owned(), json!(snapshot));
    }
    // Surface the effective mandatory-LastClean threshold, and the rewind
    // count that waives it (`CSC_REWIND_WAIVER_COUNT`), so the reviewer
    // prompt fragment `review/common/32b_revert_last_clean_mandate.md` can
    // render both numbers without being separately edited when the
    // operator sets `TRELLIS_CSC_LAST_CLEAN_THRESHOLD`. The threshold is
    // computed by `csc_last_clean_threshold()` and matches what
    // `request_allowed_resets` actually enforces. The mandate is off by
    // default, and both keys appear only while it is on — the request
    // summary is rendered verbatim into the prompt (no null-dropping on
    // this block), so a placeholder value here would read to the reviewer
    // as a threshold the kernel enforces. The same predicate gates the
    // fragment, so prompt text and summary can never disagree.
    if let Some(csc_threshold) = crate::model::csc_last_clean_threshold() {
        request_summary.insert("csc_last_clean_threshold".to_owned(), json!(csc_threshold));
        request_summary.insert(
            "csc_rewind_waiver_count".to_owned(),
            json!(crate::model::CSC_REWIND_WAIVER_COUNT),
        );
    }
    {
        let mut rationale = serde_json::Map::new();
        rationale.insert("summary".to_owned(), json!(request.latest_worker_summary));
        rationale.insert("comments".to_owned(), json!(request.latest_worker_comments));
        rationale.insert(
            "needs_restructure_suggested_nodes".to_owned(),
            json!(request.latest_worker_needs_restructure_suggested_nodes),
        );
        // PV under-model (Slice 1): surface the prior worker's disproof / route
        // opinion / reasoning so the reviewer can FORWARD the under-model claim
        // to the auditor via NeedInput. Inserted ONLY when the disproof is
        // non-empty (the prior worker emitted `target_false_under_model`), so
        // every other reviewer payload — math mode included — is byte-identical.
        if !request.latest_worker_under_model_disproof.trim().is_empty() {
            rationale.insert(
                "under_model_disproof".to_owned(),
                json!(request.latest_worker_under_model_disproof),
            );
            rationale.insert(
                "under_model_route_opinion".to_owned(),
                json!(request.latest_worker_under_model_route_opinion),
            );
            rationale.insert(
                "under_model_reasoning".to_owned(),
                json!(request.latest_worker_under_model_reasoning),
            );
        }
        request_summary.insert(
            "latest_worker_rationale".to_owned(),
            Value::Object(rationale),
        );
    }
    request_summary.insert(
        "acceptance_logic_identity".to_owned(),
        json!(request.acceptance_logic_identity),
    );
    request_summary.insert(
        "deterministic_worker_rejection_reasons".to_owned(),
        json!(request.deterministic_worker_rejection_reasons),
    );
    // On-demand audit: surface a worker's advisory audit request so the
    // reviewer can decide to forward it (emit its own `audit_request`) or
    // let it lapse. `null` when the worker raised none.
    request_summary.insert(
        "pending_worker_audit_request".to_owned(),
        json!(request.pending_worker_audit_request),
    );
    request_summary.insert(
        "latest_review_rejection_reasons".to_owned(),
        json!(request.latest_review_rejection_reasons),
    );
    // Cross-cycle history: the reviewer's prompt previously only included
    // the immediately-previous worker (via `latest_worker_rationale`). The
    // append-only burst-history ledger at the path below carries one row
    // per WrapperResponse across the entire run — workers, reviewers, and
    // verifiers — so an agent grepping by active_node can see prior
    // reviewer decisions / worker attempts on the same node. Surface the
    // path here so the prompt fragment can point at it without hardcoding.
    request_summary.insert(
        "recent_burst_history_path".to_owned(),
        json!(".trellis/logs/burst-history.jsonl"),
    );
    if !request.phase.is_theorem_stating_like() {
        // Nodes present at the end of theorem-stating. When granting a
        // restructure mode to repair an active node's signature, check
        // whether the active node is in this set: if yes, the repair
        // needs `coarse_restructure`; if no, plain `restructure` is
        // sufficient. Empty for legacy runs that predate this field.
        request_summary.insert(
            "coarse_dag_nodes".to_owned(),
            json!(request.coarse_dag_nodes),
        );
    }
    if request.phase == Phase::ProofFormalization
        && !request.resettable_theorem_stating_nodes.is_empty()
    {
        request_summary.insert(
            "resettable_theorem_stating_nodes".to_owned(),
            json!(request.resettable_theorem_stating_nodes),
        );
    }
    if request.phase == Phase::ProofFormalization && !request.approved_target_nodes.is_empty() {
        request_summary.insert(
            "approved_target_nodes".to_owned(),
            json!(request.approved_target_nodes),
        );
    }
    if let Some(confirmation) = request.protected_semantic_change_confirmation.as_ref() {
        request_summary.insert(
            "protected_semantic_change_confirmation".to_owned(),
            json!(confirmation),
        );
    }
    if !request.protected_reapproval_nodes.is_empty() {
        request_summary.insert(
            "protected_reapproval_nodes".to_owned(),
            json!(request.protected_reapproval_nodes),
        );
        request_summary.insert(
            "protected_reapproval_status".to_owned(),
            json!(pv_monotonicity_gate_status(request)),
        );
    }

    // Blocker action fields are independent choices, not a complete
    // partition. Reviewers only name obligations they are acting on in this
    // transition; omitted blockers remain live and will resurface.
    // Option C (2026-06-04): override_blocker_ids retired; the reviewer's
    // blocker actions collapse to {task, reset, request_sound_verifier}.
    let allowed_reset_ids = blocker_choice_ids(&request.allowed_reset_blockers);
    let mut action_fields: Vec<&'static str> = vec!["task_blocker_ids"];
    if !allowed_reset_ids.is_empty() {
        action_fields.push("reset_blocker_ids");
    }
    action_fields.push("request_sound_verifier_node_ids");

    // Build the prompt_schema_example, omitting fields not in the
    // current action_fields list.
    let mut prompt_schema_example = serde_json::Map::new();
    // Mirror the snake_case fix for `next_mode`/`reset` (commit 9f88125):
    // the artifact validator's `parse_decision` does `to_ascii_lowercase()`
    // against snake_case constants, so PascalCase (`AdvancePhase`,
    // `NeedInput`) lowercases to `advancephase`/`needinput` and fails to
    // match `advance_phase`/`need_input`. No hard FAIL this run because
    // every chat that emitted `decision` happened to pick the single-token
    // `Continue`/`Done` forms; surface the underscored vocabulary in the
    // example so the latent footgun goes away.
    prompt_schema_example.insert(
        "decision".to_owned(),
        json!(request
            .allowed_decisions
            .iter()
            .map(review_decision_snake)
            .collect::<Vec<_>>()),
    );
    prompt_schema_example.insert(
        "reason".to_owned(),
        json!("brief rationale for the decision"),
    );
    prompt_schema_example.insert(
        "comments".to_owned(),
        json!("optional non-authoritative comments"),
    );
    prompt_schema_example.insert(
        "task_blocker_ids".to_owned(),
        json!(["subset of listed ids assigned to the next worker; omit blockers you are not assigning now"]),
    );
    if !allowed_reset_ids.is_empty() {
        prompt_schema_example.insert(
            "reset_blocker_ids".to_owned(),
            json!(["subset of allowed reset ids"]),
        );
    }
    prompt_schema_example.insert(
        "request_sound_verifier_node_ids".to_owned(),
        json!(["node id from blocker_actions.sound_verifier_requestable_nodes"]),
    );
    prompt_schema_example.insert(
        "next_active".to_owned(),
        match request.phase {
            Phase::TheoremStating | Phase::RevisionStating => json!("node id or empty string"),
            Phase::ProofFormalization => {
                json!("node id (required; empty string is rejected)")
            }
            Phase::Cleanup => {
                json!("empty string (cleanup dispatch is task-driven via cleanup_next_task; the worker's active node is resolved from the task's target_node)")
            }
            Phase::Complete => json!("node id or empty string"),
        },
    );
    // Proposal v32 audit-2 followup #3: surface `next_active_coarse` in
    // the schema example so reviewers don't have to grep source to learn
    // about it. The string `""` is the "preserve current anchor"
    // sentinel (legal everywhere); a non-empty node id is only legal in
    // ProofFormalization Continue (non-retry) and must be a member of
    // `next_active_coarse_contract.kernel_hinted_coarse_nodes`. See
    // `review_next_active_coarse_legal_for_response` in model.rs:2394.
    prompt_schema_example.insert(
        "next_active_coarse".to_owned(),
        if request.phase == Phase::ProofFormalization
            && matches!(request.retry_outcome_kind, RetryOutcomeKind::None)
            && !request.kernel_hinted_next_active_coarse_nodes.is_empty()
        {
            json!("empty string to preserve current anchor; or a node id from kernel_hinted_next_active_coarse_nodes to switch coarse anchor this cycle")
        } else {
            json!("")
        },
    );
    // Surface `next_mode` and `reset` as the snake_case strings the
    // artifact validator (`artifact_validation.rs`) accepts, not the
    // PascalCase produced by serde's default serialization. The
    // validator's check is `to_ascii_lowercase()` against snake_case
    // constants — so the PascalCase form (e.g. `CoarseRestructure`)
    // lowercases to `coarserestructure` and fails to match
    // `coarse_restructure`. Reviewers who copied from the schema example
    // hit this every time they tried a Restructure / CoarseRestructure
    // response and had to retry with the underscore form.
    prompt_schema_example.insert(
        "next_mode".to_owned(),
        json!(request
            .allowed_next_modes
            .iter()
            .map(task_mode_snake)
            .collect::<Vec<_>>()),
    );
    prompt_schema_example.insert(
        "reset".to_owned(),
        json!(request
            .allowed_resets
            .iter()
            .map(reset_choice_snake)
            .collect::<Vec<_>>()),
    );
    prompt_schema_example.insert(
        "reset_node".to_owned(),
        if request
            .allowed_resets
            .contains(&crate::model::ResetChoice::TheoremStatingNode)
        {
            json!("node id from resettable_theorem_stating_nodes when reset=theorem_stating_node; otherwise empty string")
        } else {
            json!("")
        },
    );
    prompt_schema_example.insert(
        "difficulty_updates".to_owned(),
        json!({"node_id from allowed_difficulty_update_nodes": "easy or hard"}),
    );
    prompt_schema_example.insert(
        "allow_new_obligations".to_owned(),
        if request.phase == Phase::ProofFormalization {
            json!("true/false; false requires every new helper to be mechanically closed")
        } else {
            json!(true)
        },
    );
    prompt_schema_example.insert(
        "must_close_active".to_owned(),
        if request.phase == Phase::ProofFormalization {
            json!("true/false; true requires the active node to be mechanically closed")
        } else {
            json!(false)
        },
    );
    // Trim 4: gate the four Continue-only example fields on whether
    // Continue is in the allowed decision set. When Continue is NOT
    // allowed (e.g. terminal-state Done / AdvancePhase / NeedInput
    // states), the worker is not going to run again, so these
    // routing-hint and human-input-clearing examples have no effect on
    // the outcome and should not appear in the schema example.
    let continue_allowed = request
        .allowed_decisions
        .contains(&crate::model::ReviewDecisionKind::Continue);
    let protected_scope_available =
        request.phase == Phase::ProofFormalization && !request.approved_target_nodes.is_empty();
    if continue_allowed {
        prompt_schema_example.insert(
            "clear_human_input".to_owned(),
            if request.human_input_outstanding {
                json!(true)
            } else {
                json!("omit unless clearing human input")
            },
        );
        prompt_schema_example.insert(
            "next_worker_context_mode".to_owned(),
            json!("resume or fresh"),
        );
        prompt_schema_example.insert(
            "paper_focus_ranges".to_owned(),
            json!([{"start_line": 1, "end_line": 5, "reason": "optional source-paper focus"}]),
        );
        prompt_schema_example.insert(
            "paper_grounding".to_owned(),
            json!({
                "consulted_cited_ranges": "true after directly reading every range in paper_focus_ranges; required for continue in friction reviews and whenever paper_focus_ranges is nonempty",
                "basis_summary": "short reviewer-authored note of what the cited paper text says and why it matters",
            }),
        );
        if request.stuck_math_audit.active {
            prompt_schema_example.insert(
                "stuck_math_audit".to_owned(),
                json!({
                    "notes": "brief reviewer note from the StuckMathAudit pass",
                    "reviewer_lean_product": format!(
                        "optional schema-light diagnostic product to forward to the next worker; max {} serialized JSON characters",
                        crate::model::STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS
                    ),
                }),
            );
        }
        if !request.pending_authoring_candidate.trim().is_empty() {
            prompt_schema_example.insert(
                "assumption_authoring_request".to_owned(),
                json!("optional true to ENACT the auditor-named under-model assumption candidate C (see assumption_authoring_contract): dispatches the PV assumption-authoring worker on the Assumptions node. Non-acting: do not set next_active / authorized_node_ids / blocker action lists / audit_request alongside. Do NOT route next_active to the Assumptions node yourself — that is always rejected."),
            );
        }
        if review_audit_dismissal_legal(request) {
            prompt_schema_example.insert(
                "dismiss_audit_plan".to_owned(),
                json!("optional true to dismiss the whole current audit_plan"),
            );
            prompt_schema_example.insert(
                "dismissed_tasks".to_owned(),
                json!([{"id": "audit task id", "reason": "why this task is stale or wrong"}]),
            );
        }
        prompt_schema_example.insert("work_style_hint".to_owned(), json!("none or restructure"));
        if request.sidecar_advertise_queue_fields {
            prompt_schema_example.insert(
                "sidecar_queue_add".to_owned(),
                json!(["node names to append to the grunt queue, in priority order; each must be sidecar-eligible now and not already queued"]),
            );
            prompt_schema_example.insert(
                "sidecar_queue_remove".to_owned(),
                json!(["queued node names to drop; a node you route via next_active leaves the queue on its own"]),
            );
        }
        if protected_scope_available {
            prompt_schema_example.insert(
                "protected_semantic_change_node_ids".to_owned(),
                json!("empty list unless exceptional; subset of protected_semantic_change_contract.allowed_nodes"),
            );
            prompt_schema_example.insert(
                "confirm_protected_semantic_change_scope".to_owned(),
                json!(request.protected_semantic_change_confirmation.is_some()),
            );
        }
        if request.phase.is_theorem_stating_like()
            && request.allowed_next_modes.contains(&TaskMode::Restructure)
        {
            // TheoremStating Restructure: the reviewer-authored cross-cone
            // restatement permission. The kernel enforces the envelope, so
            // the string names it.
            prompt_schema_example.insert(
                "authorized_node_ids".to_owned(),
                json!("for next_mode=restructure: the coordinated set of existing nodes the worker may restate (.tex statement + pre-`-- BODY` Lean signature + proof-so-far); required non-empty; every entry must be one of theorem_restructure_next_active_nodes (present nodes minus frozen statements: approved-target, challenge byte-pinned, and Preamble/Axioms are frozen); next_active must be one of these. Leave empty for global / targeted."),
            );
        }
        if request.phase == Phase::ProofFormalization {
            prompt_schema_example.insert(
                "authorized_node_ids".to_owned(),
                json!("narrow list of existing nodes the worker may edit; required (non-empty) for restructure / coarse_restructure; required empty for local mode; must be a subset of next_active+next_mode's scope envelope; next_active is a scope anchor, include it here only if the worker may edit that node"),
            );
            if request.global_repair_mode_enabled {
                prompt_schema_example.insert(
                    "global_repair_request".to_owned(),
                    json!("optional Step A: omit to skip, or {proposed_extension_node_ids: [..], reason: \"..\"} to request an audit-gated cone extension. Legal while a grant is pending: an accepted Step A supersedes that grant (the kernel drops it and surfaces it to the adjudicating audit). Non-acting: do not set authorized_node_ids / next_active / task_blocker_ids alongside."),
                );
                prompt_schema_example.insert(
                    "consume_global_repair_grant".to_owned(),
                    json!("optional Step C: set true to consume the pending audit grant; next_mode must be restructure or coarse_restructure; authorized_node_ids may include all of pending_global_repair_grant.approved_extension_nodes; next_active follows the normal legality rules (a granted node is additionally legal as next_active when base-legal). The grant stays pending across rejected worker bursts and stuck/needs_restructure retries; it clears on an accepted worker burst, on an accepted fresh Step A (which supersedes it), or when its TTL (default 3 cycles from the Step-A dispatch) lapses."),
                );
            }
            if request.pending_node_retirement.is_some() {
                prompt_schema_example.insert(
                    "dispatch_node_retirement".to_owned(),
                    json!("true to dispatch the pending node retirement (see node_retirement_contract) as this worker task"),
                );
                prompt_schema_example.insert(
                    "node_retirement_decline_reason".to_owned(),
                    json!("non-empty to decline the pending node retirement"),
                );
            }
        }
    }

    // Process memory (spec §5): the challenge channel. Outside the
    // `continue_allowed` block on purpose — a challenge is non-acting,
    // and `apply_review_response` records it on ANY accepted response,
    // independent of the routing decision. Advertised only when
    // `process-memory/` holds an active entry there is to challenge
    // (the `sidecar_advertise_queue_fields` precedent), so a run without
    // active memory renders byte-identically.
    if request.process_memory_active {
        prompt_schema_example.insert(
            "memory_challenges".to_owned(),
            json!([{"entry_id": "active process-memory entry id your evidence contradicts", "reason": "what contradicts it; the next audit adjudicates"}]),
        );
    }

    // A4: the difficulty_update_contract is a proof-formalization mechanism;
    // emit `null` during TheoremStating so the prompt does not dump 33 node
    // names with `easy/hard` slots that have no functional effect.
    let difficulty_update_contract = if request.phase.is_theorem_stating_like() {
        Value::Null
    } else {
        json!({
            "allowed_nodes": request.allowed_difficulty_update_nodes,
        })
    };

    let sound_obligations: Vec<Value> = request
        .blockers
        .iter()
        .filter_map(|blocker| match (&blocker.object, blocker.kind) {
            (BlockerObject::Node { node }, BlockerKind::Soundness) => Some(json!({
                "node": node,
                "blocker_id": crate::blocker_choice_id(blocker),
                "status": request.sound_assessment_statuses.get(node),
                "worker_repair_ready": request.sound_repair_ready_nodes.contains(node),
                "verifier_requestable": request.sound_verifier_requestable_nodes.contains(node),
            })),
            _ => None,
        })
        .collect();
    let mut blocker_actions = serde_json::Map::new();
    blocker_actions.insert("required".to_owned(), json!(false));
    blocker_actions.insert("action_fields".to_owned(), json!(action_fields));
    blocker_actions.insert(
        "meaning".to_owned(),
        json!("Each list is optional action for this transition. Omitted blockers remain live; there is no complete-partition requirement."),
    );
    blocker_actions.insert(
        "choices".to_owned(),
        json!(blocker_choices(&request.blockers)),
    );
    blocker_actions.insert("allowed_reset_ids".to_owned(), json!(allowed_reset_ids));
    // Option C (2026-06-04): `allowed_override_ids` retired; the
    // reviewer's blocker actions collapse to {task, reset,
    // request_sound_verifier}. See
    // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.
    blocker_actions.insert("sound_obligations".to_owned(), json!(sound_obligations));
    blocker_actions.insert(
        "sound_verifier_requestable_nodes".to_owned(),
        json!(request.sound_verifier_requestable_nodes),
    );
    blocker_actions.insert(
        "sound_repair_ready_nodes".to_owned(),
        json!(request.sound_repair_ready_nodes),
    );
    blocker_actions.insert(
        "reset_semantics".to_owned(),
        json!("clear_current_fail_to_unknown"),
    );
    let mut optional_fields = vec![
        "clear_human_input",
        "next_worker_context_mode",
        "paper_focus_ranges",
        "paper_grounding",
        "work_style_hint",
        "next_active_coarse",
    ];
    if request.stuck_math_audit.active {
        optional_fields.push("stuck_math_audit");
    }
    if review_audit_dismissal_legal(request) {
        optional_fields.push("dismiss_audit_plan");
        optional_fields.push("dismissed_tasks");
    }
    if continue_allowed && protected_scope_available {
        optional_fields.push("protected_semantic_change_node_ids");
        optional_fields.push("confirm_protected_semantic_change_scope");
    }
    // Audit-ordered node retirement: surface the dispatch/decline fields
    // only while an order is pending.
    if request.pending_node_retirement.is_some() {
        optional_fields.push("dispatch_node_retirement");
        optional_fields.push("node_retirement_decline_reason");
    }
    // global_repair_mode: surface Step A and Step C optional fields
    // only when the feature is enabled AND we're in ProofFormalization.
    if request.global_repair_mode_enabled
        && request.phase == Phase::ProofFormalization
        && continue_allowed
    {
        optional_fields.push("global_repair_request");
        optional_fields.push("consume_global_repair_grant");
    }
    // On-demand audit: surface `audit_request` as a baseline capability
    // whenever an on-demand audit can actually be dispatched right now
    // (phase admits it, no audit lane in flight, not on cooldown). Unlike
    // global_repair it is not gated on a feature flag.
    if request.audit_request_admissible && continue_allowed {
        optional_fields.push("audit_request");
    }
    // Sidecar grunt queue (Q6): advertised only when the run's
    // `sidecar` config block is enabled (runtime-resolved flag). The
    // fields stay LEGAL when unadvertised — a reviewer emitting them on
    // a sidecar-less run is legal-but-inert (queue fills, no daemon
    // exists, nothing applies).
    if request.sidecar_advertise_queue_fields {
        optional_fields.push("sidecar_queue_add");
        optional_fields.push("sidecar_queue_remove");
    }
    // Process memory (spec §5): the challenge channel. Advertised on the
    // same runtime-resolved terms as the sidecar queue fields — only when
    // `process-memory/` holds an active entry there is to challenge, so a
    // memory-less run's contract stays byte-identical. Unadvertised the
    // field remains LEGAL (the validator normalizes it, the engine records
    // it), so the gate is presentation-only and carries no soundness risk.
    if request.process_memory_active {
        optional_fields.push("memory_challenges");
    }
    // PV under-model (approach-audit route): surface the enact flag exactly
    // when an auditor-named candidate `C` is recorded and enactable (the
    // request projection blanks `pending_authoring_candidate` outside PV
    // theorem-stating-like non-retry Reviews; it does not clear live state).
    let assumption_authoring_enactable =
        !request.pending_authoring_candidate.trim().is_empty() && continue_allowed;
    if assumption_authoring_enactable {
        optional_fields.push("assumption_authoring_request");
    }
    // `authorized_node_ids` is required when the reviewer can hand
    // proof Continue+Restructure/CoarseRestructure work to a worker;
    // otherwise it's optional (and must be empty in non-cross-node
    // modes — see review/common/36_authorized_nodes.md).
    let proof_restructure_modes_allowed = request.phase == Phase::ProofFormalization
        && continue_allowed
        && (request.allowed_next_modes.contains(&TaskMode::Restructure)
            || request
                .allowed_next_modes
                .contains(&TaskMode::CoarseRestructure));
    // `request_sound_verifier_node_ids` is intentionally NOT in required_fields:
    // the validator (artifact_validation.rs `expect_string_list`) treats it
    // as optional with default `[]`, matching the semantic that not every
    // reviewer response asks for a Sound verifier dispatch. The other entries
    // here ARE strictly required (decision, reason, task_blocker_ids, etc.).
    // Option C (2026-06-04): `override_blocker_ids` removed from the
    // required field list; the reviewer no longer needs to emit it.
    // The raw payload field is still tolerated for back-compat (see
    // RawReviewPayload) and silently dropped during normalization.
    let mut required_fields: Vec<&'static str> = vec![
        "decision",
        "reason",
        "comments",
        "task_blocker_ids",
        "reset_blocker_ids",
        "next_active",
        "next_mode",
        "reset",
        "reset_node",
        "difficulty_updates",
        "allow_new_obligations",
        "must_close_active",
    ];
    if proof_restructure_modes_allowed {
        required_fields.push("authorized_node_ids");
    } else if request.phase == Phase::ProofFormalization && continue_allowed {
        // Local-only proof state: the field is still emitted in the
        // schema example for completeness, but is required to be
        // empty.
        optional_fields.push("authorized_node_ids");
    }
    let protected_semantic_change_contract = if protected_scope_available {
        json!({
            "allowed_nodes": request.approved_target_nodes,
            "default": [],
            "requires": {
                "decision": "continue",
                "next_mode": "coarse_restructure",
                "next_active": "non_empty",
                "reset": "none",
            },
            "confirmation_required": request.protected_semantic_change_confirmation.is_some(),
            "pending_confirmation": request.protected_semantic_change_confirmation,
            "warning": if request.protected_semantic_change_confirmation.is_some() {
                json!("Confirming this scope allows a worker to reopen protected semantic meaning; any actual reopen must pass verifier lanes and then triggers human reapproval.")
            } else {
                json!("Exceptional only. Leave empty unless preserving the protected semantic node is genuinely impossible.")
            },
        })
    } else {
        Value::Null
    };

    // Cleanup-v2 Step 17: when reviewing in Phase::Cleanup, surface the
    // task list + per-status counts + allowed-next-task indices +
    // re-audit legality so the prompt can render the task table and
    // the reviewer's allowed inputs are explicit. On other phases,
    // these fields are omitted (the surface only matters in cleanup).
    let cleanup_contract = if request.phase == Phase::Cleanup
        && request.kind == crate::model::RequestKind::Review
    {
        let tasks_view: Vec<Value> = request
            .cleanup_audit_tasks_view
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({
                    "task_index": i,
                    "target_node": t.target_node,
                    "rationale": t.rationale,
                    "confidence": t.confidence,
                    "kind": t.kind,
                    "status": t.status,
                    "audit_origin_round": t.audit_origin_round,
                    "swept_parents": t.swept_parents,
                    // ExtractShared only (kernel-attached at proposal
                    // time): block length for the declared parent set.
                    // recoverable = region_block_lines × (n_parents − 1).
                    "region_block_lines": t.region_block_lines,
                })
            })
            .collect();
        let pending_indices: Vec<u32> = request
            .cleanup_audit_tasks_view
            .iter()
            .enumerate()
            .filter(|(_, t)| matches!(t.status, crate::model::CleanupTaskStatus::Pending))
            .map(|(i, _)| i as u32)
            .collect();
        let pending_count = pending_indices.len();
        let completed_count = request
            .cleanup_audit_tasks_view
            .iter()
            .filter(|t| matches!(t.status, crate::model::CleanupTaskStatus::Completed))
            .count();
        let failed_count = request
            .cleanup_audit_tasks_view
            .iter()
            .filter(|t| matches!(t.status, crate::model::CleanupTaskStatus::Failed { .. }))
            .count();
        let dismissed_count = request
            .cleanup_audit_tasks_view
            .iter()
            .filter(|t| matches!(t.status, crate::model::CleanupTaskStatus::Dismissed { .. }))
            .count();
        let request_reaudit_legal =
            request.cleanup_audit_round_view < crate::model::CLEANUP_AUDIT_MAX_ROUNDS;
        json!({
            "tasks": tasks_view,
            "pending_count": pending_count,
            "completed_count": completed_count,
            "failed_count": failed_count,
            "dismissed_count": dismissed_count,
            "pending_indices": pending_indices,
            "cleanup_audit_round": request.cleanup_audit_round_view,
            "max_rounds": crate::model::CLEANUP_AUDIT_MAX_ROUNDS,
            "request_reaudit_legal": request_reaudit_legal,
            "protected_statement_node_set": request.cleanup_protected_statement_node_set_view,
            "dispatch_semantics": {
                "cleanup_dismiss_tasks": "Array<{task_index: integer, reason: non-empty string}>; bulk-dismiss any subset of Pending tasks",
                "cleanup_next_task": "Optional<task_index>; dispatch exactly one Pending task to a worker burst this cycle. Mutually exclusive with cleanup_batch_tasks.",
                "cleanup_batch_tasks": format!(
                    "Optional<[task_index]>; dispatch multiple Pending tasks of ONE kind (LintFix or ExtractHelper) to a single worker burst, up to {} distinct present unprotected nodes (the kernel derives the exact multi-node scope). Tasks must share a kind — a burst has exactly one acceptance envelope, so a mixed batch has no defined acceptance meaning. Substitution, DeadCodeElim, and ExtractShared can never be batched — dispatch those one per burst via cleanup_next_task. Mutually exclusive with cleanup_next_task. Whole-burst atomic rollback: any one task's rejection rolls back the entire batch, so batch only tasks you are confident in; a rejected batch leaves its tasks Pending for serial re-dispatch.",
                    crate::model::CLEANUP_BATCH_MAX
                ),
                "cleanup_batch_max": crate::model::CLEANUP_BATCH_MAX,
                "cleanup_request_reaudit": "Only legal on Done; only effective when cleanup_audit_round < max_rounds",
                "cleanup_repair_node": "Optional<NodeId>; dispatch one correspondence repair candidate; mutually exclusive with task dispatch",
                "authorized_nodes": "Worker edit scope. For Substitution, include all importers; the target is implicit and deletable. LintFix, DeadCodeElim, and ExtractHelper are single-node (or kernel-derived batch scope for a batchable kind). ExtractShared derives its full unbounded parent scope from target_node plus kind.co_parents; reviewer authorization cannot widen it.",
            }
        })
    } else {
        Value::Null
    };
    let stuck_math_audit_contract = if request.stuck_math_audit.active {
        json!({
            "active": true,
            "response_field": "stuck_math_audit",
            "required_when": "decision=continue and reset=none",
            "shape": {
                "notes": "string; non-empty notes are sufficient when no product is useful",
                "reviewer_lean_product": format!(
                    "optional schema-light JSON value forwarded to the next worker when present; must serialize to at most {} JSON characters",
                    crate::model::STUCK_MATH_REVIEWER_LEAN_PRODUCT_MAX_JSON_CHARS
                ),
            },
            "current_state": request.stuck_math_audit.clone(),
        })
    } else {
        Value::Null
    };

    json!({
        "prompt_fragments": route_fragments(review_prompt_fragments(request, target), target),
        "request_summary": Value::Object(request_summary),
        "cleanup_contract": cleanup_contract,
        "stuck_math_audit_contract": stuck_math_audit_contract,
        // Option A: visibility ⇔ dismissability. The live audit plan
        // and its contract block are surfaced to the reviewer iff
        // dismissal is legal. When the plan is non-dismissable
        // (latch off, wrong phase, etc.), the live surfaces are
        // suppressed and `previous_audit_plan_snapshot` (below)
        // carries a clearly-tagged historical reference instead.
        "audit_plan": if review_audit_dismissal_legal(request) {
            json!(request.audit_plan.clone())
        } else {
            Value::Null
        },
        "audit_plan_contract": if review_audit_dismissal_legal(request) {
            let mut ctx = serde_json::Map::new();
            ctx.insert("visible".to_owned(), json!(true));
            ctx.insert("dismissal_legal".to_owned(), json!(true));
            ctx.insert(
                "dismiss_audit_plan_field".to_owned(),
                json!("dismiss_audit_plan"),
            );
            ctx.insert(
                "dismissed_tasks_field".to_owned(),
                json!("dismissed_tasks"),
            );
            ctx.insert(
                "dismissed_tasks_shape".to_owned(),
                json!([{"id": "task id from audit_plan.tasks", "reason": "non-empty reason"}]),
            );
            ctx.insert(
                "semantics".to_owned(),
                json!("Audit tasks are suggestions, not blocker authority. Stale tasks may only be dismissed explicitly while StuckMathAudit is active (proof_formalization / theorem_stating, or a NeedInputAuditor plan); otherwise route useful tasks through ordinary reviewer decisions."),
            );
            Value::Object(ctx)
        } else {
            Value::Null
        },
        // Option A snapshot surface: a clearly-tagged historical
        // reference, present iff the live plan is not surfaced and
        // some snapshot (live `audit_plan` or `superseded_audit_plan`)
        // exists. Distinct from `audit_plan` so the reviewer cannot
        // confuse a retired plan with an actionable one. The
        // `29c_last_audit_plan.md` prompt fragment explains the
        // advisory-only semantics.
        "previous_audit_plan_snapshot": request.previous_audit_plan_snapshot.clone(),
        "artifact_contract": {
            "result_type": "review_result_v1",
            "required_fields": required_fields,
            "optional_fields": optional_fields,
            "prompt_schema_example": Value::Object(prompt_schema_example),
        },
        "verifier_evidence": request.review_verifier_evidence,
        "blocker_actions": Value::Object(blocker_actions.clone()),
        "blocker_partition": Value::Object(blocker_actions),
        // Phase 2 of the bridge-to-kernel migration (2026-06-04): kernel-
        // rendered Markdown body + structured sidecar payload for the
        // reviewer-facing blocker-choices block. The bridge consumes this
        // via `_resolve_review_blocker_choices_block` and falls back to the
        // legacy in-bridge `_format_blocker_choices_summary` if the field is
        // absent (old-kernel compat). Direct parallel to
        // `worker_contract.blocker_status` (worker-side migration).
        "blocker_choices_block": review_blocker_choices_block(request),
        "need_input_contract": {
            "meaning": "escalate_to_human_before_blocker_adjudication",
            "blocker_partition_required": false,
            "task_blocker_ids": [],
            "reset_blocker_ids": [],
            "request_sound_verifier_node_ids": [],
            "next_active": "",
            "next_mode": request.mode,
            "next_worker_context_mode": "resume",
            "paper_focus_ranges": [],
            "work_style_hint": "none",
            "allow_new_obligations": true,
            "must_close_active": false,
        },
        // PV under-model (approach-audit route): present iff an auditor-named
        // candidate `C` is enactable this turn. The reviewer's ONLY legal move
        // toward the Assumptions node is this flag — `C` is auditor-owned and
        // the staged assumption still passes the assumptions-lane audit and
        // human ratification before entering the trusted base.
        "assumption_authoring_contract": if !request.pending_authoring_candidate.trim().is_empty() {
            json!({
                "candidate_invariant": request.pending_authoring_candidate,
                "field": "assumption_authoring_request",
                "requires": {
                    "decision": "continue",
                    "reset": "none",
                    "all_action_fields": "empty (non-acting, like audit_request)",
                },
                "semantics": "Set true to dispatch the PV assumption-authoring worker: it stages candidate_invariant as an under-model axiom in Tablet/Assumptions.{lean,tex}. You cannot restate or alter C. Routing next_active to the Assumptions node is never legal in any mode.",
            })
        } else {
            Value::Null
        },
        // Audit-ordered node retirement: present iff an order is pending.
        // The reviewer dispatches it as the next worker task
        // (`dispatch_node_retirement`) or declines with a recorded reason
        // (`node_retirement_decline_reason`).
        "node_retirement_contract": if let Some(pending) = request.pending_node_retirement.as_ref() {
            json!({
                "nodes": pending.nodes,
                "reason": pending.reason,
                "requested_at_cycle": pending.requested_at_cycle,
                "dispatch_field": "dispatch_node_retirement",
                "decline_field": "node_retirement_decline_reason",
                "dispatch_requires": {
                    "decision": "continue",
                    "reset": "none",
                    "next_mode": "restructure or coarse_restructure",
                    "allow_new_obligations": false,
                    "must_close_active": false,
                    "authorized_node_ids": "must include every still-present listed node (plus any surviving consumers the worker may repair)",
                },
                "semantics": "Dispatch as the next worker task: the worker deletes exactly the listed nodes and repairs surviving consumers. Acceptance verifies every listed node is absent post-burst.",
            })
        } else {
            Value::Null
        },
        "next_active_contract": {
            "kernel_hinted_nodes": request.kernel_hinted_next_active_nodes,
            "targeted_allowed_nodes": request.targeted_next_active_nodes,
            "theorem_restructure_allowed_nodes": request.theorem_restructure_next_active_nodes,
            "allow_targeted_without_next_active": request.allow_targeted_without_next_active,
            "proof_restructure_semantics": "For proof_formalization Continue (including next_mode=restructure/coarse_restructure), next_active must be in kernel_hinted_nodes; that set is already filtered to the active coarse-anchor cone (widened in coarse_repair_mode). To anchor outside the current cone, set next_active_coarse in the same response.",
            "theorem_stating_semantics": "next_mode=Global: next_active is optional; when provided it must be in kernel_hinted_nodes. next_mode=Targeted requires next_active in targeted_allowed_nodes; next_mode=Restructure requires next_active in theorem_restructure_allowed_nodes (present nodes minus frozen statements) and authorized_node_ids a non-empty subset of that same set with next_active among them.",
        },
        // A4: emit `null` during TheoremStating — the difficulty-update
        // mechanism is a proof-formalization knob (no easy/hard distinction
        // applies to nodes during theorem-stating).
        "difficulty_update_contract": difficulty_update_contract,
        "proof_obligation_scope_contract": {
            "applies_when": "phase=proof_formalization and decision=continue",
            "default_outside_applies_when": {
                "allow_new_obligations": true,
                "must_close_active": false,
            },
            "allow_new_obligations": {
                "false": "new helper nodes must be mechanically closed with no sorry, in addition to all normal scope and verifier requirements",
                "true": "new helper nodes may remain open with sorry/NL proof when otherwise legal"
            },
            "must_close_active": {
                "false": "the active node may remain open if the current scope otherwise accepts the burst",
                "true": "the active node must be mechanically closed with no sorry for the burst to be valid"
            },
            "difficulty_note": "easy/hard remains an advisory difficulty hint only; it does not change scope or closure gates"
        },
        "clear_human_input_contract": {
            "allowed_when_outstanding": request.human_input_outstanding,
            "omit_when_not_allowed": true,
        },
        "comments_contract": {
            "field": "comments",
            "semantics": "non_authoritative_guidance_forwarded_to_future_workers",
            "empty_string_means_no_comments": true,
        },
        "routing_hints_contract": {
            "next_worker_context_mode_values": ["resume", "fresh"],
            "paper_focus_ranges_shape": {"start_line": ">= 1", "end_line": ">= start_line", "reason": "optional short reason"},
            "work_style_hint_values": ["none", "restructure"],
            "continue_only": true,
            "advisory_only": true,
            "semantics": "non_authoritative_hints_forwarded_to_future_workers_without_expanding_kernel_authority",
        },
        "paper_grounding_contract": {
            "required_for_continue_reset_none_in_friction": request.review_requires_paper_grounding(),
            "required_when_paper_focus_ranges_nonempty": true,
            "friction_definition": "any blockers present, or retry_outcome_kind in {stuck, needs_restructure}",
            "attestation_field": "paper_grounding.consulted_cited_ranges",
            "attestation_semantics": "true iff the reviewer directly consulted the original paper text for every range in paper_focus_ranges before submitting this response",
            "basis_summary_field": "paper_grounding.basis_summary",
            "basis_summary_required_when_attesting": true,
            "cited_ranges_field": "paper_focus_ranges",
            "non_continue_must_be_default": true,
            "non_friction_continue_without_ranges_must_be_default": true,
        },
        "protected_semantic_change_contract": protected_semantic_change_contract,
        "reset_contract": {
            "allowed_resets": request.allowed_resets,
            "last_commit_semantics": "discard_unaccepted_live_changes_and_resume_from_last_accepted_checkpoint",
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-reviewer-result",
            "{{raw_output_path}}",
        ], &[
            "python3",
            "{{check_script_path}}",
            "trellis-reviewer-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ]),
    })
}

fn no_audit_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "scenario": "audit_dormant",
            "audit_round": 0,
            "audit_burst_index": 0,
            "max_bursts_per_round": 0,
        },
        "artifact_contract": {},
    })
}

fn no_stuck_math_audit_contract_payload() -> Value {
    json!({
        "prompt_fragments": [],
        "request_summary": {
            "phase": "",
            "scenario": "stuck_math_audit_dormant",
        },
        "artifact_contract": {},
    })
}

/// Attach the bridge-facing trimmed `prompt_facing_view` (drop
/// `prompt_fragments` + `artifact_prompt_view`, then null Option fields).
/// Shared by the GapResearch planner / critic payloads and mirrors the tail
/// of `stuck_math_audit_contract_payload`.
fn with_prompt_facing_view(mut contract: Value) -> Value {
    let mut view = contract.clone();
    if let Some(view_map) = view.as_object_mut() {
        view_map.remove("prompt_fragments");
        view_map.remove("artifact_prompt_view");
    }
    if let Some(map) = contract.as_object_mut() {
        map.insert("prompt_facing_view".to_string(), drop_null_keys(view));
    }
    contract
}

/// Revision Planner contract (`revision_plan.md` §9). Selected when the
/// StuckMathAudit lane carries a `revision_planning` context — the first audit
/// a revision run dispatches. The planner reads both papers + the current
/// tablet, classifies each changed target (add / strengthen / removed),
/// proposes the minimal update route, and emits worker tasks + a structured
/// revision action list.
///
/// The contract carries the structured `revision_planning` context (the
/// read-only planner packet: paper paths, per-target deltas, coverage,
/// protected closure, frozen / editable split), the planner role + output
/// fragments, and the `revision_actions` response schema. The accepted
/// response is a normal `StuckMathAuditResponse` plus `revision_actions`;
/// step 9 routes that into `RevisionContext` / `node_dispositions`.
fn revision_planner_contract_payload(
    request: &WrapperRequest,
    context: &crate::model::RevisionPlanningContext,
) -> Value {
    let prompt_schema_example = json!({
        "report": "revision plan: the mathematical delta, per-target classification (add/strengthen/removed), and the minimal update route",
        "tasks": [{
            "id": "stable-task-id",
            "title": "short imperative title",
            "body": "worker-facing task referencing the revision route"
        }],
        "revision_actions": {
            "targets": [{
                "target": "thm:main",
                "classification": "strengthen",
                "covering_nodes": ["MainTheorem"]
            }],
            "nodes": [{
                "node": "MainTheorem",
                "action": "restate",
                "reason": "new paper strengthens the exponent"
            }]
        }
    });
    let (revision_planner_role, revision_source_of_truth) = if request.is_pv {
        (
            "pv/stuck_audit/01_revision_planner_role.md",
            pv_stuck_audit_source_of_truth(request),
        )
    } else {
        // The role fragment is keyed on the revision kind: a genuine paper
        // revision reads two paper versions ("read both papers"), while a
        // target addition (ADD_TARGETS.md) extends a completed tablet from
        // the same, unchanged paper — the two-paper framing would read as a
        // misconfiguration to the planner.
        let role = match context.revision_kind {
            crate::model::RevisionKind::PaperRevision => {
                "stuck_math_audit/common/01_revision_planner_role.md"
            }
            crate::model::RevisionKind::TargetAddition => {
                "stuck_math_audit/common/01b_target_addition_planner_role.md"
            }
        };
        (role, "stuck_math_audit/common/02_reference_paper.md")
    };
    let mut prompt_fragments = vec![
        revision_planner_role,
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        revision_source_of_truth,
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "stuck_math_audit/common/03c_process_rules.md",
        "stuck_math_audit/common/02_request_context.md",
        stuck_audit_scratchpad_fragment(request),
        "stuck_math_audit/common/05_revision_plan_output_contract.md",
        "shared/90_artifact_delivery.md",
        structured_request_pointer_fragment(request),
    ];
    if reference_papers_fragments_active(request) {
        prompt_fragments.insert(4, "stuck_math_audit/common/02c_reference_papers.md");
    }
    let contract = json!({
        "prompt_fragments": prompt_fragments,
        "burst_role": "revision_planner",
        "request_summary": {
            "phase": request.phase,
            "scenario": "revision_planning",
            "cycle": request.cycle,
            "request_id": request.id,
            "active_node": request.active_node,
            "mode": request.mode,
            "blockers": request.blockers,
        },
        // The read-only planner packet (revision_plan.md §9). Carried verbatim
        // so the planner sees both paper paths, the projected paper diff, the
        // per-target coverage + protected closure, and the deterministic
        // frozen / editable node split computed at import.
        "revision_planning": context,
        "artifact_contract": {
            "result_type": "stuck_math_audit_result_v1",
            "report_min_chars": crate::model::AUDIT_REPORT_TEXT_MIN_CHARS,
            "report_max_chars": crate::model::AUDIT_REPORT_TEXT_MAX_CHARS,
            "prompt_schema_example": prompt_schema_example,
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-stuck-math-audit-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ], &[]),
    });
    with_prompt_facing_view(contract)
}

/// Fresh-run Initial Planner contract. Selected when the StuckMathAudit lane
/// carries an `initial_planning` context — the first burst of every fresh
/// run, dispatched before any worker. The planner reads the run's source of
/// truth (manuscript / goal prose / pinned challenge specs) + the configured
/// targets and emits an initial plan as a plain audit output (report +
/// tasks + probe_paths): foundational definitions and lowest-layer lemmas to
/// state first, DAG shape, decomposition strategy, target order. No
/// `revision_actions` and no new wire fields — the response is an ordinary
/// `StuckMathAuditResponse`, so the artifact validator is untouched. The
/// plan is advisory (Option W): workers keep their first-request
/// DAG-decomposition authority.
fn initial_planner_contract_payload(
    request: &WrapperRequest,
    context: &crate::model::InitialPlanningContext,
) -> Value {
    // Coverage re-planning variant: the SAME lane and contract shape with the
    // role fragment + report schema example swapped on the carrier's
    // `coverage_replanning` bool. `burst_role` and `request_summary.scenario`
    // stay identical — no downstream consumer needs the distinction.
    let coverage = context.coverage_replanning;
    let prompt_schema_example = if coverage {
        json!({
            "report": "coverage re-plan: progress against the current plan, the statements still needed to reach each uncovered target and where they attach to the existing DAG, the remaining target order, and the still-relevant open tasks carried forward",
            "tasks": [{
                "id": "stable-task-id",
                "title": "short imperative title",
                "body": "worker-facing task referencing the plan"
            }],
            "probe_paths": []
        })
    } else {
        json!({
            "report": "initial plan: what to read, the foundational definitions and lowest-layer lemmas to state first, the intended DAG shape and decomposition strategy, and the target order",
            "tasks": [{
                "id": "stable-task-id",
                "title": "short imperative title",
                "body": "worker-facing task referencing the plan"
            }],
            "probe_paths": []
        })
    };
    let mut prompt_schema_example = prompt_schema_example;
    // Decide polarity-flip authority on the planner
    // lane, mirroring the ordinary stuck-audit gating: a planner that
    // establishes a target's falsity files the structured carriers instead
    // of demoting the finding to plan prose. Not offered mid global
    // repair; no-op for non-Decide runs, so those contracts stay
    // byte-identical.
    // Prose decide pairs join once their statement has bound
    // (`has_actionable_decide_target`).
    let decide_flip_available = request.is_pv
        && request.pending_global_repair_request.is_none()
        && has_actionable_decide_target(request);
    if decide_flip_available {
        let obj = prompt_schema_example
            .as_object_mut()
            .expect("prompt_schema_example is a JSON object");
        obj.insert(
            "set_live_polarity".to_owned(),
            json!("\"\", \"prove\", or \"disprove\" — flip a Decide pair's live polarity (empty to leave unchanged)"),
        );
        obj.insert(
            "set_live_polarity_target".to_owned(),
            json!("optional: the Decide PRIMARY challenge target id to flip; required when the pair is not the active/held node; only meaningful with set_live_polarity"),
        );
    }
    if request.is_pv && request.trust_base_required_v1 && has_actionable_decide_target(request) {
        let obj = prompt_schema_example
            .as_object_mut()
            .expect("prompt_schema_example is a JSON object");
        obj.insert(
            "conditional_theorem_proposal".to_owned(),
            json!({
                "target_id": "Decide primary target id",
                "condition_lean": "complete Lean Prop under the registered target binders",
                "condition_informal": "concise condition",
                "rationale": "why the unrestricted result is not established",
                "trigger": "model_counterexample_mismatch | assumptions_model_gap | unconditional_not_established",
                "evidence": {
                    "disproof_sha256": null,
                    "artifact_sha256": null,
                    "assumption_ids": []
                },
                "existing_under_model_assumption_id": null,
                "concrete_counterexample_arguments": null
            }),
        );
        obj.insert("conditional_theorem_withdrawals".to_owned(), json!([]));
    }
    // Role fragment per source kind: math runs read the manuscript and PV runs
    // read the prose goal file plus the extracted crate model.
    let (initial_planner_role, initial_source_of_truth) = if request.is_pv {
        (
            if coverage {
                "pv/stuck_audit/01c_coverage_planner_role.md"
            } else {
                "pv/stuck_audit/01c_initial_planner_role.md"
            },
            pv_stuck_audit_source_of_truth(request),
        )
    } else {
        (
            if coverage {
                "stuck_math_audit/common/01c_coverage_planner_role.md"
            } else {
                "stuck_math_audit/common/01c_initial_planner_role.md"
            },
            "stuck_math_audit/common/02_reference_paper.md",
        )
    };
    let mut prompt_fragments = vec![
        initial_planner_role,
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        initial_source_of_truth,
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "stuck_math_audit/common/03c_process_rules.md",
        "stuck_math_audit/common/02_request_context.md",
        // Process memory: the planner writes `memory_operations` (refuted
        // routes, settled constraints), so it sees the same settled-entry /
        // pending-challenge view the structural audit sees. Renders empty
        // (and drops out of the prompt) on runs without process memory.
        "stuck_math_audit/common/03b_process_memory.md",
        stuck_audit_scratchpad_fragment(request),
        stuck_audit_output_contract_fragment(request),
        "shared/90_artifact_delivery.md",
        structured_request_pointer_fragment(request),
    ];
    if decide_flip_available {
        let position = prompt_fragments.len() - 3;
        prompt_fragments.insert(position, "pv/stuck_audit/06_decide_polarity_flip.md");
    }
    if reference_papers_fragments_active(request) {
        prompt_fragments.insert(4, "stuck_math_audit/common/02c_reference_papers.md");
    }
    let contract = json!({
        "prompt_fragments": prompt_fragments,
        "burst_role": "initial_planner",
        "request_summary": {
            "phase": request.phase,
            "scenario": "initial_planning",
            "cycle": request.cycle,
            "request_id": request.id,
            "active_node": request.active_node,
            "mode": request.mode,
            "blockers": request.blockers,
        },
        // The read-only planner packet: the run's source of truth (manuscript
        // path / goal path / verbatim pinned statements) + the configured
        // target ids the plan must route.
        "initial_planning": context,
        "artifact_contract": {
            "result_type": "stuck_math_audit_result_v1",
            "report_min_chars": crate::model::AUDIT_REPORT_TEXT_MIN_CHARS,
            "report_max_chars": crate::model::AUDIT_REPORT_TEXT_MAX_CHARS,
            "prompt_schema_example": prompt_schema_example,
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-stuck-math-audit-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ], &[]),
    });
    with_prompt_facing_view(contract)
}

/// GapResearch Planner contract. Selects the Planner role + output
/// fragments and asks for a brief `report` (rationale) + a natural-language
/// proof-route `.tex` (`route_tex`). On a re-plan it surfaces the
/// accumulated critic feedback so the Planner re-plans *against* the
/// critique. Synthesis posture; scratch-Lean only, never `Tablet/`. When
/// the gap genuinely has no autonomous route the Planner sets
/// `route_needs_human`: a legacy run opens its human gate, while RequiredV1
/// interprets the same artifact field as a loud terminal refusal.
fn gap_research_planner_contract_payload(
    request: &WrapperRequest,
    context: &crate::model::GapResearchContext,
) -> Value {
    // PV substitutive: the schema-example rationale references "paper
    // machinery" — re-point it to the crate / pinned model (no paper in PV).
    let prompt_schema_example = if request.is_pv {
        json!({
            "report": "brief rationale: how the route plugs the gap, what crate facts and pinned-model machinery it leans on",
            "route_tex": "the natural-language proof-route .tex body — prose describing how to address the gap, citing current Tablet nodes/obligations where motivating",
            "route_needs_human": false
        })
    } else {
        json!({
            "report": "brief rationale: how the route plugs the gap, what paper machinery it leans on",
            "route_tex": "the natural-language proof-route .tex body — prose describing how to address the gap, citing current Tablet nodes/obligations where motivating",
            "route_needs_human": false
        })
    };
    let required_v1_gap = request.is_pv && request.trust_base_required_v1;
    let (gap_research_role, gap_research_source_of_truth) = if request.is_pv {
        (
            if required_v1_gap {
                "pv/stuck_audit/01_gap_research_role_trust_v1.md"
            } else {
                "pv/stuck_audit/01_gap_research_role.md"
            },
            pv_stuck_audit_source_of_truth(request),
        )
    } else {
        (
            "stuck_math_audit/common/01_gap_research_role.md",
            "stuck_math_audit/common/02_reference_paper.md",
        )
    };
    let mut prompt_fragments = vec![
        gap_research_role,
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        gap_research_source_of_truth,
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "stuck_math_audit/common/03c_process_rules.md",
        "stuck_math_audit/common/02_request_context.md",
        "stuck_math_audit/common/02b_trigger_reason.md",
        stuck_audit_history_access_fragment(request),
        stuck_audit_scratchpad_fragment(request),
        // PV substitutive: the output-contract fragment references "paper
        // machinery" / "the paper and the tablet" and the deviation path; the
        // PV variant re-points to the crate / GOAL.md / pinned model and drops
        // the deviation sentence (no Deviation lane in PV).
        if required_v1_gap {
            "pv/stuck_audit/05_gap_plan_output_contract_trust_v1.md"
        } else if request.is_pv {
            "pv/stuck_audit/05_gap_plan_output_contract.md"
        } else {
            "stuck_math_audit/common/05_gap_plan_output_contract.md"
        },
        "shared/90_artifact_delivery.md",
        structured_request_pointer_fragment(request),
    ];
    if reference_papers_fragments_active(request) {
        prompt_fragments.insert(4, "stuck_math_audit/common/02c_reference_papers.md");
    }
    let mut contract = json!({
        "prompt_fragments": prompt_fragments,
        "burst_role": "gap_research",
        "request_summary": {
            "phase": request.phase,
            "scenario": "gap_research",
            "cycle": request.cycle,
            "request_id": request.id,
            "active_node": request.active_node,
            "mode": request.mode,
            "blockers": request.blockers,
        },
        "gap_brief": context.gap_brief,
        // On a re-plan: the running record of prior rejected routes + the
        // critic's reason for each. Drives "re-plan against the critique".
        "accumulated_critic_feedback": context.accumulated_critic_feedback,
        "latest_critic_feedback": request.latest_global_repair_audit_decline_reason,
        // PV substitutive: no paper and no Deviation lane. The PV guidance keeps
        // the route grounded in the pinned model and escalates when the goal is
        // genuinely unprovable against it.
        "deviation_guidance": if required_v1_gap {
            "The pinned ExtractionModel definitions and target statements are inviolate. Use route_tex only for an autonomous proof/repair route that stays within them. A persisted typed refutation carrier may select the stricter structured adjudication lane, and an unbound authored target must complete statement binding before that adjudication. Neither the carrier nor reviewer/Sound prose is authority. If no legal autonomous route exists, set route_needs_human: in RequiredV1 that is a terminal refusal which halts loudly and never opens a HumanGate."
        } else if request.is_pv {
            "The pinned ExtractionModel def nodes are inviolate ground truth; a route may not rest on changing them. The route must stay grounded in what the pinned model supports; when the goal property is genuinely false or unprovable against the pinned model, set route_needs_human."
        } else {
            "If the route involves a deviation from the paper, describe it in the route prose; the worker authors it through the normal deviation_requests path and the Deviation + Substantiveness verifier lanes check it. Deviations admit only minor changes (DEVIATIONS.md); when the only faithful fix is a non-minor statement change or genuinely-missing mathematics, set route_needs_human."
        },
        "artifact_contract": {
            "result_type": "stuck_math_audit_result_v1",
            "report_min_chars": crate::model::AUDIT_REPORT_TEXT_MIN_CHARS,
            "report_max_chars": crate::model::AUDIT_REPORT_TEXT_MAX_CHARS,
            "prompt_schema_example": prompt_schema_example,
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-stuck-math-audit-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ], &[]),
    });
    if required_v1_gap {
        contract
            .as_object_mut()
            .expect("gap contract is an object")
            .insert(
                "required_v1_resolution_contract".to_string(),
                json!({
                    "human_gate_available": false,
                    "legal_routes": [
                        "structured_adjudication",
                        "statement_binding",
                        "terminal_refusal"
                    ],
                    "structured_adjudication": "A validated typed {target, refutation, evidence_refs} carrier may select the stricter target-bound adjudication/station lane. It is routing evidence only and never a station record, give-up, or polarity decision.",
                    "statement_binding": "When the carrier target is worker-authored and still unbound, preserve the carrier while correspondence/substantiveness acceptance binds the statement; adjudication follows only after binding.",
                    "route_needs_human_semantics": "terminal_refusal",
                    "terminal_refusal": "Set route_needs_human only when neither structured adjudication/binding nor an autonomous proof/repair route is legal. The kernel writes the halt sentinel; no HumanGate exists in RequiredV1."
                }),
            );
    }
    with_prompt_facing_view(contract)
}

/// GapResearch Critic contract. Selects the plan-critic role + output
/// fragments and asks for an `accept` / `reject` decision over the route.
/// **Context isolation (load-bearing):** the payload carries ONLY the route
/// `.tex` + the `gap_brief` (+ the paper, which the critic reads itself) —
/// it deliberately does NOT include the planner's report, scratch, session,
/// accumulated feedback, or any narrative of how the route was produced. On
/// ACCEPT the critic writes an audit `report` + `tasks` (the existing
/// AuditPlan shape) directing implementation; on REJECT it returns concrete
/// `gap_feedback`. The critic evaluates the route independently and
/// adversarially from scratch.
fn gap_plan_critic_contract_payload(
    request: &WrapperRequest,
    context: &crate::model::GapPlanCritiqueContext,
) -> Value {
    let prompt_schema_example = json!({
        "report": "on accept: the audit report directing implementation (omit on reject)",
        "tasks": [{
            "id": "stable-task-id",
            "title": "short imperative title",
            "body": "what the worker must do, referencing the accepted route"
        }],
        "gap_decision": "accept | reject",
        "gap_feedback": "REQUIRED when reject: the concrete re-plan brief"
    });
    // A dedicated critic role fragment (the burst-facing role text is the
    // plan-critic posture).
    let (gap_plan_critic_role, gap_plan_critic_source_of_truth) = if request.is_pv {
        (
            "pv/stuck_audit/01_gap_plan_critic_role.md",
            pv_stuck_audit_source_of_truth(request),
        )
    } else {
        (
            "stuck_math_audit/common/01_gap_plan_critic_role.md",
            "stuck_math_audit/common/02_reference_paper.md",
        )
    };
    let mut prompt_fragments = vec![
        gap_plan_critic_role,
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        gap_plan_critic_source_of_truth,
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "stuck_math_audit/common/03c_process_rules.md",
        // PV substitutive: the critic output-contract references "plus the
        // paper"; the PV variant re-points to the crate / GOAL.md / pinned model.
        if request.is_pv {
            "pv/stuck_audit/05_gap_plan_critic_output_contract.md"
        } else {
            "stuck_math_audit/common/05_gap_plan_critic_output_contract.md"
        },
        "shared/90_artifact_delivery.md",
        structured_request_pointer_fragment(request),
    ];
    if reference_papers_fragments_active(request) {
        prompt_fragments.insert(4, "stuck_math_audit/common/02c_reference_papers.md");
    }
    let contract = json!({
        "prompt_fragments": prompt_fragments,
        "burst_role": "gap_plan_critic",
        "request_summary": {
            "phase": request.phase,
            "scenario": "gap_plan_critic",
            "cycle": request.cycle,
            "request_id": request.id,
        },
        // The ONLY things the critic receives about the plan: the route
        // .tex and the gap brief. No planner reasoning crosses here.
        "gap_brief": context.gap_brief,
        "route_tex": context.route_tex,
        // PV substitutive: no Deviation lane. The PV posture grounds ACCEPT in
        // what the pinned model supports and drops the deviation-path sentence.
        "critic_posture": if request.is_pv {
            "Falsification-first. Try to BREAK the route. Your ACCEPT is the sole authorization standing in for a removed human — default toward REJECT when the soundness case against the pinned model is not airtight. On ACCEPT, write the report + tasks that direct a worker to implement the route. When the route is non-minimal, rests on missing mathematics, or is not supported by the pinned model, REJECT with feedback steering toward a human escalation."
        } else {
            "Falsification-first. Try to BREAK the route. Your ACCEPT is the sole authorization standing in for a removed human — default toward REJECT when the faithfulness/soundness case is not airtight. On ACCEPT, write the report + tasks that direct a worker to implement the route; a route involving a deviation must direct the worker to author it through the normal deviation_requests path (the Deviation + Substantiveness lanes check it). When the route is non-minimal or rests on missing mathematics, REJECT with feedback steering toward a human escalation."
        },
        "artifact_contract": {
            "result_type": "stuck_math_audit_result_v1",
            "report_min_chars": crate::model::AUDIT_REPORT_TEXT_MIN_CHARS,
            "report_max_chars": crate::model::AUDIT_REPORT_TEXT_MAX_CHARS,
            "prompt_schema_example": prompt_schema_example,
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-stuck-math-audit-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ], &[]),
    });
    with_prompt_facing_view(contract)
}

fn assumptions_lane_contract_payload(
    request: &WrapperRequest,
    lane: &crate::model::AssumptionsLaneContext,
) -> Value {
    let prompt_schema_example = json!({
        "assumptions_lane_verdict": "pass | reject",
        "assumptions_lane_reason": "REQUIRED when reject: the eligibility / citation-resolves / corpus / hunt / hook-scope finding",
        "assumptions_lane_hunt_result": "the class-appropriate Rust hunt result (corroboration only: 'failed to refute over N runs' / 'constructed and checked witnesses up to N bytes'); never claims to ESTABLISH C",
        "assumptions_lane_probe_result": "optional diagnostic: outcome of the in-model Lean refutation attempt of C's unconditional form (a scratch `example : ¬ C'` compiled against the tablet)",
    });
    let prompt_fragments = vec![
        "pv/stuck_audit/08_assumptions_lane_role.md",
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        pv_stuck_audit_source_of_truth(request),
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "stuck_math_audit/common/03c_process_rules.md",
        "shared/90_artifact_delivery.md",
        structured_request_pointer_fragment(request),
    ];
    let contract = json!({
        "prompt_fragments": prompt_fragments,
        "burst_role": "assumptions_lane",
        "request_summary": {
            "phase": request.phase,
            "scenario": "assumptions_lane",
            "cycle": request.cycle,
            "request_id": request.id,
        },
        // The ONLY things the lane receives about `C`: the candidate + the
        // worker-authored statement / locator / justification. No auditor
        // disproof reasoning crosses here (context isolation).
        "candidate_under_model_assumption": {
            "candidate_invariant": lane.candidate_invariant,
            "axiom_name": lane.axiom_name,
            "lean_statement": lane.lean_statement,
            "citation_locator": lane.citation_locator,
            "rust_justification": lane.rust_justification,
            "needed_by": lane.needed_by,
            "claim_class": crate::assumptions_registry::normalize_claim_class(&lane.claim_class),
        },
        "lane_posture": "Gate C only. (1) Citation resolves in the fetched Rust-docs corpus. (2) Citation guarantees C and matches Lean. (3) The class-appropriate Rust hunt is run and recorded — behavior: refutation-shaped; domain: construction-shaped (build witnesses in safe Rust, check agreement with C, report the size bound reached). (4) C is a Rust language/compiler guarantee about what the language admits, independent of any program. (5) Quantified over-approximating container variables sit under their validity hook; a domain claim's existential witness asserts hook membership. A domain PASS takes the human AssumptionReview gate live immediately. PASS only if all checks hold.",
        "artifact_contract": {
            "result_type": "stuck_math_audit_result_v1",
            "report_min_chars": crate::model::AUDIT_REPORT_TEXT_MIN_CHARS,
            "report_max_chars": crate::model::AUDIT_REPORT_TEXT_MAX_CHARS,
            "prompt_schema_example": prompt_schema_example,
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-stuck-math-audit-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ], &[]),
    });
    with_prompt_facing_view(contract)
}

/// Cleanup-v2 Step 12 (2026-05-14): audit-burst prompt contract.
/// Populated for `RequestKind::Audit` only; otherwise returns a
/// dormant payload. Surfaces:
///   - Phase + audit round + burst index + max bursts per round
///   - The live DAG view (present_nodes, deps, target_claims)
///   - Protected-statement node set (statements + protected closure)
///   - Current `cleanup_audit_tasks` (rendered with status, kind,
///     confidence, rationale, audit_origin_round)
///   - Current `cleanup_audit_scratchpad`
///   - Latest audit rejection reason (when re-issuing after a
///     validation-fail or malformed response)
///   - Artifact contract for the `AuditResponse` JSON shape
/// Lean's stock elaboration budget, the baseline a node's own
/// `set_option maxHeartbeats` is judged against.
const DEFAULT_MAX_HEARTBEATS: u64 = 200_000;

/// The largest elaboration budget a node's source grants itself, when that
/// grant exceeds the stock one.
///
/// Read straight from the node file rather than from any record: it states
/// what the author had to allow for the declarations to elaborate, so it is
/// evidence the node outgrew the default budget whether or not a measurement
/// exists for its current content.
///
/// Only budgets above `DEFAULT_MAX_HEARTBEATS` qualify. A node may also
/// *tighten* the option below the default — a claim of cheapness, the opposite
/// of what this signal reports — and reporting that as an override would tell
/// the consumer the reverse of the truth. Lean reads `0` as "no limit", which
/// orders above every finite grant rather than below them.
fn max_heartbeats_override(repo_path: &Path, node_name: &str) -> Option<u64> {
    let path = repo_path
        .join("Tablet")
        .join(format!("{node_name}.lean"));
    let text = fs::read_to_string(path).ok()?;
    let mut best: Option<u64> = None;
    for chunk in text.split("set_option maxHeartbeats").skip(1) {
        let Some(granted) = chunk
            .split_whitespace()
            .next()
            .and_then(|token| token.parse::<u64>().ok())
        else {
            continue;
        };
        let ranks_above = |a: u64, b: u64| if a == 0 || b == 0 { 0 } else { a.max(b) };
        best = Some(best.map_or(granted, |seen: u64| ranks_above(seen, granted)));
    }
    best.filter(|granted| *granted == 0 || *granted > DEFAULT_MAX_HEARTBEATS)
}

/// Advisory per-node elaboration cost, rendered for the Cleanup audit.
///
/// Ranking information for ExtractHelper proposals and nothing else. The
/// numbers are emitted raw, unsorted, and uncompared — no tiers, no
/// thresholds, no ordering — because `walltime_ms` and `peak_rss_kib` are
/// machine- and load-dependent and may never gate a decision. The consumer
/// does its own ranking with the caveats stated in its prompt fragment.
///
/// A node earns an entry by having a measurement current with its present
/// content, or by raising `maxHeartbeats`, or both. Absence means there is no
/// measurement for the content the node has now; a stale record and a node
/// that was never measured are deliberately indistinguishable here, matching
/// the collapse `fresh_elaboration_cost` performs for every other reader.
fn node_elaboration_cost_view(request: &WrapperRequest, repo_path: Option<&Path>) -> Value {
    let Some(repo) = repo_path else {
        return json!({});
    };
    let mut rendered = serde_json::Map::new();
    for node in &request.current_present_nodes {
        let mut entry = serde_json::Map::new();
        if let Some(record) = request.elaboration_cost_view.get(node) {
            let current_content = record.cost_version
                == crate::model::CURRENT_ELABORATION_COST_VERSION
                && crate::cache_key::lean_closure_cache_key(repo, node.as_str())
                    .is_some_and(|key| key == record.source_closure_hash);
            if current_content {
                entry.insert("walltime_ms".to_string(), json!(record.walltime_ms));
                entry.insert("peak_rss_kib".to_string(), json!(record.peak_rss_kib));
                entry.insert(
                    "olean_size_bytes".to_string(),
                    json!(record.olean_size_bytes),
                );
                // Absent in ordinary operation: heartbeats are an in-process
                // Lean counter, so nothing at the lake-build boundary can read
                // them. Rendered when a measurement pass has supplied one.
                if let Some(heartbeats) = record.heartbeats {
                    entry.insert("heartbeats".to_string(), json!(heartbeats));
                    entry.insert(
                        "heartbeats_measured_for_current_content".to_string(),
                        json!(record.heartbeats_are_current()),
                    );
                }
            }
        }
        if let Some(granted) = max_heartbeats_override(repo, node.as_str()) {
            entry.insert("max_heartbeats_override".to_string(), json!(granted));
        }
        if !entry.is_empty() {
            rendered.insert(node.to_string(), Value::Object(entry));
        }
    }
    Value::Object(rendered)
}

/// Proof-body shingle width. This is detection resolution, not a proposal
/// threshold: every region the scan finds is emitted raw and remains
/// eligible for audit proposal.
const DEDUP_SCAN_K: usize = 50;
const DEDUP_REGION_CAP: usize = 25;
const DEDUP_SCAN_VERSION: u32 = 1;

/// Lean words that carry syntax/tactic meaning rather than local-name
/// identity. Alpha normalization rewrites only lowercase-initial, undotted
/// identifiers outside this list. The list is deliberately a constant in
/// the kernel so scan-version changes are reviewable and replay-visible.
const DEDUP_ALPHA_KEYWORDS: &[&str] = &[
    "abbrev",
    "aesop",
    "all_goals",
    "apply",
    "assumption",
    "at",
    "attribute",
    "axiom",
    "by",
    "calc",
    "case",
    "cases",
    "class",
    "constructor",
    "contradiction",
    "def",
    "decreasing_by",
    "deriving",
    "do",
    "else",
    "end",
    "exact",
    "exists",
    "false_or_by_contra",
    "first",
    "focus",
    "for",
    "fun",
    "generalize",
    "have",
    "if",
    "import",
    "induction",
    "inductive",
    "in",
    "include",
    "infer_instance",
    "instance",
    "intro",
    "intros",
    "let",
    "match",
    "next",
    "nomatch",
    "obtain",
    "omega",
    "open",
    "or",
    "rcases",
    "rfl",
    "rw",
    "set",
    "show",
    "simp",
    "simp_all",
    "simpa",
    "solve",
    "specialize",
    "structure",
    "suffices",
    "theorem",
    "then",
    "try",
    "unfold",
    "universe",
    "variable",
    "where",
    "with",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DedupMatchKind {
    Exact,
    Alpha,
}

impl DedupMatchKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Alpha => "alpha",
        }
    }
}

#[derive(Clone, Debug)]
struct DedupBodyLine {
    normalized: String,
    /// Zero-based line within the raw region below `-- BODY`.
    raw_body_line: usize,
}

#[derive(Clone, Debug)]
struct DedupBody {
    lines: Vec<DedupBodyLine>,
}

#[derive(Clone, Debug)]
struct DedupAtomicWindow {
    node_set: BTreeSet<NodeId>,
    /// Per-node half-open spans in the comment/blank-stripped body view.
    spans: BTreeMap<NodeId, (usize, usize)>,
    match_kind: DedupMatchKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DedupGroup {
    node_set: BTreeSet<NodeId>,
    spans: BTreeMap<NodeId, (usize, usize)>,
    match_kind: DedupMatchKind,
    block_lines: usize,
    recoverable: usize,
}

fn normalized_dedup_body(content: &str) -> Option<DedupBody> {
    let (_, body) = content.split_once("-- BODY")?;
    let lines = body
        .lines()
        .enumerate()
        .filter_map(|(raw_body_line, line)| {
            // This scan is advisory and deliberately lexical. Strip the
            // first line comment, collapse whitespace, and ignore blanks;
            // acceptance-time compilation and outcome gates are authoritative.
            let code = line.split_once("--").map_or(line, |(code, _)| code);
            let normalized = code.split_whitespace().collect::<Vec<_>>().join(" ");
            (!normalized.is_empty()).then_some(DedupBodyLine {
                normalized,
                raw_body_line,
            })
        })
        .collect();
    Some(DedupBody { lines })
}

fn is_alpha_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '\'' || ch == '.'
}

fn alpha_normalize_window(lines: &[DedupBodyLine]) -> String {
    let input = lines
        .iter()
        .map(|line| line.normalized.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    let mut output = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !is_alpha_identifier_char(chars[i]) || chars[i] == '.' {
            output.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_alpha_identifier_char(chars[i]) {
            i += 1;
        }
        let token: String = chars[start..i].iter().collect();
        let rewritable = token
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
            && !token.contains('.')
            && !DEDUP_ALPHA_KEYWORDS.iter().any(|keyword| *keyword == token);
        if rewritable {
            let next = names.len();
            let ordinal = *names.entry(token).or_insert(next);
            let _ = write!(output, "v{ordinal}");
        } else {
            output.push_str(&token);
        }
    }
    output
}

fn exact_window_text(lines: &[DedupBodyLine]) -> String {
    lines
        .iter()
        .map(|line| line.normalized.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn dedup_window_hash(lines: &[DedupBodyLine], match_kind: DedupMatchKind) -> String {
    let normalized = match match_kind {
        DedupMatchKind::Exact => exact_window_text(lines),
        DedupMatchKind::Alpha => alpha_normalize_window(lines),
    };
    crate::cache_key::hash_text(&normalized)
}

fn dedup_atomic_windows(
    bodies: &BTreeMap<NodeId, DedupBody>,
    match_kind: DedupMatchKind,
) -> Vec<DedupAtomicWindow> {
    // hash -> node -> ordered occurrence starts. BTree throughout: these
    // maps feed payload bytes and may never inherit randomized iteration.
    let mut occurrences: BTreeMap<String, BTreeMap<NodeId, Vec<usize>>> = BTreeMap::new();
    for (node, body) in bodies {
        if body.lines.len() < DEDUP_SCAN_K {
            continue;
        }
        for start in 0..=body.lines.len() - DEDUP_SCAN_K {
            let hash = dedup_window_hash(
                &body.lines[start..start + DEDUP_SCAN_K],
                match_kind,
            );
            occurrences
                .entry(hash)
                .or_default()
                .entry(node.clone())
                .or_default()
                .push(start);
        }
    }

    let mut atomic = Vec::new();
    for by_node in occurrences.into_values() {
        if by_node.len() < 2 {
            continue;
        }
        let node_set: BTreeSet<NodeId> = by_node.keys().cloned().collect();
        // Repeated identical shingles in one node are paired by occurrence
        // rank. This is deterministic and avoids an exponential Cartesian
        // product; ordinary proof blocks have one occurrence per node.
        let copies = by_node.values().map(Vec::len).min().unwrap_or(0);
        for occurrence_index in 0..copies {
            let spans = by_node
                .iter()
                .map(|(node, starts)| {
                    let start = starts[occurrence_index];
                    (node.clone(), (start, start + DEDUP_SCAN_K))
                })
                .collect();
            atomic.push(DedupAtomicWindow {
                node_set: node_set.clone(),
                spans,
                match_kind,
            });
        }
    }
    atomic.sort_by(|a, b| {
        a.match_kind
            .cmp(&b.match_kind)
            .then_with(|| a.node_set.cmp(&b.node_set))
            .then_with(|| a.spans.cmp(&b.spans))
    });
    atomic
}

fn spans_overlap(left: (usize, usize), right: (usize, usize)) -> bool {
    left.0 < right.1 && right.0 < left.1
}

fn merge_dedup_windows(atomic: Vec<DedupAtomicWindow>) -> Vec<DedupGroup> {
    let mut merged: Vec<DedupGroup> = Vec::new();
    for window in atomic {
        let compatible = merged.iter_mut().rev().find(|group| {
            group.match_kind == window.match_kind
                && group.node_set == window.node_set
                && group.node_set.iter().all(|node| {
                    group
                        .spans
                        .get(node)
                        .zip(window.spans.get(node))
                        .is_some_and(|(left, right)| spans_overlap(*left, *right))
                })
        });
        if let Some(group) = compatible {
            for (node, (start, end)) in window.spans {
                if let Some(span) = group.spans.get_mut(&node) {
                    span.0 = span.0.min(start);
                    span.1 = span.1.max(end);
                }
            }
            group.block_lines = group
                .spans
                .values()
                .map(|(start, end)| end - start)
                .min()
                .unwrap_or(0);
            group.recoverable = group
                .block_lines
                .saturating_mul(group.node_set.len().saturating_sub(1));
        } else {
            let block_lines = window
                .spans
                .values()
                .map(|(start, end)| end - start)
                .min()
                .unwrap_or(0);
            let recoverable = block_lines.saturating_mul(window.node_set.len() - 1);
            merged.push(DedupGroup {
                node_set: window.node_set,
                spans: window.spans,
                match_kind: window.match_kind,
                block_lines,
                recoverable,
            });
        }
    }
    merged.sort_by(|a, b| {
        a.match_kind
            .cmp(&b.match_kind)
            .then_with(|| a.node_set.cmp(&b.node_set))
            .then_with(|| a.spans.cmp(&b.spans))
    });
    // Every exact match is necessarily also an alpha match. Keep the exact
    // view and suppress only byte-identical alpha shadows; wider/narrower
    // alpha groups remain real alternatives.
    let exact_shapes: BTreeSet<(BTreeSet<NodeId>, BTreeMap<NodeId, (usize, usize)>)> = merged
        .iter()
        .filter(|group| group.match_kind == DedupMatchKind::Exact)
        .map(|group| (group.node_set.clone(), group.spans.clone()))
        .collect();
    merged
        .into_iter()
        .filter(|group| {
            group.match_kind == DedupMatchKind::Exact
                || !exact_shapes.contains(&(group.node_set.clone(), group.spans.clone()))
        })
        .collect()
}

fn groups_overlap(left: &DedupGroup, right: &DedupGroup) -> bool {
    left.node_set.intersection(&right.node_set).any(|node| {
        left.spans
            .get(node)
            .zip(right.spans.get(node))
            .is_some_and(|(a, b)| spans_overlap(*a, *b))
    })
}

fn dedup_group_preference(left: &DedupGroup, right: &DedupGroup) -> std::cmp::Ordering {
    right
        .recoverable
        .cmp(&left.recoverable)
        .then_with(|| right.node_set.len().cmp(&left.node_set.len()))
        .then_with(|| left.node_set.cmp(&right.node_set))
        // An exact and alpha view can otherwise tie on all mandated keys;
        // exact is the stronger observation and wins that final tie.
        .then_with(|| left.match_kind.cmp(&right.match_kind))
        .then_with(|| left.spans.cmp(&right.spans))
}

fn dedup_regions(groups: &[DedupGroup]) -> Vec<Vec<usize>> {
    // Deterministic union-find: groups arrive stably sorted, pairs are
    // visited lexicographically, and roots always attach toward the lower
    // index. No payload byte depends on allocation or hash iteration.
    let mut parent: Vec<usize> = (0..groups.len()).collect();
    fn find(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            let grandparent = parent[parent[index]];
            parent[index] = grandparent;
            index = grandparent;
        }
        index
    }
    for left in 0..groups.len() {
        for right in left + 1..groups.len() {
            if !groups_overlap(&groups[left], &groups[right]) {
                continue;
            }
            let left_root = find(&mut parent, left);
            let right_root = find(&mut parent, right);
            if left_root != right_root {
                let (low, high) = if left_root < right_root {
                    (left_root, right_root)
                } else {
                    (right_root, left_root)
                };
                parent[high] = low;
            }
        }
    }
    let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..groups.len() {
        let root = find(&mut parent, index);
        by_root.entry(root).or_default().push(index);
    }
    by_root.into_values().collect()
}

fn dedup_identifier_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !(chars[i].is_ascii_alphabetic() || chars[i] == '_') {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < chars.len()
            && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '\'')
        {
            i += 1;
        }
        tokens.push(chars[start..i].iter().collect());
    }
    tokens
}

fn bound_names_in_block(lines: &[DedupBodyLine]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in lines {
        let tokens = dedup_identifier_tokens(&line.normalized);
        for binder in ["have", "set", "let"] {
            if let Some(index) = tokens.iter().position(|token| token == binder) {
                if let Some(name) = tokens.get(index + 1) {
                    if name
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_ascii_lowercase())
                    {
                        names.insert(name.clone());
                    }
                }
            }
        }
        if tokens.first().is_some_and(|token| token == "obtain") {
            for name in tokens.iter().skip(1) {
                if name == "from" || name == "with" {
                    break;
                }
                if name
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_lowercase())
                    && !DEDUP_ALPHA_KEYWORDS.iter().any(|keyword| keyword == name)
                {
                    names.insert(name.clone());
                }
            }
        }
    }
    names
}

fn post_block_dependence(
    group: &DedupGroup,
    bodies: &BTreeMap<NodeId, DedupBody>,
) -> Vec<Value> {
    group
        .node_set
        .iter()
        .filter_map(|node| {
            let body = bodies.get(node)?;
            let (start, end) = *group.spans.get(node)?;
            let bound = bound_names_in_block(&body.lines[start..end]);
            let tail_tokens: Vec<String> = body.lines[end..]
                .iter()
                .flat_map(|line| dedup_identifier_tokens(&line.normalized))
                .collect();
            let mut referenced = Vec::new();
            let mut reference_count = 0usize;
            for name in bound {
                let count = tail_tokens.iter().filter(|token| **token == name).count();
                if count > 0 {
                    reference_count = reference_count.saturating_add(count);
                    if referenced.len() < 20 {
                        referenced.push(name);
                    }
                }
            }
            let tail_set: BTreeSet<&str> = tail_tokens.iter().map(String::as_str).collect();
            let closers: Vec<&str> = ["omega", "simp_all", "aesop", "assumption"]
                .into_iter()
                .filter(|closer| tail_set.contains(closer))
                .collect();
            Some(json!({
                "node": node,
                "block_bound_names_referenced_after": referenced,
                "reference_count": reference_count,
                "context_consuming_closers_after_block": closers,
            }))
        })
        .collect()
}

fn rendered_member(
    node: &NodeId,
    normalized_span: (usize, usize),
    bodies: &BTreeMap<NodeId, DedupBody>,
) -> Value {
    let raw_span = bodies.get(node).and_then(|body| {
        let (start, end) = normalized_span;
        let first = body.lines.get(start)?.raw_body_line;
        let last = body.lines.get(end.checked_sub(1)?)?.raw_body_line + 1;
        Some([first, last])
    });
    json!({
        "node": node,
        "body_span": raw_span.unwrap_or([normalized_span.0, normalized_span.1]),
    })
}

fn dedup_region_id(component: &[usize], groups: &[DedupGroup]) -> String {
    let mut canonical = String::new();
    for &index in component {
        let group = &groups[index];
        let _ = write!(
            canonical,
            "{}|{}|{}|",
            group.match_kind.as_str(),
            group.block_lines,
            group.recoverable
        );
        for node in &group.node_set {
            let span = group.spans.get(node).copied().unwrap_or_default();
            let _ = write!(canonical, "{}:{}-{};", node.as_str(), span.0, span.1);
        }
        canonical.push('\n');
    }
    crate::cache_key::hash_text(&canonical)
        .chars()
        .take(12)
        .collect()
}

/// Deterministic, request-build-time view of cross-node proof duplication.
///
/// The scan is intentionally paid only for Cleanup audit requests. On the
/// live corpus the validated Python predecessor completed in seconds; this
/// Rust port is a handful of near-linear ordered-map passes over roughly
/// 500k proof-body lines, negligible beside an approximately 18-minute
/// worker burst. The result is never stored in protocol state: replay builds
/// it again from the same Tablet bytes, just like `node_elaboration_cost_view`.
fn shared_proof_block_view(repo_path: Option<&Path>) -> Value {
    let mut bodies: BTreeMap<NodeId, DedupBody> = BTreeMap::new();
    if let Some(repo) = repo_path {
        if let Ok(entries) = fs::read_dir(repo.join("Tablet")) {
            let mut lean_paths: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if path.is_file() && name.ends_with(".lean") {
                    lean_paths.insert(name.to_string(), path);
                }
            }
            for (name, path) in lean_paths {
                let Some(node) = name.strip_suffix(".lean") else {
                    continue;
                };
                let Ok(content) = fs::read_to_string(path) else {
                    continue;
                };
                if let Some(body) = normalized_dedup_body(&content) {
                    bodies.insert(NodeId::from(node), body);
                }
            }
        }
    }

    let mut atomic = dedup_atomic_windows(&bodies, DedupMatchKind::Exact);
    atomic.extend(dedup_atomic_windows(&bodies, DedupMatchKind::Alpha));
    atomic.sort_by(|a, b| {
        a.match_kind
            .cmp(&b.match_kind)
            .then_with(|| a.node_set.cmp(&b.node_set))
            .then_with(|| a.spans.cmp(&b.spans))
    });
    let groups = merge_dedup_windows(atomic);
    let components = dedup_regions(&groups);
    let mut rendered_regions: Vec<(DedupGroup, Value)> = Vec::new();
    for mut component in components {
        component.sort_by(|left, right| dedup_group_preference(&groups[*left], &groups[*right]));
        let Some(&representative_index) = component.first() else {
            continue;
        };
        let representative = groups[representative_index].clone();
        let alternatives: Vec<Value> = component
            .iter()
            .skip(1)
            .map(|&index| {
                let group = &groups[index];
                json!({
                    "match": group.match_kind.as_str(),
                    "n_nodes": group.node_set.len(),
                    "block_lines": group.block_lines,
                    "recoverable": group.recoverable,
                    "members": group.node_set,
                })
            })
            .collect();
        let members: Vec<Value> = representative
            .spans
            .iter()
            .map(|(node, span)| rendered_member(node, *span, &bodies))
            .collect();
        let has_lemma_shaped_member = representative.spans.iter().any(|(node, span)| {
            bodies
                .get(node)
                .is_some_and(|body| span.0 == 0 && span.1 == body.lines.len())
        });
        let region_id = dedup_region_id(&component, &groups);
        let value = json!({
            "region_id": region_id,
            "match": representative.match_kind.as_str(),
            "block_lines": representative.block_lines,
            "n_nodes": representative.node_set.len(),
            "recoverable": representative.recoverable,
            "members": members,
            "nested_alternatives": alternatives,
            "has_lemma_shaped_member": has_lemma_shaped_member,
            "post_block_dependence": post_block_dependence(&representative, &bodies),
        });
        rendered_regions.push((representative, value));
    }
    rendered_regions.sort_by(|(left, _), (right, _)| {
        right
            .recoverable
            .cmp(&left.recoverable)
            .then_with(|| left.block_lines.cmp(&right.block_lines))
            .then_with(|| left.node_set.cmp(&right.node_set))
            .then_with(|| left.match_kind.cmp(&right.match_kind))
            .then_with(|| left.spans.cmp(&right.spans))
    });
    let regions_total = rendered_regions.len();
    let regions: Vec<Value> = rendered_regions
        .into_iter()
        .take(DEDUP_REGION_CAP)
        .map(|(_, value)| value)
        .collect();
    json!({
        "scan_version": DEDUP_SCAN_VERSION,
        "scan_params": {"k": DEDUP_SCAN_K},
        "regions_total": regions_total,
        "truncated": regions_total > DEDUP_REGION_CAP,
        "regions": regions,
    })
}

pub fn audit_contract_payload(request: &WrapperRequest, repo_path: Option<&Path>) -> Value {
    if request.kind != crate::model::RequestKind::Audit {
        return no_audit_contract_payload();
    }
    let tasks_view: Vec<Value> = request
        .cleanup_audit_tasks_view
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "task_index": i,
                "target_node": t.target_node,
                "rationale": t.rationale,
                "confidence": t.confidence,
                "kind": t.kind,
                "status": t.status,
                "audit_origin_round": t.audit_origin_round,
                "swept_parents": t.swept_parents,
                "region_block_lines": t.region_block_lines,
            })
        })
        .collect();
    let mut payload = json!({
        "prompt_fragments": [
            "audit/00_intro.md",
            "shared/10_repository_root.md",
            "audit/03_state.md",
            "audit/05_loop_semantics.md",
            "audit/10_target_constraints.md",
            "audit/12_elaboration_cost.md",
            "audit/13_shared_proof_blocks.md",
            "audit/20_artifact_contract.md",
            "shared/90_artifact_delivery.md",
            structured_request_pointer_fragment(request),
        ],
        "request_summary": {
            "phase": request.phase,
            "scenario": "cleanup_audit",
            "audit_round": request.cleanup_audit_round_view,
            "audit_burst_index": request.cleanup_audit_burst_count_view,
            "max_bursts_per_round": crate::model::CLEANUP_AUDIT_MAX_BURSTS_PER_ROUND,
            "max_rounds": crate::model::CLEANUP_AUDIT_MAX_ROUNDS,
        },
        "dag_view": {
            "present_nodes": request.current_present_nodes,
            "deps": request.current_deps,
            "target_claims": request.current_target_claims,
            "configured_targets": request.configured_targets,
        },
        "challenge_view": if request.configured_challenge_targets.is_empty() {
            json!(null)
        } else {
            json!({
                "configured_challenge_targets": challenge_registry_json(request),
                "challenge_coverage": challenge_coverage_json(request),
            })
        },
        "protected_statement_node_set": request.cleanup_protected_statement_node_set_view,
        "cleanup_audit_tasks": tasks_view,
        "cleanup_audit_scratchpad": request.cleanup_audit_scratchpad_view,
        "latest_audit_rejection_reason": request.latest_audit_rejection_reason_view,
        "node_elaboration_cost": node_elaboration_cost_view(request, repo_path),
        "shared_proof_blocks": shared_proof_block_view(repo_path),
        "artifact_contract": {
            "result_type": "cleanup_audit_result_v1",
            "prompt_schema_example": {
                "new_tasks": [
                    {
                        "target_node": "NodeId",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "substitution",
                            "replacement": {
                                "kind": "mathlib",
                                "citation": "Nat.add_comm"
                            }
                        }
                    },
                    {
                        "target_node": "NodeId",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "substitution",
                            "replacement": {
                                "kind": "tablet_wrapper",
                                "node": "ReplacementNodeId"
                            }
                        }
                    },
                    {
                        "target_node": "NodeId",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "lint_fix",
                            "warning_text": "the build warning to eliminate"
                        }
                    },
                    {
                        "target_node": "ParentNodeId",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "dead_code_elim",
                            "hint": "likely-dead have/block locations"
                        }
                    },
                    {
                        "target_node": "ParentNodeId",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "extract_helper",
                            "ordinal": 1,
                            "hint": "which block of the proof to lift out"
                        }
                    },
                    {
                        "target_node": "CanonicalLeastParent",
                        "rationale": "free-form audit reasoning",
                        "confidence": "high | medium | low",
                        "kind": {
                            "kind": "extract_shared",
                            "co_parents": ["OtherParent", "ThirdParent"],
                            "ordinal": 1,
                            "hint": "region id and strengthening guidance"
                        }
                    }
                ],
                "task_modifications": [
                    {"task_index": 0, "reason": "second-look: not actually a wrapper"}
                ],
                "scratchpad_replace": "scratchpad text to carry across bursts",
                "outcome": "audit_done | need_to_continue"
            }
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-audit-result",
            "{{raw_output_path}}",
        ], &[]),
    });
    // Cleanup-v2 migration (2026-06-04): pre-compute the trimmed inline
    // view the bridge renders in the prompt. Mirrors what
    // `bridge_prompts._prompt_facing_audit_contract` used to do: keep
    // only `request_summary` and `artifact_contract.result_type`. The
    // dropped fields (`dag_view`, `protected_statement_node_set`,
    // `cleanup_audit_tasks`, `cleanup_audit_scratchpad`,
    // `latest_audit_rejection_reason`) are rendered via the dedicated
    // placeholders in `audit/03_state.md`; the full payload still
    // ships via `structured_request_path`.
    let mut view = serde_json::Map::new();
    if let Some(rs) = payload.get("request_summary").cloned() {
        view.insert("request_summary".to_string(), rs);
    }
    if let Some(result_type) = payload
        .get("artifact_contract")
        .and_then(|ac| ac.get("result_type"))
        .cloned()
    {
        view.insert(
            "artifact_contract".to_string(),
            json!({"result_type": result_type}),
        );
    }
    if let Some(map) = payload.as_object_mut() {
        map.insert("prompt_facing_view".to_string(), Value::Object(view));
    }
    payload
}

pub fn stuck_math_audit_contract_payload(request: &WrapperRequest) -> Value {
    if request.kind != crate::model::RequestKind::StuckMathAudit {
        return no_stuck_math_audit_contract_payload();
    }
    let is_need_input_auditor = request.stuck_math_audit.need_input_audit.is_some();
    // GapResearch planner / critic lanes. Mutually exclusive with
    // need_input_audit and the global-repair request (audit-role mutex).
    // The Planner produces a natural-language `route_tex`; the Critic
    // returns `gap_decision`. The critic payload carries ONLY the route +
    // brief — never the planner's reasoning (context isolation).
    let gap_research = request.stuck_math_audit.gap_research.as_ref();
    let gap_plan_critique = request.stuck_math_audit.gap_plan_critique.as_ref();
    // Revision-planning lane (`revision_plan.md` §9). Structurally mutually
    // exclusive with NeedInputAuditor / global-repair / GapResearch
    // planner+critic: we early-return here, and a debug-assert guards against a
    // co-set scenario field reaching this branch (the `StuckMathAuditState`
    // audit-role mutex in `check_invariants` is the runtime enforcement; this
    // assert localizes the violation to contract construction).
    if let Some(revision_planning) = request.stuck_math_audit.revision_planning.as_ref() {
        debug_assert!(
            !is_need_input_auditor
                && gap_research.is_none()
                && gap_plan_critique.is_none()
                && request.stuck_math_audit.initial_planning.is_none()
                && request.pending_global_repair_request.is_none(),
            "revision_planning StuckMathAudit lane must not be co-set with \
             need_input_audit / gap_research / gap_plan_critique / \
             initial_planning / pending_global_repair_request (revision_plan.md §9)"
        );
        return revision_planner_contract_payload(request, revision_planning);
    }
    // Fresh-run initial-planning lane. Structurally mutually exclusive with
    // every other audit lane (the `StuckMathAuditState` audit-role mutex);
    // the debug-assert localizes any co-set violation to contract
    // construction, mirroring the revision branch above.
    if let Some(initial_planning) = request.stuck_math_audit.initial_planning.as_ref() {
        debug_assert!(
            !is_need_input_auditor
                && gap_research.is_none()
                && gap_plan_critique.is_none()
                && request.pending_global_repair_request.is_none(),
            "initial_planning StuckMathAudit lane must not be co-set with \
             need_input_audit / gap_research / gap_plan_critique / \
             pending_global_repair_request"
        );
        return initial_planner_contract_payload(request, initial_planning);
    }
    if gap_research.is_some() {
        return gap_research_planner_contract_payload(request, gap_research.unwrap());
    }
    if gap_plan_critique.is_some() {
        return gap_plan_critic_contract_payload(request, gap_plan_critique.unwrap());
    }
    // Give-up adjudication critic lane: a blinded classification burst over a
    // give-up PROPOSAL that has not been recorded. Mutually exclusive with the
    // other audit lanes (audit-role mutex).
    // PV under-model (Slice 2): the ASSUMPTIONS-LANE contract. A distinct burst
    // from the auditor (no burst both proposes and certifies). Gates a
    // worker-authored `C` ONLY — the citation-resolves guard, reading the
    // corpus, the adversarial Rust hunt, and the language-guarantee-only rule.
    // Mutually exclusive with the other audit lanes (audit-role mutex).
    if let Some(lane) = request.stuck_math_audit.assumptions_lane.as_ref() {
        return assumptions_lane_contract_payload(request, lane);
    }
    // Suppress the cone_clean fragment + contract + schema-example
    // field when there are no allowed nodes to clean.
    // `resettable_theorem_stating_nodes` is populated only in
    // `Phase::ProofFormalization` (see
    // `ProtocolState::resettable_theorem_stating_nodes`); in
    // TheoremStating it is always empty, so cone_clean is suppressed
    // there without hardcoding phase. Mirrors other phase-conditional
    // fragment gating that keys off populated request data, not
    // `request.phase`.
    // Decide polarity-flip authority: when a `Decide` target is configured,
    // the audit lane (NeedInputAuditor or plain stuck-audit) is the
    // sole authority that flips a pair between proving `T` and disproving it via
    // `set_live_polarity`. Not offered to the global-repair auditor (a different
    // adjudication). No-op for non-Decide runs.
    // W6 (D1): same prose opening as the planner gate above.
    let decide_flip_available = request.is_pv
        && request.pending_global_repair_request.is_none()
        && has_actionable_decide_target(request);
    let cone_clean_available =
        !is_need_input_auditor && !request.resettable_theorem_stating_nodes.is_empty();
    let prompt_schema_example = if is_need_input_auditor {
        json!({
            "confirm_need_input": false,
            "report": "substantive audit report with concrete current evidence",
            "tasks": [
                {
                    "id": "task-1",
                    "title": "short title",
                    "body": "specific recovery task with evidence, expected impact, and suggested next check"
                }
            ],
            "probe_paths": [
                ".trellis/stuck-math-audit/cycle-N-request-M/probe.lean"
            ]
        })
    } else if cone_clean_available {
        json!({
            "report": "substantive audit report with concrete current evidence",
            "cone_clean_node": "optional node id from cone_clean_contract.allowed_nodes, or empty string",
            "tasks": [
                {
                    "id": "task-1",
                    "title": "short title",
                    "body": "specific task with evidence, expected impact, and suggested next check"
                }
            ],
            "probe_paths": [
                ".trellis/stuck-math-audit/cycle-N-request-M/probe.lean"
            ]
        })
    } else {
        json!({
            "report": "substantive audit report with concrete current evidence",
            "tasks": [
                {
                    "id": "task-1",
                    "title": "short title",
                    "body": "specific task with evidence, expected impact, and suggested next check"
                }
            ],
            "probe_paths": [
                ".trellis/stuck-math-audit/cycle-N-request-M/probe.lean"
            ]
        })
    };
    let mut prompt_schema_example = prompt_schema_example;
    if request.pending_global_repair_request.is_some() {
        let obj = prompt_schema_example
            .as_object_mut()
            .expect("prompt_schema_example is a JSON object");
        obj.insert("global_repair_approve".to_owned(), json!(true));
        obj.insert(
            "global_repair_approved_extension_node_ids".to_owned(),
            json!(["minimal subset of pending_global_repair_request.proposed_extension_node_ids"]),
        );
        obj.insert(
            "global_repair_auditor_reason".to_owned(),
            json!("brief decline reason; required iff global_repair_approve is false"),
        );
    }
    if decide_flip_available {
        let obj = prompt_schema_example
            .as_object_mut()
            .expect("prompt_schema_example is a JSON object");
        obj.insert(
            "set_live_polarity".to_owned(),
            json!("\"\", \"prove\", or \"disprove\" — flip a Decide pair's live polarity (empty to leave unchanged)"),
        );
        obj.insert(
            "set_live_polarity_target".to_owned(),
            json!("optional: the Decide PRIMARY challenge target id to flip; required when the pair is not the active/held node; only meaningful with set_live_polarity"),
        );
    }
    // Audit-ordered node retirement: advertise the field only where it is
    // legal — a ProofFormalization plan-writing audit outside the
    // global-repair Step B adjudication.
    if request.phase == Phase::ProofFormalization
        && !is_need_input_auditor
        && request.pending_global_repair_request.is_none()
    {
        let obj = prompt_schema_example
            .as_object_mut()
            .expect("prompt_schema_example is a JSON object");
        obj.insert(
            "node_retirement_request".to_owned(),
            json!("optional: {\"nodes\": [...], \"reason\": \"...\"} — order deletion of the named present, non-coarse, non-protected nodes as the next worker task; at most one of this / cone_clean_node"),
        );
    }
    // PV under-model (Slice 1): when this NeedInput audit is adjudicating a
    // target-bound model refutation (from a worker outcome or the typed
    // reviewer/Sound carrier), advertise the auditor's ruling fields. The
    // audit may select the strict route, but the sole mutator and its station
    // preconditions remain the only flip authority. Surfaced only on an
    // under-model audit, so every other auditor contract — math included —
    // is byte-identical.
    let under_model_audit = request
        .stuck_math_audit
        .need_input_audit
        .as_ref()
        .is_some_and(|ctx| !ctx.under_model_disproof.trim().is_empty());
    if under_model_audit {
        let obj = prompt_schema_example
            .as_object_mut()
            .expect("prompt_schema_example is a JSON object");
        obj.insert(
            "under_model_ruling".to_owned(),
            if request.trust_base_required_v1 {
                json!("\"bug\" means only that the supplied witness was verified to refute T in the extracted model, so route to Disprove; \"reject\" means it does not. Do not classify source reachability or run Rust here: the post-refutation claim-shape validator owns that later decision.")
            } else {
                json!("\"bug\" (the witness is a real, constructible Rust input ⇒ flip polarity to Disprove), \"deviation\" (the witness is reachable only by violating a Rust language invariant Aeneas dropped ⇒ authorize an assumption and NAME the candidate invariant C below), or \"reject\" (the disproof does not hold against the model ⇒ route back to ordinary proving). Use the Empirical Rust check to settle the witness-realizability question before ruling.")
            },
        );
        obj.insert(
            "under_model_candidate_invariant".to_owned(),
            if request.trust_base_required_v1 {
                json!("must be empty in trust protocol v2; attempt the Lean refutation or use the retained assumption-floor workflow")
            } else {
                json!("REQUIRED non-empty when under_model_ruling=deviation: name the candidate Rust language/compiler guarantee C. Empty otherwise.")
            },
        );
    }
    // PV under-model (approach-audit route): a PLAN-writing PV audit whose
    // plan recommends opening the assumption-authoring lane must ALSO name
    // the candidate `C` in the structured field — free-text task bodies are
    // not machine-readable, and the reviewer can only ENACT a structured
    // candidate (`assumption_authoring_request`), never restate one. The
    // worker-595 halt came from exactly this gap: a plan said "author the
    // assumption" but no structured `C` existed, so the reviewer routed an
    // ordinary worker at the Assumptions node and the checker rejected it.
    if request.is_pv && !under_model_audit && request.pending_global_repair_request.is_none() {
        let obj = prompt_schema_example
            .as_object_mut()
            .expect("prompt_schema_example is a JSON object");
        obj.insert(
            "under_model_candidate_invariant".to_owned(),
            if request.trust_base_required_v1 {
                json!("must be empty in trust protocol v2: the assumption-authoring lane is closed. Attempt the Lean refutation or use the retained assumption-floor workflow.")
            } else {
                json!("REQUIRED non-empty when (and only when) your audit plan recommends authoring a PV under-model assumption: state the candidate Rust language/compiler guarantee C precisely (the reviewer can only enact this structured field — a task body alone is NOT enactable). Empty otherwise.")
            },
        );
    }
    // PV substitutive: role + source-of-truth + theorem-stating-framing prose
    // re-anchor to the Rust crate + GOAL.md + pinned model (no paper). The
    // global-repair-auditor role has no PV variant authored, so it keeps the
    // math file even for PV (it carries no paper-specific anchor language).
    let role_fragment = if request.pending_global_repair_request.is_some() {
        "stuck_math_audit/common/01_global_repair_auditor_role.md"
    } else if is_need_input_auditor {
        if request.is_pv {
            "pv/stuck_audit/01_need_input_auditor_role.md"
        } else {
            "stuck_math_audit/common/01_need_input_auditor_role.md"
        }
    } else if request.is_pv {
        "pv/stuck_audit/01_role.md"
    } else {
        "stuck_math_audit/common/01_role.md"
    };
    let output_fragment = if is_need_input_auditor {
        // PV substitutive: the need-input output contract references a
        // "fundamental paper problem" / "paper-faithful path"; the PV variant
        // re-points to the goal being false/unprovable against the pinned model.
        if request.is_pv {
            "pv/stuck_audit/05_need_input_output_contract.md"
        } else {
            "stuck_math_audit/common/05_need_input_output_contract.md"
        }
    } else {
        stuck_audit_output_contract_fragment(request)
    };
    let source_of_truth_fragment = if request.is_pv {
        pv_stuck_audit_source_of_truth(request)
    } else {
        "stuck_math_audit/common/02_reference_paper.md"
    };
    let mut prompt_fragments = vec![
        role_fragment,
        "shared/10_repository_root.md",
        "shared/20_read_files.md",
        source_of_truth_fragment,
        "shared/25_filespec.md",
        "shared/30_project_invariants.md",
        "stuck_math_audit/common/03c_process_rules.md",
        "stuck_math_audit/common/02_request_context.md",
        "stuck_math_audit/common/02b_trigger_reason.md",
        stuck_audit_history_access_fragment(request),
        // Process memory: bridge-rendered block (settled entries for the
        // active cone + index) and the pending-challenge adjudication
        // list; renders empty (and drops out of the prompt) on runs with
        // no `process-memory/` directory and no pending challenges.
        "stuck_math_audit/common/03b_process_memory.md",
        stuck_audit_scratchpad_fragment(request),
    ];
    // Reference papers: right after the source-of-truth fragment
    // (index 3). Conditional, so registry-free audit prompts are
    // byte-identical. (02c, not 03: `03_history_access.md` owns 03 and
    // `02b_trigger_reason.md` owns 02b.)
    if reference_papers_fragments_active(request) {
        prompt_fragments.insert(4, "stuck_math_audit/common/02c_reference_papers.md");
    }
    if !is_need_input_auditor && request.phase == Phase::TheoremStating {
        let theorem_framing_fragment = if request.is_pv {
            "pv/stuck_audit/01b_theorem_stating_framing.md"
        } else {
            "stuck_math_audit/common/01b_theorem_stating_framing.md"
        };
        prompt_fragments.insert(1, theorem_framing_fragment);
        // Same helper-node policy the worker and reviewer see in
        // TheoremStating, so audit prescriptions don't reflexively call
        // for new helper nodes as the default soundness-repair move.
        prompt_fragments.push("review/common/33c_theorem_helper_policy.md");
    }
    if cone_clean_available {
        prompt_fragments.push("stuck_math_audit/common/04b_cone_clean.md");
    }
    if decide_flip_available {
        prompt_fragments.push("pv/stuck_audit/06_decide_polarity_flip.md");
    }
    if request.is_pv && request.trust_base_required_v1 && has_actionable_decide_target(request) {
        prompt_fragments.push("pv/stuck_audit/09_conditional_theorem_proposal.md");
    }
    // PV under-model (Slice 1): the target-bound adjudication instructions —
    // verify the refutation in-lane, then select the strict route. Shown only
    // on an under-model audit, so every other auditor prompt is byte-identical.
    if under_model_audit {
        // Stage 9 (claim B): the eligibility rule has ONE home and rides
        // both adjudication surfaces.
        prompt_fragments.push("pv/stuck_audit/06_deviation_eligibility.md");
        prompt_fragments.push(if request.trust_base_required_v1 {
            "pv/stuck_audit/08_model_refutation_adjudication_trust_v1.md"
        } else {
            "pv/stuck_audit/07_under_model_adjudication.md"
        });
    }
    prompt_fragments.extend([
        output_fragment,
        "shared/90_artifact_delivery.md",
        structured_request_pointer_fragment(request),
    ]);
    let cone_clean_contract = if cone_clean_available {
        json!({
            "allowed_nodes": request.resettable_theorem_stating_nodes,
            "response_field": "cone_clean_node",
            "optional": true,
            "semantics": "Optional coarse-node cone clean. Runtime restores the selected node to theorem-stating files, prunes orphaned helpers, and sends the audit plan to Review.",
        })
    } else {
        Value::Null
    };
    let confirm_need_input_contract = if is_need_input_auditor {
        json!({
            "response_field": "confirm_need_input",
            "true_semantics": "Confirm a real fundamental paper problem or paper/tablet impossibility requiring human input.",
            "false_semantics": "Reject the escalation and provide recovery tasks when the issue is fixable within the protocol.",
            "false_requires_tasks": true,
        })
    } else {
        Value::Null
    };
    let mut contract = json!({
        "prompt_fragments": prompt_fragments,
        "burst_role": if is_need_input_auditor { "need_input_auditor" } else { "stuck_math_audit" },
        "request_summary": {
            "phase": request.phase,
            // PV-neutral heading (string only; the lane/state/enum + the
            // routing `burst_role` keep their math names). The agent-facing
            // scenario reads "stuck-verification audit" for PV.
            "scenario": if is_need_input_auditor {
                "need_input_auditor"
            } else if request.is_pv {
                "stuck_verification_audit"
            } else {
                "stuck_math_audit"
            },
            "cycle": request.cycle,
            "request_id": request.id,
            "active_node": request.active_node,
            "mode": request.mode,
            "cycles_since_clean": request.cycles_since_clean,
            "no_sound_progress_window_cycles": request.no_sound_progress_window_cycles,
            "shallow_coarse_closed_count": request.shallow_coarse_closed_count,
            "cycles_since_shallow_coarse_closed_count_increase": request.cycles_since_shallow_coarse_closed_count_increase,
            "last_clean_rewind_count": request.last_clean_rewind_count,
            "retry_outcome_kind": request.retry_outcome_kind,
            "retry_attempt": request.retry_attempt,
            "blockers": request.blockers,
            "current_present_nodes": request.current_present_nodes,
            "current_proof_nodes": request.current_proof_nodes,
            "current_deps": request.current_deps,
            "current_target_claims": request.current_target_claims,
            // Coarse-DAG context for the GlobalRepairAuditor (out-of-cone
            // extension grants) and cone_clean decisions. Populated alongside
            // `current_deps`; empty/None/false on legacy runs and before the
            // theorem-stating → proof-formalization transition computes the
            // coarse set.
            "coarse_dag_nodes": request.coarse_dag_nodes,
            "active_coarse_node": request.active_coarse_node,
            "coarse_repair_mode": request.coarse_repair_mode,
            // Challenge registry + coverage, populated the same way the
            // coarse-DAG view was extended to StuckMathAudit: the audit
            // adjudicates routing over the live DAG and needs the
            // prescribed-target picture. Empty objects on paper-only runs.
            "configured_challenge_targets": challenge_registry_json(request),
            "challenge_coverage": challenge_coverage_json(request),
            "resettable_theorem_stating_nodes": request.resettable_theorem_stating_nodes,
            "latest_worker_rationale": {
                "summary": request.latest_worker_summary,
                "comments": request.latest_worker_comments,
                "needs_restructure_suggested_nodes": request.latest_worker_needs_restructure_suggested_nodes,
            },
            "reviewer_comments": request.reviewer_comments,
            "deterministic_worker_rejection_reasons": request.deterministic_worker_rejection_reasons,
        },
        "audit_latch": request.stuck_math_audit.clone(),
        "stuck_math_audit": request.stuck_math_audit.clone(),
        "need_input_audit": request.stuck_math_audit.need_input_audit.clone(),
        "previous_audit_plan_snapshot": request.previous_audit_plan_snapshot.clone(),
        "latest_stuck_math_audit_rejection_reason": request.latest_stuck_math_audit_rejection_reason,
        "cone_clean_contract": cone_clean_contract,
        "confirm_need_input_contract": confirm_need_input_contract,
        "artifact_contract": {
            "result_type": "stuck_math_audit_result_v1",
            "report_min_chars": crate::model::AUDIT_REPORT_TEXT_MIN_CHARS,
            "report_max_chars": crate::model::AUDIT_REPORT_TEXT_MAX_CHARS,
            "task_title_max_chars": crate::model::AUDIT_TASK_TITLE_MAX_CHARS,
            "task_body_max_chars": crate::model::AUDIT_TASK_BODY_MAX_CHARS,
            "plan_max_json_chars": crate::model::AUDIT_PLAN_MAX_JSON_CHARS,
            "prompt_schema_example": prompt_schema_example,
        },
        "artifact_prompt_view": artifact_prompt_view_with_commands(&[
            "python3",
            "{{check_script_path}}",
            "trellis-stuck-math-audit-result",
            "{{raw_output_path}}",
            "--context-json",
            "{{context_json_path}}",
        ], &[]),
    });
    // Decide polarity FACTS, gated with the flip authority
    // (`decide_flip_available`): the EFFECTIVE live polarity of every
    // configured `Decide` primary, `Prove`-default materialized. The
    // adjudicator of `set_live_polarity` reads the current polarity here
    // instead of inferring it from which side's node is live (both sides
    // can be live under `Prove` when the refutation was authored directly
    // into Tablet). Inserted only when the flip authority is offered so
    // every other contract stays byte-identical.
    if decide_flip_available {
        if let Some(map) = contract.as_object_mut() {
            map.insert(
                "decide_live_polarity".to_string(),
                decide_live_polarity_json(request),
            );
        }
    }
    // Reference-paper registry + live claims. Inserted only when a
    // registry is configured so every other contract stays byte-identical.
    if !request.configured_reference_papers.is_empty() {
        if let Some(map) = contract.as_object_mut() {
            map.insert(
                "reference_papers".to_string(),
                json!(request.configured_reference_papers),
            );
            map.insert(
                "node_reference_grounds".to_string(),
                json!(request.node_reference_grounds),
            );
        }
    }
    // Process memory (spec §5): pending worker/reviewer challenges this
    // audit must adjudicate. Inserted only when non-empty so the
    // no-challenge contract stays byte-identical.
    if !request.pending_memory_challenges.is_empty() {
        if let Some(map) = contract.as_object_mut() {
            map.insert(
                "pending_memory_challenges".to_string(),
                json!(request.pending_memory_challenges),
            );
        }
    }
    // Audit-ordered node retirement: surface the still-pending order and
    // the reviewer's most recent decline for this audit's adjudication.
    // Inserted only when present so every other contract stays
    // byte-identical.
    if let Some(pending) = request.pending_node_retirement.as_ref() {
        if let Some(map) = contract.as_object_mut() {
            map.insert("pending_node_retirement".to_string(), json!(pending));
        }
    }
    if let Some(decline) = request.latest_node_retirement_decline.as_ref() {
        if let Some(map) = contract.as_object_mut() {
            map.insert(
                "latest_node_retirement_decline".to_string(),
                json!(decline),
            );
        }
    }
    // global_repair_mode: when this audit adjudicates a Step A that
    // superseded a still-pending grant, surface the dropped grant so the
    // auditor sees what the new proposal replaces. Inserted only when
    // present so every other contract stays byte-identical.
    if let Some(grant) = request
        .pending_global_repair_request
        .as_ref()
        .and_then(|pending| pending.superseded_grant.as_ref())
    {
        if let Some(map) = contract.as_object_mut() {
            map.insert("superseded_global_repair_grant".to_string(), json!(grant));
        }
    }
    // Cleanup-v2 migration (2026-06-04): pre-compute the trimmed view the
    // bridge renders inline in the prompt. Mirrors what
    // `bridge_prompts._prompt_facing_stuck_math_audit_contract` used to do
    // (drop `prompt_fragments` + `artifact_prompt_view` housekeeping, then
    // drop any null Option<> fields). Bridge reads this verbatim; the
    // full contract still ships via `structured_request_path`.
    let mut view = contract.clone();
    if let Some(view_map) = view.as_object_mut() {
        view_map.remove("prompt_fragments");
        view_map.remove("artifact_prompt_view");
    }
    if let Some(map) = contract.as_object_mut() {
        map.insert("prompt_facing_view".to_string(), drop_null_keys(view));
    }
    contract
}

pub fn populate_request_prompt_contracts(request: &mut WrapperRequest, repo_path: Option<&Path>) {
    request.prompt_contract_version = prompt_contract_version();
    request.project_invariants = project_invariants_payload(request.is_pv);
    request.paper_contract = paper_contract_payload(request, repo_path);
    request.corr_contract = correspondence_contract_payload(request, repo_path);
    request.sound_contract = soundness_contract_payload(request, repo_path);
    request.worker_contract = worker_contract_payload(request, repo_path);
    request.review_contract = review_contract_payload(request, repo_path);
    request.audit_contract = audit_contract_payload(request, repo_path);
    request.stuck_math_audit_contract = stuck_math_audit_contract_payload(request);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn reference_registry() -> BTreeMap<RefPaperId, crate::model::ReferencePaperSpec> {
        BTreeMap::from([
            (
                RefPaperId::from("smith2020"),
                crate::model::ReferencePaperSpec {
                    tex_path: "paper/refs/smith2020.tex".into(),
                    source_id: "Smith 2020".into(),
                },
            ),
            (
                RefPaperId::from("unclaimed99"),
                crate::model::ReferencePaperSpec {
                    tex_path: "paper/refs/unclaimed99.tex".into(),
                    source_id: "Unclaimed 99".into(),
                },
            ),
        ])
    }

    const NEW_REFERENCE_FRAGMENTS: [&str; 4] = [
        "worker/common/19b_reference_papers.md",
        "review/common/27b_reference_papers.md",
        "stuck_math_audit/common/02c_reference_papers.md",
        "verifier/substantiveness/16_reference_grounds.md",
    ];

    #[test]
    fn live_disprove_worker_and_dedicated_corr_render_only_the_closed_artifact_carriers() {
        let target = crate::model::ChallengeTargetId::from("goal:generic");
        let primary = crate::model::ChallengeTargetSpec {
            kind: crate::model::ChallengeTargetKind::Theorem,
            name: "Generic".into(),
            lean: "theorem Generic : True := by".into(),
            informal: "generic GOAL target prose".into(),
            resolution: crate::model::ChallengeResolution::Decide,
            ..crate::model::ChallengeTargetSpec::default()
        };
        let mut worker = WrapperRequest {
            phase: Phase::ProofFormalization,
            work_kind: WorkerWorkKind::Standard,
            is_pv: true,
            trust_base_required_v1: true,
            active_node: Some(crate::model::NodeId::from(
                crate::model::refutation_node_name(&primary.name),
            )),
            configured_challenge_targets: BTreeMap::from([(target.clone(), primary)]),
            pv_live_polarity: BTreeMap::from([(
                target.clone(),
                crate::model::ChallengePolarity::Disprove,
            )]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut worker, None);
        let declaration = worker
            .worker_contract
            .pointer("/prompt_schema_example/rust_witness_artifact")
            .unwrap();
        assert_eq!(declaration["target_id"], json!(target));
        assert_eq!(
            declaration["relative_path"],
            json!(crate::trust_base::rust_witness_relative_path(&target))
        );
        assert!(worker.worker_contract["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fragment| {
                fragment.as_str()
                    == Some("pv/worker/common/24_rust_witness_artifact.md")
            }));

        let digest = crate::trust_base::raw_sha256(b"generic correspondence");
        let reviewed = crate::trust_base::RustWitnessReviewedDigests {
            target_statement_sha256: digest,
            negated_statement_sha256: digest,
            negative_closure_sha256: digest,
            artifact_sha256: digest,
            receipt_sha256: digest,
        };
        let artifact_request = crate::trust_base::RustWitnessCorrespondenceRequest {
            schema: crate::trust_base::RUST_WITNESS_CORRESPONDENCE_REQUEST_SCHEMA.into(),
            target_id: target,
            goal_target_prose_utf8: "generic GOAL target prose".into(),
            target_lean_utf8: "theorem Generic : True := by".into(),
            negated_target_lean_utf8: "theorem Generic__Refutation : ¬ True := by".into(),
            checked_negative_proof_closure: crate::model::LocalClosureRecord::default(),
            artifact_source_utf8: "#[test] fn witness() {}\n".into(),
            execution_receipt: json!({"receipt_sha256": digest}),
            reviewed_digests: reviewed,
            request_sha256: digest,
        };
        let corr = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            rust_witness_artifact_correspondence: Some(artifact_request.clone()),
            ..WrapperRequest::default()
        };
        let rendered = correspondence_contract_payload(&corr, None);
        assert_eq!(
            rendered.pointer("/request_summary/rust_witness_artifact_correspondence"),
            Some(&serde_json::to_value(&artifact_request).unwrap())
        );
        assert!(rendered["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fragment| {
                fragment.as_str()
                    == Some("pv/verifier/correspondence/10_rust_witness_artifact.md")
            }));
    }

    #[test]
    fn empty_registry_prompts_and_payloads_carry_no_reference_paper_surface() {
        // Byte-identity requirement: an empty-registry run's assembled
        // fragment lists and contract payloads contain NONE of the new
        // fragments/keys — the only wire delta vs the pre-feature binary
        // is prompt_contract_version itself.
        for (kind, phase) in [
            (crate::model::RequestKind::Worker, Phase::ProofFormalization),
            (crate::model::RequestKind::Review, Phase::ProofFormalization),
            (
                crate::model::RequestKind::StuckMathAudit,
                Phase::ProofFormalization,
            ),
            (crate::model::RequestKind::Paper, Phase::TheoremStating),
        ] {
            let mut request = WrapperRequest {
                kind,
                phase,
                substantiveness_verify_nodes: if kind == crate::model::RequestKind::Paper {
                    BTreeSet::from([NodeId::from("MainTheorem")])
                } else {
                    BTreeSet::new()
                },
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            let combined = serde_json::to_string(&serde_json::json!({
                "worker": request.worker_contract,
                "review": request.review_contract,
                "audit": request.stuck_math_audit_contract,
                "paper": request.paper_contract,
            }))
            .unwrap();
            for fragment in NEW_REFERENCE_FRAGMENTS {
                assert!(
                    !combined.contains(fragment),
                    "{kind:?}: empty-registry contracts must not push {fragment}"
                );
            }
            assert!(
                !combined.contains("reference_papers"),
                "{kind:?}: empty-registry contracts must not emit reference_papers"
            );
            assert!(
                !combined.contains("node_reference_grounds"),
                "{kind:?}: empty-registry contracts must not emit node_reference_grounds"
            );
        }
    }

    #[test]
    fn registry_run_worker_review_audit_carry_registry_and_fragments() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            configured_reference_papers: reference_registry(),
            node_reference_grounds: BTreeMap::from([(
                NodeId::from("MainTheorem"),
                BTreeSet::from([RefPaperId::from("smith2020")]),
            )]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker = &request.worker_contract;
        assert!(worker["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "worker/common/19b_reference_papers.md"));
        assert_eq!(
            worker["request_summary"]["reference_papers"]["smith2020"]["tex_path"],
            json!("paper/refs/smith2020.tex")
        );
        assert_eq!(
            worker["request_summary"]["node_reference_grounds"]["MainTheorem"],
            json!(["smith2020"])
        );
        assert_eq!(
            worker["prompt_schema_example"]["node_reference_grounds"],
            json!({"node_id": ["reference_paper_id"]})
        );
        assert!(worker["reported_delta_fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "node_reference_grounds"));

        request.kind = crate::model::RequestKind::Review;
        populate_request_prompt_contracts(&mut request, None);
        let review = &request.review_contract;
        assert!(review["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "review/common/27b_reference_papers.md"));
        assert_eq!(
            review["request_summary"]["reference_papers"]["smith2020"]["source_id"],
            json!("Smith 2020")
        );

        request.kind = crate::model::RequestKind::StuckMathAudit;
        populate_request_prompt_contracts(&mut request, None);
        let audit = &request.stuck_math_audit_contract;
        assert!(audit["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "stuck_math_audit/common/02c_reference_papers.md"));
        assert_eq!(
            audit["reference_papers"]["smith2020"]["source_id"],
            json!("Smith 2020")
        );
    }

    #[test]
    fn substantiveness_payload_restricts_registry_to_frontier_claims() {
        // Amendment G5: the per-node paper payload carries the
        // STATE-carried registry restricted to ids claimed by FRONTIER
        // nodes, plus the frontier claims; the fragment fires only when
        // a frontier node has claims.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            substantiveness_verify_nodes: BTreeSet::from([NodeId::from("MainTheorem")]),
            configured_reference_papers: reference_registry(),
            node_reference_grounds: BTreeMap::from([
                (
                    NodeId::from("MainTheorem"),
                    BTreeSet::from([RefPaperId::from("smith2020")]),
                ),
                // Off-frontier claim: restricted OUT of the payload.
                (
                    NodeId::from("OffFrontier"),
                    BTreeSet::from([RefPaperId::from("unclaimed99")]),
                ),
            ]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let payload = &request.paper_contract;
        assert!(payload["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "verifier/substantiveness/16_reference_grounds.md"));
        assert_eq!(
            payload["reference_papers"]["smith2020"]["tex_path"],
            json!("paper/refs/smith2020.tex")
        );
        assert!(
            payload["reference_papers"].get("unclaimed99").is_none(),
            "registry is restricted to frontier-claimed ids"
        );
        assert_eq!(
            payload["node_reference_grounds"],
            json!({"MainTheorem": ["smith2020"]})
        );
        // The prompt-facing view keeps the non-null keys too.
        assert_eq!(
            payload["prompt_facing_view"]["node_reference_grounds"],
            json!({"MainTheorem": ["smith2020"]})
        );

        // Registry configured but NO frontier claims: keys emit as Null
        // (the bridge's _drop_null_keys strips them; the view already
        // has), and the fragment stays out.
        let mut claim_free = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            substantiveness_verify_nodes: BTreeSet::from([NodeId::from("MainTheorem")]),
            configured_reference_papers: reference_registry(),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut claim_free, None);
        let payload = &claim_free.paper_contract;
        assert!(payload["reference_papers"].is_null());
        assert!(payload["node_reference_grounds"].is_null());
        assert!(payload["prompt_facing_view"]
            .get("reference_papers")
            .is_none());
        assert!(!payload["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "verifier/substantiveness/16_reference_grounds.md"));
    }

    #[test]
    fn substantiveness_payload_advertises_false_as_stated() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            is_pv: true,
            substantiveness_verify_nodes: BTreeSet::from([NodeId::from("GoalStmt")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        assert_eq!(
            request.paper_contract["artifact_contract"]["phase_blocks"]
                ["substantiveness"]["verdict_values"],
            json!(["Pass", "FalseAsStated", "Fail", "NotDoneYet"])
        );
        assert_eq!(
            request.paper_contract["artifact_contract"]["phase_blocks"]
                ["substantiveness"]["comment_required_on_false_as_stated"],
            json!(true)
        );
    }

    #[test]
    fn cleanup_audit_contract_includes_artifact_delivery_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Audit,
            phase: Phase::Cleanup,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let fragments = request.audit_contract["prompt_fragments"]
            .as_array()
            .expect("audit prompt fragments");
        let fragment_names: Vec<_> = fragments
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        let delivery_idx = fragment_names
            .iter()
            .position(|item| *item == "shared/90_artifact_delivery.md")
            .expect("artifact delivery fragment");
        let pointer_idx = fragment_names
            .iter()
            .position(|item| *item == STRUCTURED_REQUEST_POINTER_FRAGMENT)
            .expect("structured request pointer fragment");
        assert!(delivery_idx < pointer_idx);

        let command = request.audit_contract["artifact_prompt_view"]["json_check_command_template"]
            .as_array()
            .expect("audit json check command");
        assert!(command
            .iter()
            .any(|item| item.as_str() == Some("trellis-audit-result")));
    }

    fn write_dedup_node(repo: &Path, node: &str, step_count: usize) {
        let body = (0..step_count)
            .map(|index| format!("  exact Step{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(
            repo.join("Tablet").join(format!("{node}.lean")),
            format!(
                "import Tablet.Preamble\n\n-- [TABLET NODE: {node}]\ntheorem {node} : True := by\n-- BODY\n{body}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn shared_proof_blocks_are_deterministic_and_group_n_nodes_once() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        for node in ["A", "B", "C"] {
            write_dedup_node(repo, node, 60);
        }

        let first = shared_proof_block_view(Some(repo));
        let second = shared_proof_block_view(Some(repo));
        assert_eq!(first, second, "the request-build view must be byte-stable");
        assert_eq!(first["regions_total"], json!(1));
        let regions = first["regions"].as_array().unwrap();
        assert_eq!(regions.len(), 1, "one N=3 group, never three pairs");
        assert_eq!(regions[0]["n_nodes"], json!(3));
        assert_eq!(regions[0]["block_lines"], json!(60));
        assert_eq!(regions[0]["recoverable"], json!(120));
        let members = regions[0]["members"].as_array().unwrap();
        assert_eq!(members.len(), 3);
    }

    #[test]
    fn shared_proof_blocks_fold_nested_node_sets_into_one_region() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write_dedup_node(repo, "A", 80);
        write_dedup_node(repo, "B", 80);
        write_dedup_node(repo, "C", 60);

        let view = shared_proof_block_view(Some(repo));
        assert_eq!(view["regions_total"], json!(1));
        let region = &view["regions"][0];
        assert_eq!(region["n_nodes"], json!(3));
        assert_eq!(region["block_lines"], json!(60));
        let alternatives = region["nested_alternatives"].as_array().unwrap();
        assert_eq!(alternatives.len(), 1);
        assert_eq!(alternatives[0]["n_nodes"], json!(2));
        assert_eq!(alternatives[0]["block_lines"], json!(69));
    }

    #[test]
    fn stuck_math_audit_contract_emits_dedicated_fragments() {
        let mut request = WrapperRequest {
            id: 2,
            kind: crate::model::RequestKind::StuckMathAudit,
            cycle: 1,
            phase: Phase::ProofFormalization,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test trigger".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            // Non-empty so the cone_clean fragment + contract are
            // emitted (gated on the set being non-empty rather than
            // hardcoding phase).
            resettable_theorem_stating_nodes: BTreeSet::from([NodeId::from("n")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let fragments = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .expect("prompt fragments");
        let fragment_names: Vec<_> = fragments
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        assert!(fragment_names.contains(&"stuck_math_audit/common/01_role.md"));
        assert!(fragment_names.contains(&"stuck_math_audit/common/02_reference_paper.md"));
        assert!(fragment_names.contains(&"stuck_math_audit/common/04b_cone_clean.md"));
        assert!(fragment_names.contains(&"stuck_math_audit/common/05_output_contract.md"));
        assert!(
            !fragment_names.contains(&"stuck_math_audit/common/05_need_input_output_contract.md")
        );
        let reference_idx = fragment_names
            .iter()
            .position(|item| *item == "stuck_math_audit/common/02_reference_paper.md")
            .expect("reference paper fragment");
        let context_idx = fragment_names
            .iter()
            .position(|item| *item == "stuck_math_audit/common/02_request_context.md")
            .expect("request context fragment");
        let cone_clean_idx = fragment_names
            .iter()
            .position(|item| *item == "stuck_math_audit/common/04b_cone_clean.md")
            .expect("cone clean fragment");
        let output_idx = fragment_names
            .iter()
            .position(|item| *item == "stuck_math_audit/common/05_output_contract.md")
            .expect("output contract fragment");
        assert!(reference_idx < context_idx);
        assert!(cone_clean_idx < output_idx);
        assert_eq!(
            request.stuck_math_audit_contract["artifact_contract"]["result_type"],
            json!("stuck_math_audit_result_v1")
        );
        assert_eq!(
            request.stuck_math_audit_contract["burst_role"],
            json!("stuck_math_audit")
        );
        assert!(request.stuck_math_audit_contract["confirm_need_input_contract"].is_null());
        assert!(
            request.stuck_math_audit_contract["artifact_contract"]["prompt_schema_example"]
                .as_object()
                .expect("schema object")
                .get("confirm_need_input")
                .is_none()
        );
    }

    /// A `Decide`-resolution Theorem primary spec plus its auto-seeded
    /// refutation pair, mirroring the registry shape the runtime seeds.
    fn decide_pair_specs(
        primary_id: &str,
        name: &str,
    ) -> Vec<(crate::model::ChallengeTargetId, crate::model::ChallengeTargetSpec)> {
        let refutation_id = crate::model::refutation_target_id(
            &crate::model::ChallengeTargetId::from(primary_id),
        );
        vec![
            (
                crate::model::ChallengeTargetId::from(primary_id),
                crate::model::ChallengeTargetSpec {
                    kind: crate::model::ChallengeTargetKind::Theorem,
                    name: name.to_string(),
                    lean: format!("theorem {name} : True :="),
                    resolution: crate::model::ChallengeResolution::Decide,
                    ..crate::model::ChallengeTargetSpec::default()
                },
            ),
            (
                refutation_id,
                crate::model::ChallengeTargetSpec {
                    kind: crate::model::ChallengeTargetKind::Theorem,
                    name: crate::model::refutation_node_name(name),
                    lean: format!(
                        "theorem {} : ¬ True :=",
                        crate::model::refutation_node_name(name)
                    ),
                    ..crate::model::ChallengeTargetSpec::default()
                },
            ),
        ]
    }

    #[test]
    fn stuck_math_audit_contract_surfaces_effective_decide_live_polarity() {
        // Two Decide pairs: `goal:flipped` was flipped to Disprove by a prior
        // audit; `goal:defaulted` has NO `pv_live_polarity` entry (the Prove
        // default). The `goal:defaulted` pair also has its REFUTATION node
        // live and claiming the refutation target — the
        // `compute_float_correct_or_defer` shape (refutation authored
        // directly into Tablet under Prove polarity). The contract must say
        // "prove" for it: polarity is a surfaced FACT, never inferred from
        // node liveness.
        let mut targets = BTreeMap::new();
        for (id, spec) in decide_pair_specs("goal:defaulted", "ComputeFloatCorrectOrDefer")
            .into_iter()
            .chain(decide_pair_specs("goal:flipped", "OtherContract"))
        {
            targets.insert(id, spec);
        }
        let mut request = WrapperRequest {
            id: 3,
            kind: crate::model::RequestKind::StuckMathAudit,
            cycle: 2,
            phase: Phase::ProofFormalization,
            is_pv: true,
            configured_challenge_targets: targets,
            challenge_targets_configured: true,
            pv_live_polarity: BTreeMap::from([(
                crate::model::ChallengeTargetId::from("goal:flipped"),
                crate::model::ChallengePolarity::Disprove,
            )]),
            current_present_nodes: BTreeSet::from([
                NodeId::from("ComputeFloatCorrectOrDefer"),
                NodeId::from("ComputeFloatCorrectOrDefer__Refutation"),
            ]),
            current_challenge_claims: BTreeMap::from([
                (
                    NodeId::from("ComputeFloatCorrectOrDefer"),
                    BTreeSet::from([crate::model::ChallengeTargetId::from("goal:defaulted")]),
                ),
                (
                    NodeId::from("ComputeFloatCorrectOrDefer__Refutation"),
                    BTreeSet::from([crate::model::refutation_target_id(
                        &crate::model::ChallengeTargetId::from("goal:defaulted"),
                    )]),
                ),
            ]),
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test trigger".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let contract = &request.stuck_math_audit_contract;
        // EVERY configured Decide PRIMARY appears with its EFFECTIVE
        // polarity (absent map entry materialized as "prove"); refutation
        // target ids are never keys.
        assert_eq!(
            contract["decide_live_polarity"],
            json!({"goal:defaulted": "prove", "goal:flipped": "disprove"})
        );
        // The facts ride the prompt-facing view the auditor actually reads.
        assert_eq!(
            contract["prompt_facing_view"]["decide_live_polarity"],
            json!({"goal:defaulted": "prove", "goal:flipped": "disprove"})
        );
        // Gated with the same flip authority that advertises the response
        // fields + fragment.
        let schema = contract["artifact_contract"]["prompt_schema_example"]
            .as_object()
            .expect("schema object");
        assert!(schema.contains_key("set_live_polarity"));
        assert!(schema.contains_key("set_live_polarity_target"));
        assert!(contract["prompt_fragments"]
            .as_array()
            .expect("prompt fragments")
            .iter()
            .any(|f| f == "pv/stuck_audit/06_decide_polarity_flip.md"));
    }

    /// W6 (D1): the flip authority reaches PROSE audit contracts — but
    /// only once a decide pair is ACTIONABLE (its authored statement has
    /// bound). Unbound authored pairs advertise nothing (a proposal would
    /// only earn a `statement_unbound` bounce); math contracts stay
    /// byte-identical.
    #[test]
    fn prose_stuck_audit_advertises_flip_only_when_statement_bound() {
        let build = |bound: bool, is_pv: bool| {
            let mut targets = BTreeMap::new();
            targets.insert(
                crate::model::ChallengeTargetId::from("goal:f"),
                crate::model::ChallengeTargetSpec {
                    kind: crate::model::ChallengeTargetKind::Theorem,
                    resolution: crate::model::ChallengeResolution::Decide,
                    statement_provenance:
                        crate::model::StatementProvenance::WorkerAuthored,
                    name: if bound { "GoalStmt".into() } else { String::new() },
                    lean: if bound {
                        "theorem GoalStmt (x : Nat) : x + 0 = x := by".into()
                    } else {
                        String::new()
                    },
                    ..crate::model::ChallengeTargetSpec::default()
                },
            );
            targets.insert(
                crate::model::refutation_target_id(&crate::model::ChallengeTargetId::from(
                    "goal:f",
                )),
                crate::model::ChallengeTargetSpec {
                    kind: crate::model::ChallengeTargetKind::Theorem,
                    statement_provenance:
                        crate::model::StatementProvenance::KernelDerived,
                    ..crate::model::ChallengeTargetSpec::default()
                },
            );
            let mut request = WrapperRequest {
                id: 3,
                kind: crate::model::RequestKind::StuckMathAudit,
                cycle: 2,
                phase: Phase::ProofFormalization,
                is_pv,
                configured_challenge_targets: targets,
                challenge_targets_configured: true,
                stuck_math_audit: crate::model::StuckMathAuditState {
                    active: true,
                    trigger: "test trigger".into(),
                    ..crate::model::StuckMathAuditState::default()
                },
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            request
        };

        // Bound prose pair: flip authority advertised.
        let request = build(true, true);
        let contract = &request.stuck_math_audit_contract;
        let schema = contract["artifact_contract"]["prompt_schema_example"]
            .as_object()
            .expect("schema object");
        assert!(
            schema.contains_key("set_live_polarity"),
            "a bound prose decide pair advertises the flip authority"
        );
        assert!(contract["prompt_fragments"]
            .as_array()
            .expect("prompt fragments")
            .iter()
            .any(|f| f == "pv/stuck_audit/06_decide_polarity_flip.md"));

        // Unbound prose pair: nothing advertised.
        let request = build(false, true);
        let contract = &request.stuck_math_audit_contract;
        let schema = contract["artifact_contract"]["prompt_schema_example"]
            .as_object()
            .expect("schema object");
        assert!(
            !schema.contains_key("set_live_polarity"),
            "an UNBOUND authored pair advertises no flip authority"
        );
        assert!(!contract["prompt_fragments"]
            .as_array()
            .expect("prompt fragments")
            .iter()
            .any(|f| f == "pv/stuck_audit/06_decide_polarity_flip.md"));

        // Non-PV: byte-identically absent.
        let request = build(true, false);
        let schema = request.stuck_math_audit_contract["artifact_contract"]
            ["prompt_schema_example"]
            .as_object()
            .expect("schema object");
        assert!(!schema.contains_key("set_live_polarity"));
    }

    /// Stage 5 (plan doc 32, Codex 9): the give-up surface is advertised —
    /// schema field + fragment — on trust-required PV audit requests with a
    /// Decide target, and ONLY there (every other contract, math included,
    /// stays byte-identical).
    #[test]
    fn stuck_math_audit_contract_omits_decide_live_polarity_without_decide_target() {
        // All-Prove registry (no Decide primary): the key must be ABSENT —
        // not an empty object — so non-Decide contracts stay byte-identical.
        let mut request = WrapperRequest {
            id: 3,
            kind: crate::model::RequestKind::StuckMathAudit,
            cycle: 2,
            phase: Phase::ProofFormalization,
            is_pv: true,
            configured_challenge_targets: BTreeMap::from([(
                crate::model::ChallengeTargetId::from("goal:plain"),
                crate::model::ChallengeTargetSpec {
                    kind: crate::model::ChallengeTargetKind::Theorem,
                    name: "PlainGoal".to_string(),
                    lean: "theorem PlainGoal : True :=".to_string(),
                    ..crate::model::ChallengeTargetSpec::default()
                },
            )]),
            challenge_targets_configured: true,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test trigger".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let contract = request
            .stuck_math_audit_contract
            .as_object()
            .expect("contract object");
        assert!(!contract.contains_key("decide_live_polarity"));
        assert!(!contract["prompt_facing_view"]
            .as_object()
            .expect("prompt facing view")
            .contains_key("decide_live_polarity"));
        assert!(!contract["artifact_contract"]["prompt_schema_example"]
            .as_object()
            .expect("schema object")
            .contains_key("set_live_polarity"));
    }

    #[test]
    fn revision_planning_lane_selects_revision_planner_contract() {
        let mut request = WrapperRequest {
            id: 5,
            kind: crate::model::RequestKind::StuckMathAudit,
            cycle: 1,
            phase: Phase::RevisionStating,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "revision planning".into(),
                revision_planning: Some(crate::model::RevisionPlanningContext {
                    old_paper_path: "paper/revision/old.tex".into(),
                    new_paper_path: "paper/revision/new.tex".into(),
                    frozen_nodes: BTreeSet::from([NodeId::from("Aux")]),
                    editable_nodes: BTreeSet::from([NodeId::from("MainTheorem")]),
                    ..crate::model::RevisionPlanningContext::default()
                }),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let contract = &request.stuck_math_audit_contract;
        assert_eq!(contract["burst_role"], json!("revision_planner"));
        assert_eq!(
            contract["request_summary"]["scenario"],
            json!("revision_planning")
        );
        // The read-only planner packet is on the wire verbatim.
        assert_eq!(
            contract["revision_planning"]["new_paper_path"],
            json!("paper/revision/new.tex")
        );
        assert_eq!(
            contract["revision_planning"]["frozen_nodes"],
            json!(["Aux"])
        );
        // The revision branch does NOT emit the ordinary / need-input role
        // fragments or the cone_clean / confirm_need_input contracts.
        assert!(contract["confirm_need_input_contract"].is_null());
        assert_eq!(
            contract["artifact_contract"]["result_type"],
            json!("stuck_math_audit_result_v1")
        );
        // Default kind (PaperRevision, the legacy value) selects the
        // two-paper revision planner role; the output contract rides along
        // regardless of kind.
        let fragments = contract["prompt_fragments"]
            .as_array()
            .expect("prompt_fragments array");
        assert!(fragments
            .iter()
            .any(|f| f == "stuck_math_audit/common/01_revision_planner_role.md"));
        assert!(!fragments
            .iter()
            .any(|f| f == "stuck_math_audit/common/01b_target_addition_planner_role.md"));
        assert!(fragments
            .iter()
            .any(|f| f == "stuck_math_audit/common/05_revision_plan_output_contract.md"));
    }

    /// `RevisionKind` keys the planner role fragment: `TargetAddition`
    /// (add-targets revival, same unchanged paper) selects the
    /// target-addition role instead of the two-paper revision role; the
    /// rest of the fragment list — in particular the output contract — is
    /// identical across kinds.
    #[test]
    fn target_addition_kind_selects_target_addition_planner_role() {
        let make_request = |kind: crate::model::RevisionKind| {
            let mut request = WrapperRequest {
                id: 5,
                kind: crate::model::RequestKind::StuckMathAudit,
                cycle: 1,
                phase: Phase::RevisionStating,
                stuck_math_audit: crate::model::StuckMathAuditState {
                    active: true,
                    trigger: "revision planning".into(),
                    revision_planning: Some(crate::model::RevisionPlanningContext {
                        revision_kind: kind,
                        old_paper_path: "paper/paper.tex".into(),
                        new_paper_path: "paper/paper.tex".into(),
                        ..crate::model::RevisionPlanningContext::default()
                    }),
                    ..crate::model::StuckMathAuditState::default()
                },
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            request
        };

        let fragments_for = |request: &WrapperRequest| -> Vec<String> {
            request.stuck_math_audit_contract["prompt_fragments"]
                .as_array()
                .expect("prompt_fragments array")
                .iter()
                .map(|f| f.as_str().expect("fragment string").to_string())
                .collect()
        };

        let addition = make_request(crate::model::RevisionKind::TargetAddition);
        let addition_fragments = fragments_for(&addition);
        assert!(addition_fragments
            .iter()
            .any(|f| f == "stuck_math_audit/common/01b_target_addition_planner_role.md"));
        assert!(!addition_fragments
            .iter()
            .any(|f| f == "stuck_math_audit/common/01_revision_planner_role.md"));
        assert_eq!(addition.stuck_math_audit_contract["burst_role"], json!("revision_planner"));

        let revision = make_request(crate::model::RevisionKind::PaperRevision);
        let revision_fragments = fragments_for(&revision);
        assert!(revision_fragments
            .iter()
            .any(|f| f == "stuck_math_audit/common/01_revision_planner_role.md"));
        assert!(!revision_fragments
            .iter()
            .any(|f| f == "stuck_math_audit/common/01b_target_addition_planner_role.md"));

        // Only the role fragment differs between the kinds.
        let diff: Vec<&String> = addition_fragments
            .iter()
            .filter(|f| !revision_fragments.contains(f))
            .collect();
        assert_eq!(
            diff,
            vec!["stuck_math_audit/common/01b_target_addition_planner_role.md"]
        );
        assert_eq!(addition_fragments.len(), revision_fragments.len());

        // Both role fragments exist on disk.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../trellis/prompt_fragments");
        for path in [
            "stuck_math_audit/common/01_revision_planner_role.md",
            "stuck_math_audit/common/01b_target_addition_planner_role.md",
        ] {
            assert!(
                base.join(path).exists(),
                "fragment {path} must exist on disk"
            );
        }
    }

    #[test]
    fn need_input_auditor_contract_uses_dedicated_role_on_same_lane() {
        let mut request = WrapperRequest {
            id: 3,
            kind: crate::model::RequestKind::StuckMathAudit,
            cycle: 8,
            phase: Phase::TheoremStating,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "reviewer requested NeedInput".into(),
                need_input_audit: Some(crate::model::NeedInputAuditContext {
                    phase: Phase::TheoremStating,
                    reviewer_reason: "suspected paper gap".into(),
                    reviewer_comments: "reviewer escalation".into(),
                    review_request_id: 2,
                    review_cycle: 8,
                    ..crate::model::NeedInputAuditContext::default()
                }),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let fragments = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .expect("prompt fragments");
        let fragment_names: Vec<_> = fragments
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        assert_eq!(
            fragment_names[0],
            "stuck_math_audit/common/01_need_input_auditor_role.md"
        );
        assert!(
            fragment_names.contains(&"stuck_math_audit/common/05_need_input_output_contract.md")
        );
        assert!(!fragment_names.contains(&"stuck_math_audit/common/04b_cone_clean.md"));
        assert!(!fragment_names.contains(&"stuck_math_audit/common/05_output_contract.md"));
        assert_eq!(
            request.stuck_math_audit_contract["burst_role"],
            json!("need_input_auditor")
        );
        assert_eq!(
            request.stuck_math_audit_contract["request_summary"]["scenario"],
            json!("need_input_auditor")
        );
        assert_eq!(
            request.stuck_math_audit_contract["artifact_contract"]["prompt_schema_example"]
                ["confirm_need_input"],
            json!(false)
        );
        assert_eq!(
            request.stuck_math_audit_contract["confirm_need_input_contract"]
                ["false_requires_tasks"],
            json!(true)
        );
        assert!(request.stuck_math_audit_contract["cone_clean_contract"].is_null());
    }

    #[test]
    fn cleanup_worker_contract_restricts_outcomes() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        assert_eq!(
            request.worker_contract["allowed_outcomes"],
            json!(["valid", "invalid"])
        );
        assert_eq!(
            request.worker_contract["stuck_contract"]["allowed"],
            json!(false)
        );
        assert_eq!(
            request.worker_contract["needs_restructure_contract"]["allowed"],
            json!(false)
        );
        assert!(request.worker_contract["prompt_fragments"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|value| value == "worker/final_cleanup/05_task.md"))));
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/cleanup/37_field_guidance.md"));
        assert!(fragments
            .iter()
            .any(|item| item == "worker/cleanup/35_reviewer_comments.md"));
        assert!(fragments
            .iter()
            .any(|item| item == "worker/cleanup/45_outcomes.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/35_reviewer_comments.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/37_field_guidance.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/45_outcomes.md"));
    }

    #[test]
    fn dead_code_elim_worker_selects_its_task_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                cleanup_active_task_kind_view: Some(
                    crate::model::CleanupTaskKind::DeadCodeElim {
                        hint: "first have-chain".into(),
                    },
                ),
                cleanup_active_target_node_view: Some(NodeId::from("LongProof")),
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/final_cleanup/06_dead_code_task.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/final_cleanup/06_lintfix_task.md"));
    }

    #[test]
    fn extract_shared_worker_selects_its_task_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                cleanup_active_task_kind_view: Some(
                    crate::model::CleanupTaskKind::ExtractShared {
                        co_parents: BTreeSet::from([
                            NodeId::from("ParentB"),
                            NodeId::from("ParentC"),
                        ]),
                        ordinal: 1,
                        hint: "region".into(),
                    },
                ),
                cleanup_active_target_node_view: Some(NodeId::from("ParentA")),
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/final_cleanup/06_extract_shared_task.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/final_cleanup/06_extract_helper_task.md"));
    }

    #[test]
    fn proof_worker_contract_advertises_audit_request() {
        // Fix 5/6: a ProofFormalization worker (a phase that admits a
        // StuckMathAudit) must both see the `46_audit_request.md` fragment
        // and have `audit_request` in its prompt_schema_example, so the
        // advertised field set matches what the fragment tells it to emit.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            active_node: Some(NodeId::from("a")),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments
                .iter()
                .any(|item| item == "worker/common/46_audit_request.md"),
            "proof worker must see the audit_request fragment"
        );
        assert!(
            request.worker_contract["prompt_schema_example"]
                .get("audit_request")
                .is_some(),
            "proof worker schema must advertise audit_request"
        );
    }

    #[test]
    fn cleanup_worker_contract_omits_audit_request() {
        // Fix 6: Cleanup does NOT admit a StuckMathAudit
        // (`record_latest_worker_rationale` drops the request), so neither
        // the fragment nor the schema field is shown — the cleanup worker
        // uses `cleanup_request_reaudit` instead.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            !fragments
                .iter()
                .any(|item| item == "worker/common/46_audit_request.md"),
            "cleanup worker must NOT see the audit_request fragment"
        );
        assert!(
            request.worker_contract["prompt_schema_example"]
                .get("audit_request")
                .is_none(),
            "cleanup worker schema must NOT advertise audit_request"
        );
    }

    #[test]
    fn paper_contract_includes_covering_nodes_for_each_target() {
        let request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            paper_verify_targets: BTreeSet::from([
                TargetId::from("target_a"),
                TargetId::from("target_b"),
            ]),
            current_present_nodes: BTreeSet::from([
                NodeId::from("CoverA"),
                NodeId::from("CoverB"),
                NodeId::from("Hidden"),
            ]),
            current_target_claims: BTreeMap::from([
                (
                    NodeId::from("CoverA"),
                    BTreeSet::from([TargetId::from("target_a")]),
                ),
                (
                    NodeId::from("CoverB"),
                    BTreeSet::from([TargetId::from("target_a"), TargetId::from("target_b")]),
                ),
                (
                    NodeId::from("Missing"),
                    BTreeSet::from([TargetId::from("target_b")]),
                ),
            ]),
            ..WrapperRequest::default()
        };

        let contract = paper_contract_payload(&request, None);
        assert_eq!(
            contract["target_covering_nodes"],
            json!({
                "target_a": ["CoverA", "CoverB"],
                "target_b": ["CoverB"],
            })
        );
    }

    #[test]
    fn substantiveness_payload_surfaces_rejected_deviation_claims() {
        // A node on the substantiveness frontier claims a deviation the
        // Deviation lane rejected. The per-node substantiveness payload must
        // surface it under `rejected_deviations` so the verifier reads the
        // claim as an EXPECTED Fail (route to worker) rather than a
        // claimed-but-unauthorized system inconsistency. The contract must
        // be self-consistent: the rejected id appears under
        // `rejected_deviations` and `node_deviation_claims`, and NOT under
        // `authorized_deviations`.
        let rejected_id = crate::model::DeviationId::from("dev:flat");
        let request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::ProofFormalization,
            substantiveness_verify_nodes: BTreeSet::from([NodeId::from("N")]),
            node_deviation_claims: BTreeMap::from([(
                NodeId::from("N"),
                BTreeSet::from([rejected_id.clone()]),
            )]),
            authorized_deviations: BTreeMap::new(),
            rejected_deviations: BTreeMap::from([(
                rejected_id.clone(),
                "reference/flat.tex".to_string(),
            )]),
            ..WrapperRequest::default()
        };

        let contract = paper_contract_payload(&request, None);
        assert_eq!(
            contract["request_summary"]["scenario"],
            json!("substantiveness")
        );
        assert_eq!(
            contract["rejected_deviations"],
            json!({ "dev:flat": "reference/flat.tex" }),
            "substantiveness payload must surface the rejected deviation"
        );
        // Self-consistency: the rejected id is claimed, not authorized.
        assert_eq!(contract["authorized_deviations"], json!({}));
        assert_eq!(
            contract["node_deviation_claims"],
            json!({ "N": ["dev:flat"] })
        );
    }

    #[test]
    fn worker_prompt_uses_brief_scheme_when_context_is_resumed() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.worker_contract["prompt_fragments"][0],
            json!("common/00_trellis_scheme_brief.md")
        );
    }

    #[test]
    fn worker_prompt_uses_full_scheme_when_context_is_fresh() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: true,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.worker_contract["prompt_fragments"][0],
            json!("common/TRELLIS_FORMALIZATION_SCHEME.md")
        );
    }

    #[test]
    fn review_prompt_uses_brief_scheme_when_context_is_resumed() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            fresh_context: false,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.review_contract["prompt_fragments"][0],
            json!("common/00_trellis_scheme_brief.md")
        );
    }

    #[test]
    fn verifier_prompts_use_verifier_scheme() {
        // B6: verifiers always get the trim verifier-only scheme that
        // omits reviewer-only mode-machinery sections.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::TheoremStating,
            fresh_context: false,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.corr_contract["prompt_fragments"][0],
            json!("common/TRELLIS_FORMALIZATION_SCHEME_verifier.md")
        );
    }

    #[test]
    fn worker_prompt_omits_verifier_evidence_fragment_when_none_exists() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/34_verifier_evidence.md"));
    }

    #[test]
    fn worker_prompt_includes_verifier_evidence_fragment_when_present() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            review_verifier_evidence: crate::model::ReviewVerifierEvidence {
                corr: std::collections::BTreeMap::from([(
                    crate::model::NodeId::from("node_1"),
                    std::collections::BTreeMap::from([(
                        "lane_1".to_string(),
                        crate::model::CorrReviewerLaneEvidence {
                            node: crate::model::NodeId::from("node_1"),
                            correspondence: crate::model::CorrReviewerPhaseEvidence {
                                decision: "FAIL".to_string(),
                                issues: vec![],
                            },
                            overall: "REJECT".to_string(),
                            summary: "mismatch".to_string(),
                            comments: "fix it".to_string(),
                        },
                    )]),
                )]),
                ..crate::model::ReviewVerifierEvidence::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/34_verifier_evidence.md"));
    }

    #[test]
    fn worker_prompt_omits_deterministic_rejection_fragment_when_none_exists() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/32_deterministic_worker_rejection.md"));
    }

    #[test]
    fn worker_prompt_includes_deterministic_rejection_fragment_when_present() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            retry_outcome_kind: crate::model::RetryOutcomeKind::Invalid,
            deterministic_worker_rejection_reasons: vec![
                "Tablet/SubcriticalExpectation.lean has an application type mismatch".to_string(),
            ],
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/32_deterministic_worker_rejection.md"));
    }

    #[test]
    fn worker_prompt_always_includes_scratchpad_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/31_scratchpad.md"));
    }

    #[test]
    fn worker_prompt_includes_last_invalid_fragment_on_invalid_retry() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            invalid_attempt: true,
            retry_outcome_kind: crate::model::RetryOutcomeKind::Invalid,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/31_last_invalid.md"));
    }

    #[test]
    fn worker_prompt_includes_last_invalid_fragment_on_stuck_retry() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            invalid_attempt: false,
            retry_outcome_kind: crate::model::RetryOutcomeKind::Stuck,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/31_last_invalid.md"));
    }

    #[test]
    fn worker_prompt_includes_last_invalid_fragment_on_needs_restructure_retry() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            invalid_attempt: false,
            retry_outcome_kind: crate::model::RetryOutcomeKind::NeedsRestructure,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/31_last_invalid.md"));
    }

    #[test]
    fn worker_prompt_omits_last_invalid_fragment_when_no_retry_context() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            invalid_attempt: false,
            retry_outcome_kind: crate::model::RetryOutcomeKind::None,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/31_last_invalid.md"));
    }

    #[test]
    fn theorem_worker_prompt_includes_first_request_dag_fragment_for_request_one() {
        let mut request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments.iter().any(|item| {
            item == "worker/theorem_stating/12_first_request_dag_decomposition.md"
        }));
    }

    #[test]
    fn proof_worker_prompt_includes_soundness_review_fragment_when_soundness_blocker_present() {
        let soundness_blocker = crate::model::Blocker {
            kind: BlockerKind::Soundness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([soundness_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments
                .iter()
                .any(|item| { item == "worker/proof_formalization/05_after_soundness_review.md" }),
            "proof worker prompt with Soundness blocker must include the \
             after-soundness-review fragment; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_omits_soundness_review_fragment_when_no_soundness_blocker() {
        let corr_blocker = crate::model::Blocker {
            kind: BlockerKind::NodeCorr,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([corr_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            !fragments
                .iter()
                .any(|item| { item == "worker/proof_formalization/05_after_soundness_review.md" }),
            "proof worker prompt without a Soundness blocker must omit the \
             after-soundness-review fragment; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_includes_both_substantiveness_and_soundness_fragments_when_both_blockers_present(
    ) {
        let soundness_blocker = crate::model::Blocker {
            kind: BlockerKind::Soundness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let substantiveness_blocker = crate::model::Blocker {
            kind: BlockerKind::Substantiveness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("B"),
            },
            fingerprint: "fp-b".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([soundness_blocker, substantiveness_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments.iter().any(|item| {
                item == "worker/theorem_stating/05_after_substantiveness_review.md"
            }),
            "expected substantiveness fragment present; got: {:?}",
            fragments
        );
        assert!(
            fragments
                .iter()
                .any(|item| { item == "worker/proof_formalization/05_after_soundness_review.md" }),
            "expected soundness fragment present; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_includes_corr_review_fragment_when_nodecorr_blocker_present() {
        let corr_blocker = crate::model::Blocker {
            kind: BlockerKind::NodeCorr,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([corr_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments.iter().any(|item| {
                item == "worker/proof_formalization/05_after_correspondence_review.md"
            }),
            "proof worker prompt with NodeCorr blocker must include the \
             after-correspondence-review fragment; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_omits_corr_review_fragment_when_no_nodecorr_blocker() {
        let soundness_blocker = crate::model::Blocker {
            kind: BlockerKind::Soundness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([soundness_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            !fragments.iter().any(|item| {
                item == "worker/proof_formalization/05_after_correspondence_review.md"
            }),
            "proof worker prompt without a NodeCorr blocker must omit the \
             after-correspondence-review fragment; got: {:?}",
            fragments
        );
    }

    #[test]
    fn proof_worker_prompt_includes_all_three_scenario_fragments_when_all_blockers_present() {
        let soundness_blocker = crate::model::Blocker {
            kind: BlockerKind::Soundness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("A"),
            },
            fingerprint: "fp-a".to_string(),
            deferred: false,
        };
        let substantiveness_blocker = crate::model::Blocker {
            kind: BlockerKind::Substantiveness,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("B"),
            },
            fingerprint: "fp-b".to_string(),
            deferred: false,
        };
        let corr_blocker = crate::model::Blocker {
            kind: BlockerKind::NodeCorr,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("C"),
            },
            fingerprint: "fp-c".to_string(),
            deferred: false,
        };
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                ..crate::model::WorkerContext::default()
            },
            blockers: BTreeSet::from([soundness_blocker, substantiveness_blocker, corr_blocker]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        for expected in [
            "worker/theorem_stating/05_after_substantiveness_review.md",
            "worker/proof_formalization/05_after_correspondence_review.md",
            "worker/proof_formalization/05_after_soundness_review.md",
        ] {
            assert!(
                fragments.iter().any(|item| item == expected),
                "expected fragment {} present; got: {:?}",
                expected,
                fragments
            );
        }
    }

    #[test]
    fn theorem_worker_prompt_omits_first_request_dag_fragment_after_request_one() {
        let mut request = WrapperRequest {
            id: 2,
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments.iter().any(|item| {
            item == "worker/theorem_stating/12_first_request_dag_decomposition.md"
        }));
    }

    #[test]
    fn worker_prompt_includes_post_initial_sketch_policy_after_cycle_one() {
        let mut request = WrapperRequest {
            id: 2,
            cycle: 2,
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| { item == "worker/common/39_post_initial_sketch_policy.md" }));
    }

    #[test]
    fn worker_prompt_omits_post_initial_sketch_policy_on_cycle_one() {
        let mut request = WrapperRequest {
            id: 1,
            cycle: 1,
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| { item == "worker/common/39_post_initial_sketch_policy.md" }));
    }

    #[test]
    fn theorem_review_prompt_omits_proof_restructure_strategy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/36_difficulty_strategy.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/37_restructure_strategy.md"));
    }

    #[test]
    fn review_contract_includes_latest_worker_rationale_in_request_summary() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            latest_worker_summary: "worker built a broad first DAG".to_string(),
            latest_worker_comments: "critical window branch still feels shaky".to_string(),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.review_contract["request_summary"]["latest_worker_rationale"]["summary"],
            serde_json::json!("worker built a broad first DAG")
        );
        assert_eq!(
            request.review_contract["request_summary"]["latest_worker_rationale"]["comments"],
            serde_json::json!("critical window branch still feels shaky")
        );
    }

    /// PV Phase 8 monotonicity gate. A ProtectedReapproval request whose
    /// reopened scope carries a PV spec node surfaces the versioned
    /// monotonicity prose plus the per-node `diff_corr_fingerprint_axes`
    /// bullets in `request_summary.protected_reapproval_status`. An all-math
    /// request (empty `protected_reapproval_nodes`) carries no such status key.
    #[test]
    fn protected_reapproval_gate_carries_monotonicity_prose_and_axis_bullets() {
        let spec = NodeId::from("Spec");
        let approved = serde_json::json!({
            "own_tex": "old statement",
            "lean_semantic_closure": "lc",
            "preamble_tex": "pre",
        })
        .to_string();
        let current = serde_json::json!({
            "own_tex": "new statement",
            "lean_semantic_closure": "lc",
            "preamble_tex": "pre",
        })
        .to_string();
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            protected_reapproval_nodes: BTreeSet::from([spec.clone()]),
            protected_reapproval_corr_fingerprint_pairs: BTreeMap::from([(
                spec.clone(),
                crate::model::ProtectedReapprovalCorrDiffInput {
                    approved,
                    current,
                },
            )]),
            node_role: BTreeMap::from([(spec.clone(), PvRole::Spec)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let status = request.review_contract["request_summary"]["protected_reapproval_status"]
            .as_str()
            .expect("protected_reapproval_status string");
        assert!(
            status.contains("strengthening or"),
            "gate must carry the monotonicity prose; got {status:?}"
        );
        assert!(
            status.contains("Spec:") && status.contains("`.tex` statement block"),
            "gate must carry the per-node own_tex axis bullet; got {status:?}"
        );

        // Math counterpart: no protected_reapproval_nodes ⇒ no status key.
        let mut math = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut math, None);
        assert!(
            math.review_contract["request_summary"]
                .get("protected_reapproval_status")
                .is_none(),
            "all-math gate must carry no protected_reapproval_status"
        );
    }

    // Option C (2026-06-04): `review_contract_surfaces_allowed_override_ids`
    // removed — `allowed_override_ids` and `override_blocker_ids` are no
    // longer emitted into the reviewer contract. See
    // REVIEWER_OVERRIDE_RETIREMENT_2026-06-04.md.

    #[test]
    fn review_contract_surfaces_need_input_contract() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            mode: crate::TaskMode::Local,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/31_need_input.md"));
        assert_eq!(
            request.review_contract["need_input_contract"]["task_blocker_ids"],
            serde_json::json!([])
        );
        // Option C (2026-06-04): `override_blocker_ids` no longer emitted
        // into the need_input_contract.
        assert!(
            request.review_contract["need_input_contract"]
                .get("override_blocker_ids")
                .is_none(),
            "override_blocker_ids should not be emitted into need_input_contract"
        );
        assert_eq!(
            request.review_contract["need_input_contract"]["next_active"],
            serde_json::json!("")
        );
        assert_eq!(
            request.review_contract["need_input_contract"]["next_mode"],
            serde_json::json!(crate::TaskMode::Local)
        );
    }

    /// Structural invariant: the `need_input_contract` sub-block emitted by
    /// `review_contract_payload` MUST advertise empty arrays for every
    /// blocker-id / verifier-node list field. The kernel's own legality
    /// predicate `review_response_rejection_reasons` rejects ANY non-empty
    /// `reset_blockers` on NeedInput; the other three list fields are
    /// likewise NeedInput-illegal. Publishing placeholder strings here
    /// causes reviewers to paraphrase the example literally and get
    /// bounced. Guards against future schema-example drift.
    #[test]
    fn review_need_input_contract_lists_are_empty_arrays() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            mode: crate::TaskMode::Local,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let need_input = &request.review_contract["need_input_contract"];
        // Option C (2026-06-04): `override_blocker_ids` removed from the
        // need_input_contract sub-block; the field is no longer emitted.
        for field in [
            "task_blocker_ids",
            "reset_blocker_ids",
            "request_sound_verifier_node_ids",
        ] {
            assert_eq!(
                need_input[field],
                Value::Array(vec![]),
                "need_input_contract.{} must be an empty array (NeedInput legality predicate rejects any non-empty entries)",
                field
            );
        }
    }

    #[test]
    fn proof_review_prompt_includes_proof_restructure_strategy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/36_difficulty_strategy.md"));
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/37_restructure_strategy.md"));
    }

    #[test]
    fn proof_review_contract_surfaces_protected_semantic_confirmation() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: BTreeSet::from([crate::model::ReviewDecisionKind::Continue]),
            approved_target_nodes: BTreeSet::from([NodeId::from("A"), NodeId::from("B")]),
            protected_semantic_change_confirmation: Some(
                crate::model::ProtectedSemanticChangeConfirmation {
                    nodes: BTreeSet::from([NodeId::from("B")]),
                    next_active: Some(NodeId::from("A")),
                    next_mode: crate::model::TaskMode::CoarseRestructure,
                    allow_new_obligations: true,
                    must_close_active: false,
                },
            ),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        assert_eq!(
            schema["confirm_protected_semantic_change_scope"],
            serde_json::json!(true)
        );
        assert_eq!(
            request.review_contract["protected_semantic_change_contract"]["confirmation_required"],
            serde_json::json!(true)
        );
        assert_eq!(
            request.review_contract["protected_semantic_change_contract"]["allowed_nodes"],
            serde_json::json!(BTreeSet::from([NodeId::from("A"), NodeId::from("B")]))
        );
    }

    #[test]
    fn cleanup_review_prompt_omits_proof_difficulty_strategy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            allowed_difficulty_update_nodes: BTreeSet::from([NodeId::from("A")]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/36_difficulty_strategy.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/37_restructure_strategy.md"));
    }

    #[test]
    fn theorem_review_prompt_omits_revert_last_clean_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        // Always-shown umbrella stays
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/32_revert.md"));
        // last_clean carve-out is withheld; the mandatory threshold never
        // applies in TheoremStating, so its language would contradict the
        // legal non-rewind Continue path.
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/32a_revert_last_clean.md"));
    }

    #[test]
    fn proof_review_prompt_includes_revert_last_clean_fragment_after_revert() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        let revert_idx = fragments
            .iter()
            .position(|item| item == "review/common/32_revert.md")
            .expect("32_revert.md present");
        let last_clean_idx = fragments
            .iter()
            .position(|item| item == "review/common/32a_revert_last_clean.md")
            .expect("32a_revert_last_clean.md present in ProofFormalization");
        // `32a` must land immediately after `32_revert.md` — same
        // ordering position as the prior Python-side splice.
        assert_eq!(last_clean_idx, revert_idx + 1);
    }

    #[test]
    fn review_contract_withholds_csc_mandate_surface_while_it_is_disabled() {
        // Default posture: the mandate is off, so the reviewer is shown
        // neither the mandate fragment nor a threshold number it would
        // otherwise be told the kernel enforces.
        let mut env = crate::EnvScope::lock();
        env.unset("TRELLIS_CSC_LAST_CLEAN_THRESHOLD");
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        // The discretionary `last_clean` guidance survives.
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/32a_revert_last_clean.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/32b_revert_last_clean_mandate.md"));
        let summary = request.review_contract["request_summary"]
            .as_object()
            .expect("review request summary");
        assert!(!summary.contains_key("csc_last_clean_threshold"));
        assert!(!summary.contains_key("csc_rewind_waiver_count"));
        // The inputs the reviewer reasons about are still there.
        assert!(summary.contains_key("cycles_since_clean"));
        assert!(summary.contains_key("last_clean_rewind_count"));
    }

    #[test]
    fn review_contract_carries_csc_mandate_surface_once_the_operator_enables_it() {
        let mut env = crate::EnvScope::lock();
        env.set("TRELLIS_CSC_LAST_CLEAN_THRESHOLD", "15");
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        let carve_out_idx = fragments
            .iter()
            .position(|item| item == "review/common/32a_revert_last_clean.md")
            .expect("32a_revert_last_clean.md present in ProofFormalization");
        let mandate_idx = fragments
            .iter()
            .position(|item| item == "review/common/32b_revert_last_clean_mandate.md")
            .expect("32b mandate fragment present once enabled");
        assert_eq!(mandate_idx, carve_out_idx + 1);
        let summary = request.review_contract["request_summary"]
            .as_object()
            .expect("review request summary");
        assert_eq!(summary["csc_last_clean_threshold"], json!(15));
        assert_eq!(
            summary["csc_rewind_waiver_count"],
            json!(crate::model::CSC_REWIND_WAIVER_COUNT)
        );
    }

    #[test]
    fn theorem_review_prompt_omits_csc_mandate_fragment_even_when_enabled() {
        // The mandate is ProofFormalization-only; the stating phases keep
        // their legal non-rewind Continue path, so the mandate fragment
        // stays withheld there exactly as `32a` does.
        let mut env = crate::EnvScope::lock();
        env.set("TRELLIS_CSC_LAST_CLEAN_THRESHOLD", "15");
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/32b_revert_last_clean_mandate.md"));
    }

    #[test]
    fn cleanup_review_prompt_includes_revert_last_clean_fragment_after_revert() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        let revert_idx = fragments
            .iter()
            .position(|item| item == "review/common/32_revert.md")
            .expect("32_revert.md present");
        let last_clean_idx = fragments
            .iter()
            .position(|item| item == "review/common/32a_revert_last_clean.md")
            .expect("32a_revert_last_clean.md present in Cleanup");
        assert_eq!(last_clean_idx, revert_idx + 1);
    }

    #[test]
    fn worker_prompt_includes_new_node_difficulty_guidance_when_new_nodes_are_allowed() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/38_new_node_difficulty.md"));
    }

    #[test]
    fn worker_prompt_includes_new_node_difficulty_guidance_for_proof_easy() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofEasy,
                validation_kind: WorkerValidationKind::ProofEasy,
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::ProofEasy,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/38_new_node_difficulty.md"));
    }

    #[test]
    fn worker_prompt_omits_new_node_difficulty_guidance_when_new_nodes_are_forbidden() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            fresh_context: false,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Cleanup,
                validation_kind: WorkerValidationKind::Cleanup,
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::Cleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/38_new_node_difficulty.md"));
    }

    struct SourceRecourseOverrideGuard {
        prior: Option<bool>,
    }

    impl SourceRecourseOverrideGuard {
        fn install(value: bool) -> Self {
            let mut slot = super::SOURCE_RECOURSE_AVAILABLE_OVERRIDE
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let prior = *slot;
            *slot = Some(value);
            Self { prior }
        }
    }

    impl Drop for SourceRecourseOverrideGuard {
        fn drop(&mut self) {
            let mut slot = super::SOURCE_RECOURSE_AVAILABLE_OVERRIDE
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            *slot = self.prior;
        }
    }

    /// `reviewer_source_recourse_available()` feeds the Review prompt
    /// fragments, so this override is an input to request-payload
    /// derivation — exactly like the env-backed tuning knobs. It used to
    /// have a mutex of its own, which left it unserialized against tests
    /// that derive a Review payload twice and compare (the engine's
    /// `apply_event` invariant check). Flipping it between those two
    /// derivations surfaces as `InvariantViolation("in-flight request
    /// payload does not match derived state")` in a test that never
    /// touched the override. Take the one crate-wide lock instead.
    fn with_source_recourse_env<F: FnOnce()>(snapshot: Option<&str>, sha: Option<&str>, body: F) {
        let _guard = crate::process_globals_test_guard();
        let available = snapshot
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
            && sha.map(|value| !value.trim().is_empty()).unwrap_or(false);
        let _override_guard = SourceRecourseOverrideGuard::install(available);
        body();
    }

    #[test]
    fn review_prompt_includes_source_recourse_when_env_vars_set() {
        with_source_recourse_env(
            Some("/tmp/trellis-source-snapshot/abc"),
            Some("abc"),
            || {
                let mut request = WrapperRequest {
                    kind: crate::model::RequestKind::Review,
                    phase: Phase::TheoremStating,
                    ..WrapperRequest::default()
                };
                populate_request_prompt_contracts(&mut request, None);
                let fragments = request.review_contract["prompt_fragments"]
                    .as_array()
                    .expect("review prompt fragments");
                assert!(fragments
                    .iter()
                    .any(|item| item == "review/common/05_source_recourse.md"));
            },
        );
    }

    #[test]
    fn review_prompt_omits_source_recourse_when_env_vars_unset() {
        with_source_recourse_env(None, None, || {
            let mut request = WrapperRequest {
                kind: crate::model::RequestKind::Review,
                phase: Phase::TheoremStating,
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            let fragments = request.review_contract["prompt_fragments"]
                .as_array()
                .expect("review prompt fragments");
            assert!(!fragments
                .iter()
                .any(|item| item == "review/common/05_source_recourse.md"));
        });
    }

    #[test]
    fn review_prompt_omits_source_recourse_when_only_one_env_var_set() {
        // Defense-in-depth: both env vars must be set together. If the
        // operator sets only the SHA (or only the snapshot path), we
        // omit the fragment rather than render with a placeholder.
        with_source_recourse_env(Some("/tmp/trellis-source-snapshot/abc"), None, || {
            let mut request = WrapperRequest {
                kind: crate::model::RequestKind::Review,
                phase: Phase::TheoremStating,
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            let fragments = request.review_contract["prompt_fragments"]
                .as_array()
                .expect("review prompt fragments");
            assert!(!fragments
                .iter()
                .any(|item| item == "review/common/05_source_recourse.md"));
        });
        with_source_recourse_env(None, Some("abc"), || {
            let mut request = WrapperRequest {
                kind: crate::model::RequestKind::Review,
                phase: Phase::TheoremStating,
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            let fragments = request.review_contract["prompt_fragments"]
                .as_array()
                .expect("review prompt fragments");
            assert!(!fragments
                .iter()
                .any(|item| item == "review/common/05_source_recourse.md"));
        });
    }

    // -- Trim 3: forbidden_legacy_fields stub-only relic ----------------

    #[test]
    fn no_worker_contract_payload_no_longer_emits_forbidden_legacy_fields() {
        // Trim 3 absent-when-inert: the no_worker_contract_payload stub
        // (returned when kind != Worker) used to emit a vestigial
        // `forbidden_legacy_fields: []`. The actual worker contract
        // already omits it (audit A6); the stub now matches.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert!(
            !request
                .worker_contract
                .as_object()
                .unwrap()
                .contains_key("forbidden_legacy_fields"),
            "no_worker_contract_payload stub must not emit forbidden_legacy_fields"
        );
    }

    #[test]
    fn worker_contract_request_summary_includes_rejection_reasons_on_retry() {
        // Regression guard for a request-summary omission bug:
        // `deterministic_worker_rejection_reasons`
        // was populated at the request top level but absent from
        // `worker_contract.request_summary`, causing
        // `bridge_prompts.py`'s `request_summary.get(...)` lookup to render
        // `[]` and the 32_deterministic_worker_rejection.md fragment to surface
        // an empty array — leaving the retrying worker without the rejection
        // text. The fragment `last_invalid` pointer was rendered fine; only
        // the inline JSON was empty. This test pins the fix.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            retry_outcome_kind: crate::model::RetryOutcomeKind::Invalid,
            invalid_attempt: true,
            deterministic_worker_rejection_reasons: vec![
                "Declaration name is \"C0_identity\", expected \"FixedSetProjectionMiddleRegimeExponent\"".to_string(),
                ".lean shape errors: [\"single principal top-level declaration\"]".to_string(),
            ],
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let request_summary = &request.worker_contract["request_summary"];
        let reasons = request_summary
            .get("deterministic_worker_rejection_reasons")
            .expect("worker_contract.request_summary must surface deterministic_worker_rejection_reasons");
        let reasons_arr = reasons
            .as_array()
            .expect("rejection_reasons must be an array");
        assert_eq!(reasons_arr.len(), 2);
        assert!(
            reasons_arr[0]
                .as_str()
                .expect("first reason must be a string")
                .contains("C0_identity"),
            "rejection reason text must propagate to the worker prompt's request_summary"
        );
    }

    #[test]
    fn worker_contract_request_summary_emits_empty_rejection_reasons_when_none() {
        // Symmetry guard: when there's nothing to report, the field is still
        // present (and empty) — matches the reviewer payload's pattern at
        // line 1729-1732 and lets the bridge render an empty array without
        // a missing-key fallback.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let request_summary = &request.worker_contract["request_summary"];
        let reasons = request_summary
            .get("deterministic_worker_rejection_reasons")
            .expect("field must be present even when empty");
        assert!(
            reasons.as_array().expect("must be an array").is_empty(),
            "no-reject case should emit []"
        );
    }

    #[test]
    fn worker_contract_request_summary_surfaces_rejected_deviations() {
        // The recovery worker (seated on a substantiveness-Fail node whose
        // cause is a rejected deviation claim) must see the rejected set
        // explicitly so it can drop the claim or revise the deviation.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            rejected_deviations: BTreeMap::from([(
                crate::model::DeviationId::from("dev:flat"),
                "reference/flat.tex".to_string(),
            )]),
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let request_summary = &request.worker_contract["request_summary"];
        assert_eq!(
            request_summary["rejected_deviations"],
            json!({ "dev:flat": "reference/flat.tex" }),
            "worker contract must surface the rejected deviation for recovery"
        );
    }

    #[test]
    fn worker_contract_payload_no_longer_emits_forbidden_legacy_fields() {
        // Trim 3 present-when-relevant baseline: the actual
        // `worker_contract_payload` continues to omit the field
        // (audit A6 already removed it). The pair documents both halves
        // of the contract.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert!(
            !request
                .worker_contract
                .as_object()
                .unwrap()
                .contains_key("forbidden_legacy_fields"),
            "worker_contract_payload must not emit forbidden_legacy_fields"
        );
    }

    // -- Trim 10: paper_focus_ranges + work_style_hint default-omit -----

    #[test]
    fn worker_context_omits_default_routing_hint_fields() {
        // Trim 10 absent-when-inert: when paper_focus_ranges is empty and
        // work_style_hint is None (defaults), the worker_context payload
        // should omit both fields.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker_context = &request.worker_contract["request_summary"]["worker_context"];
        let map = worker_context
            .as_object()
            .expect("worker_context must be an object");
        assert!(
            !map.contains_key("paper_focus_ranges"),
            "default empty paper_focus_ranges should be omitted"
        );
        assert!(
            !map.contains_key("work_style_hint"),
            "default work_style_hint=none should be omitted"
        );
    }

    #[test]
    fn worker_context_includes_routing_hint_fields_when_set() {
        // Trim 10 present-when-relevant: when the kernel forwards
        // non-default routing hints, the fields must appear so the
        // worker can act on them.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofRestructure,
                paper_focus_ranges: vec![crate::model::PaperFocusRange {
                    start_line: 1,
                    end_line: 10,
                    reason: "hint".to_string(),
                    doc: None,
                }],
                work_style_hint: crate::model::WorkerWorkStyleHint::Restructure,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker_context = &request.worker_contract["request_summary"]["worker_context"];
        let map = worker_context
            .as_object()
            .expect("worker_context must be an object");
        assert!(
            map.contains_key("paper_focus_ranges"),
            "non-default paper_focus_ranges must be present"
        );
        assert!(
            map.contains_key("work_style_hint"),
            "non-default work_style_hint must be present"
        );
    }

    /// The reviewer-facing cleanup contract summary must advertise the
    /// batch dispatch capability (`cleanup_batch_tasks` + the cap) in
    /// `dispatch_semantics`, so the reviewer does not have to spelunk
    /// kernel source to confirm batch support. Regression for the
    /// system_feedback the reviewer raised when the prompt/kernel
    /// supported batching but the rendered contract omitted it.
    #[test]
    fn cleanup_contract_dispatch_semantics_advertises_batch() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            cleanup_audit_tasks_view: vec![crate::model::CleanupAuditTask {
                target_node: crate::model::NodeId::from("SomeLintNode"),
                rationale: "unused simp arg".to_string(),
                confidence: crate::model::CleanupTaskConfidence::default(),
                kind: crate::model::CleanupTaskKind::LintFix {
                    warning_text: "unnecessarySimpArgs".to_string(),
                },
                status: crate::model::CleanupTaskStatus::Pending,
                audit_origin_round: 1,
                swept_parents: BTreeSet::new(),
                region_block_lines: None,
            }],
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let ds = &request.review_contract["cleanup_contract"]["dispatch_semantics"];
        assert!(
            ds.get("cleanup_batch_tasks")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()),
            "dispatch_semantics must advertise cleanup_batch_tasks; got {ds:?}"
        );
        assert_eq!(
            ds.get("cleanup_batch_max").and_then(|v| v.as_u64()),
            Some(crate::model::CLEANUP_BATCH_MAX as u64),
            "dispatch_semantics must advertise the batch cap; got {ds:?}"
        );
    }

    /// Cleanup-v2 (audit Finding 5a): when a Substitution cleanup task is
    /// in flight, the worker's JSON must include the active task view
    /// fields (`cleanup_active_task_kind`, `cleanup_active_target_node`,
    /// `cleanup_active_rationale`). The substitution worker prompt
    /// fragment references these by name; without them the prompt
    /// rendered `target_node` against no actual value.
    #[test]
    fn worker_context_payload_surfaces_cleanup_substitution_view_fields() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                cleanup_active_task_kind_view: Some(crate::model::CleanupTaskKind::Substitution {
                    replacement: crate::model::CleanupReplacement::Mathlib {
                        citation: "Nat.add_comm".into(),
                    },
                }),
                cleanup_active_target_node_view: Some(crate::model::NodeId::from("Wrapper")),
                cleanup_active_rationale_view: "Inlines Nat.add_comm 1-for-1".to_string(),
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker_context = &request.worker_contract["request_summary"]["worker_context"];
        let map = worker_context
            .as_object()
            .expect("worker_context must be an object");
        assert!(
            map.contains_key("cleanup_active_task_kind"),
            "Substitution task_kind view field must be surfaced to the worker"
        );
        assert!(
            map.contains_key("cleanup_active_target_node"),
            "target_node view field must be surfaced to the worker"
        );
        assert_eq!(
            map.get("cleanup_active_target_node")
                .and_then(|v| v.as_str()),
            Some("Wrapper")
        );
        assert!(
            map.contains_key("cleanup_active_rationale"),
            "rationale view field must be surfaced to the worker"
        );
    }

    #[test]
    fn cleanup_extraction_contract_advertises_birth_and_batch_obligations() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                cleanup_active_batch_kind_view: Some("extract_helper".to_string()),
                cleanup_active_batch_view: vec![
                    (crate::model::NodeId::from("ParentA"), "first block".to_string()),
                    (crate::model::NodeId::from("ParentB"), "second block".to_string()),
                ],
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let contract = &request.worker_contract;
        assert_eq!(contract["scope_contract"]["new_nodes_allowed"], json!(true));
        assert_eq!(
            contract["request_summary"]["worker_context"]["cleanup_active_batch"],
            json!([["ParentA", "first block"], ["ParentB", "second block"]])
        );
        assert_eq!(
            contract["request_summary"]["worker_context"]["cleanup_active_batch_kind"],
            json!("extract_helper")
        );
        assert!(contract["reported_delta_fields"]
            .as_array()
            .is_some_and(|fields| fields.contains(&json!("target_claim_updates"))));
        assert_eq!(
            contract["prompt_schema_example"]["target_claim_updates"],
            json!({"new_helper_node_id": []})
        );
        let schema = &contract["prompt_schema_example"];
        let validated = crate::artifact_validation::validate_trellis_worker_result_data_with_allowed_outcomes(
            schema,
            &["valid".to_string(), "invalid".to_string()],
        );
        assert!(validated.ok, "schema example must validate: {:?}", validated.errors);
        let schema = schema.as_object().expect("prompt_schema_example object");
        assert!(!schema.contains_key("difficulty_updates"));
        assert!(!schema.contains_key("deviation_requests"));
        assert!(!schema.contains_key("needs_restructure_suggested_nodes"));
    }

    /// Cleanup-v2 (audit Finding 5a): legacy lint-only / non-cleanup
    /// worker requests (no active cleanup task) must NOT surface the
    /// view fields — they would be confusing noise (null payloads in
    /// non-cleanup contexts).
    #[test]
    fn worker_context_payload_omits_cleanup_view_fields_when_no_active_task() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                cleanup_active_task_kind_view: None,
                cleanup_active_target_node_view: None,
                cleanup_active_rationale_view: String::new(),
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let worker_context = &request.worker_contract["request_summary"]["worker_context"];
        let map = worker_context
            .as_object()
            .expect("worker_context must be an object");
        assert!(!map.contains_key("cleanup_active_task_kind"));
        assert!(!map.contains_key("cleanup_active_target_node"));
        assert!(!map.contains_key("cleanup_active_rationale"));
    }

    /// Cleanup-v2 (audit Finding 5b): `existing_node_scope_mode` for
    /// FinalCleanup must NOT be "all_present". The runtime validator
    /// restricts edits to `authorized_nodes ∪ {target_node}` for
    /// Substitution and `{target_node}` for LintFix — both are
    /// whitelist-shaped, not all_present.
    #[test]
    fn worker_contract_scope_mode_for_final_cleanup_is_authorized_existing_nodes() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::FinalCleanup,
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract {
                validation_kind: WorkerValidationKind::FinalCleanup,
                ..crate::model::WorkerAcceptanceContract::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let mode = request.worker_contract["scope_contract"]["existing_node_scope_mode"].clone();
        assert_eq!(mode, json!("authorized_existing_nodes"));
    }

    // -- Trim 13: acceptance_check_command_template empty -> Null -------

    #[test]
    fn corr_contract_acceptance_check_command_template_is_null() {
        // Trim 13 absent-when-inert: corr verifier has no acceptance
        // checker (the `&[]` slice is passed). The kernel emits Null so
        // the bridge null-drop helper strips the line entirely.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let view = &request.corr_contract["artifact_prompt_view"];
        assert_eq!(
            view["acceptance_check_command_template"],
            Value::Null,
            "corr verifier has no acceptance checker; field must be null"
        );
    }

    #[test]
    fn worker_contract_acceptance_check_command_template_is_array() {
        // Trim 13 present-when-relevant: the worker contract DOES supply
        // an acceptance check template (the runtime-snapshot context-aware
        // command). The field stays an array.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let view = &request.worker_contract["artifact_prompt_view"];
        let raw = &view["acceptance_check_command_template"];
        assert_eq!(
            raw,
            &json!([
                "python3",
                "{{check_script_path}}",
                "trellis-worker-result",
                "{{raw_output_path}}",
                "--repo",
                "{{repo_path}}",
                "--context-json",
                "{{acceptance_context_path}}"
            ]),
            "the documented acceptance command must carry both repository and context paths"
        );
    }

    // -- Trim 4: Continue-only schema example fields --------------------

    #[test]
    fn review_schema_example_omits_continue_only_fields_when_continue_disallowed() {
        // Trim 4 absent-when-inert: when allowed_decisions excludes
        // Continue (e.g. terminal-state Done-only or NeedInput-only),
        // the four routing-hint / human-input clear example fields are
        // unreachable and must be absent from the schema example.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Done);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            allowed_decisions: allowed,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        for field in [
            "clear_human_input",
            "next_worker_context_mode",
            "paper_focus_ranges",
            "work_style_hint",
        ] {
            assert!(
                !map.contains_key(field),
                "{field} should be absent when Continue not in allowed_decisions"
            );
        }
    }

    #[test]
    fn review_schema_example_includes_continue_only_fields_when_continue_allowed() {
        // Trim 4 present-when-relevant: when Continue is allowed, the
        // four fields must appear so the reviewer knows their schema.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        allowed.insert(crate::model::ReviewDecisionKind::NeedInput);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        for field in [
            "clear_human_input",
            "next_worker_context_mode",
            "paper_focus_ranges",
            "work_style_hint",
        ] {
            assert!(
                map.contains_key(field),
                "{field} must be present when Continue is in allowed_decisions"
            );
        }
    }

    #[test]
    fn review_schema_example_decision_uses_snake_case() {
        // Section F regression: previously the schema example emitted
        // `allowed_decisions` directly, producing PascalCase
        // (`AdvancePhase`, `NeedInput`). `parse_decision` lowercases the
        // input against snake_case constants, so a reviewer copy-paste of
        // `AdvancePhase` lowercases to `advancephase` and fails to match
        // `advance_phase`. The schema example must surface the snake_case
        // form. Sort order is BTreeSet iteration over the
        // ReviewDecisionKind enum (Continue, AdvancePhase, NeedInput, Done
        // — i.e. the enum-declared order, mapped to snake_case).
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        allowed.insert(crate::model::ReviewDecisionKind::AdvancePhase);
        allowed.insert(crate::model::ReviewDecisionKind::NeedInput);
        allowed.insert(crate::model::ReviewDecisionKind::Done);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        let decision = map.get("decision").expect("decision key present");
        let decision_arr = decision.as_array().expect("decision is a JSON array");
        let values: Vec<&str> = decision_arr
            .iter()
            .map(|v| v.as_str().expect("decision array element is a string"))
            .collect();
        assert_eq!(
            values,
            vec!["continue", "advance_phase", "need_input", "done"],
            "schema example decision array must be snake_case in enum-declared order"
        );
        assert!(
            !values.contains(&"AdvancePhase"),
            "schema example must not surface PascalCase AdvancePhase"
        );
        assert!(
            !values.contains(&"NeedInput"),
            "schema example must not surface PascalCase NeedInput"
        );
    }

    #[test]
    fn review_schema_example_decision_array_round_trips_through_validator() {
        // Section F regression: each rendered decision value must be
        // accepted by the artifact validator's decision check (the same
        // path `parse_decision` covers — both lowercase the raw string
        // against the snake_case constant set). Build a minimal valid
        // reviewer payload and substitute each schema-example decision
        // value; the validator must not emit the
        // "decision must be one of [...]" rejection for any of them.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        allowed.insert(crate::model::ReviewDecisionKind::AdvancePhase);
        allowed.insert(crate::model::ReviewDecisionKind::NeedInput);
        allowed.insert(crate::model::ReviewDecisionKind::Done);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let decision_arr = schema["decision"]
            .as_array()
            .expect("decision is a JSON array");
        for value in decision_arr {
            let value_str = value.as_str().expect("decision array element is a string");
            let payload = json!({
                "decision": value_str,
                "reason": "round-trip test",
                "comments": "",
                "task_blocker_ids": [],
                "override_blocker_ids": [],
                "reset_blocker_ids": [],
                "next_active": "",
                "next_mode": "global",
                "reset": "none",
                "difficulty_updates": {},
                "allow_new_obligations": true,
                "must_close_active": false,
                "clear_human_input": false,
            });
            let result =
                crate::artifact_validation::validate_trellis_reviewer_result_data(&payload);
            assert!(
                !result.errors.iter().any(|e| e.contains("decision must be one of")),
                "schema-example decision value {value_str:?} should round-trip through the validator without a 'decision must be one of' error; got errors: {:?}",
                result.errors
            );
        }
    }

    #[test]
    fn review_schema_example_includes_next_active_coarse_with_sentinel() {
        // Section B prompt-half: in ProofFormalization with a non-empty
        // `kernel_hinted_next_active_coarse_nodes` set and no retry
        // (RetryOutcomeKind::None), the schema example surfaces
        // `next_active_coarse` with the descriptive sentinel string so
        // reviewers don't have to grep source to learn about the field.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut hinted = BTreeSet::new();
        hinted.insert(NodeId::from("CoarseNodeB"));
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            kernel_hinted_next_active_coarse_nodes: hinted,
            retry_outcome_kind: RetryOutcomeKind::None,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        let value = map
            .get("next_active_coarse")
            .expect("next_active_coarse key present");
        let text = value
            .as_str()
            .expect("next_active_coarse schema-example value is a string");
        assert!(
            text.contains("kernel_hinted_next_active_coarse_nodes"),
            "descriptive sentinel must reference the hinted-coarse-nodes set; got {text:?}"
        );
        assert!(
            text.contains("preserve current anchor"),
            "descriptive sentinel must describe the empty-string preserve case; got {text:?}"
        );
        // `next_active_coarse` is documented as an optional reviewer
        // field via the `optional_fields` vec surfaced on the artifact
        // contract (one level above `prompt_schema_example`).
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields surfaced on artifact_contract");
        assert!(
            optional
                .iter()
                .any(|v| v.as_str() == Some("next_active_coarse")),
            "next_active_coarse must be listed in optional_fields"
        );
    }

    #[test]
    fn review_schema_example_next_active_coarse_is_empty_when_locked() {
        // Section B prompt-half: outside the ProofFormalization Continue
        // (non-retry) window — or with an empty hinted-coarse-nodes set —
        // the schema example surfaces an empty-string sentinel for
        // `next_active_coarse`, signalling "anchor locked / no switch
        // legal this cycle." The key still appears so reviewers see
        // the field; the value just tells them not to populate it.
        let mut allowed = std::collections::BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        // Case 1: TheoremStating phase (never legal to switch the coarse
        // anchor) — empty sentinel.
        let mut request_theorem = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed.clone(),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request_theorem, None);
        let theorem_schema =
            &request_theorem.review_contract["artifact_contract"]["prompt_schema_example"];
        let theorem_map = theorem_schema
            .as_object()
            .expect("theorem prompt_schema_example object");
        assert_eq!(
            theorem_map.get("next_active_coarse"),
            Some(&json!("")),
            "TheoremStating must surface next_active_coarse with the empty sentinel"
        );
        // Case 2: ProofFormalization but empty hinted set — still empty
        // sentinel (no anchor switch is currently legal).
        let mut request_proof = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            kernel_hinted_next_active_coarse_nodes: BTreeSet::new(),
            retry_outcome_kind: RetryOutcomeKind::None,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request_proof, None);
        let proof_schema =
            &request_proof.review_contract["artifact_contract"]["prompt_schema_example"];
        let proof_map = proof_schema
            .as_object()
            .expect("proof prompt_schema_example object");
        assert_eq!(
            proof_map.get("next_active_coarse"),
            Some(&json!("")),
            "ProofFormalization with empty hinted set must surface next_active_coarse with the empty sentinel"
        );
    }

    #[test]
    fn contracts_advertise_memory_challenges_when_process_memory_is_active() {
        // Process memory (spec §5): the challenge channel. The reviewer
        // and worker fragments instruct both roles to file a
        // `memory_challenges` item; the contract must offer the field, or
        // the role is told to emit something its contract does not
        // expose (the live cycle-491 halt loop: the reviewer
        // correctly refused and emitted system feedback instead).
        //
        // Gated on the runtime-resolved `process_memory_active` flag, on
        // the `sidecar_advertise_queue_fields` precedent: runs with no
        // active memory keep byte-identical contracts.
        let mut review = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            process_memory_active: true,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut review, None);
        let optional = review.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        assert!(
            optional.iter().any(|v| v.as_str() == Some("memory_challenges")),
            "memory_challenges must be listed in the review optional_fields"
        );
        assert!(
            review.review_contract["artifact_contract"]["prompt_schema_example"]
                .as_object()
                .expect("review prompt_schema_example object")
                .contains_key("memory_challenges"),
            "review schema example must advertise memory_challenges"
        );

        let mut worker = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            active_node: Some(NodeId::from("a")),
            process_memory_active: true,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut worker, None);
        assert!(
            worker.worker_contract["prompt_schema_example"]
                .as_object()
                .expect("worker prompt_schema_example object")
                .contains_key("memory_challenges"),
            "worker schema example must advertise memory_challenges"
        );
    }

    #[test]
    fn contracts_omit_memory_challenges_without_active_process_memory() {
        // The gate's other half: a run with no active `process-memory/`
        // entry has nothing to challenge, so neither contract surfaces
        // the field and the rendered bytes match a pre-feature run. The
        // field stays LEGAL when unadvertised (the validator normalizes
        // it and `record_memory_challenges` records it), so the gate is
        // presentation-only.
        let mut review = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut review, None);
        assert!(!review.process_memory_active, "test precondition");
        let optional = review.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        assert!(
            !optional.iter().any(|v| v.as_str() == Some("memory_challenges")),
            "memory_challenges must be absent from optional_fields without active process memory"
        );
        assert!(
            !review.review_contract["artifact_contract"]["prompt_schema_example"]
                .as_object()
                .expect("review prompt_schema_example object")
                .contains_key("memory_challenges"),
            "review schema example must omit memory_challenges without active process memory"
        );

        let mut worker = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            active_node: Some(NodeId::from("a")),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut worker, None);
        assert!(
            !worker.worker_contract["prompt_schema_example"]
                .as_object()
                .expect("worker prompt_schema_example object")
                .contains_key("memory_challenges"),
            "worker schema example must omit memory_challenges without active process memory"
        );
    }

    #[test]
    fn review_schema_example_omits_dismiss_fields_without_stuck_math_audit() {
        // Section A: with an `audit_plan` present but
        // `stuck_math_audit.active=false`, the kernel-side gate
        // (model.rs:3087-3115) rejects any dismissal attempt. Mirror that
        // in the schema-example renderer: the `dismiss_audit_plan` /
        // `dismissed_tasks` fields must NOT be surfaced so reviewers
        // don't see an affordance that the kernel will hard-reject.
        //
        // Option A extension: visibility ⇔ dismissability. Beyond
        // suppressing the dismiss field names, the entire
        // `audit_plan_contract` block and the `review_contract.audit_plan`
        // surface go to Null when dismissal is illegal — preventing the
        // Review 315 muddle where the reviewer reads the plan as
        // authoritative but the kernel rejects every dismissal attempt.
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: false,
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        assert!(
            !map.contains_key("dismiss_audit_plan"),
            "dismiss_audit_plan must be absent when StuckMathAudit is inactive"
        );
        assert!(
            !map.contains_key("dismissed_tasks"),
            "dismissed_tasks must be absent when StuckMathAudit is inactive"
        );
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        let optional_names: Vec<&str> = optional.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !optional_names.contains(&"dismiss_audit_plan"),
            "dismiss_audit_plan must not be listed in optional_fields when StuckMathAudit is inactive"
        );
        assert!(
            !optional_names.contains(&"dismissed_tasks"),
            "dismissed_tasks must not be listed in optional_fields when StuckMathAudit is inactive"
        );
        // Option A: dismissal illegal ⇒ live audit_plan surface zeros
        // out entirely (the reviewer sees a clearly-tagged historical
        // snapshot via `previous_audit_plan_snapshot` instead, not the
        // live plan).
        assert!(
            review_audit_dismissal_legal(&request) == false,
            "test precondition: dismissal must be illegal"
        );
        assert_eq!(
            request.review_contract["audit_plan"],
            Value::Null,
            "review_contract.audit_plan must be null when dismissal is illegal (visibility ⇔ dismissability)"
        );
        assert_eq!(
            request.review_contract["audit_plan_contract"],
            Value::Null,
            "audit_plan_contract must be null when dismissal is illegal"
        );
        // The block-form of the A-followup field-name gating is now
        // collapsed into the whole-block null gating: the dismiss
        // field-name keys remain absent (vacuously, since the block
        // is null and not an object).
    }

    #[test]
    fn review_schema_example_omits_dismiss_fields_when_phase_disallows() {
        // Section A: with `stuck_math_audit.active=true` but the phase
        // outside the legal set (Cleanup here) AND the audit plan not
        // flagged `need_input_audit`, the kernel-side gate rejects
        // dismissals. The schema example must omit the dismiss-fields so
        // the affordance isn't shown when illegal.
        //
        // Option A extension: visibility ⇔ dismissability. The whole
        // `audit_plan_contract` block and the `review_contract.audit_plan`
        // surface go to Null when dismissal is illegal.
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: false,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        assert!(
            !map.contains_key("dismiss_audit_plan"),
            "dismiss_audit_plan must be absent in Cleanup phase with non-need-input plan"
        );
        assert!(
            !map.contains_key("dismissed_tasks"),
            "dismissed_tasks must be absent in Cleanup phase with non-need-input plan"
        );
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        let optional_names: Vec<&str> = optional.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !optional_names.contains(&"dismiss_audit_plan"),
            "dismiss_audit_plan must not be listed in optional_fields outside the legal window"
        );
        assert!(
            !optional_names.contains(&"dismissed_tasks"),
            "dismissed_tasks must not be listed in optional_fields outside the legal window"
        );
        // Option A: dismissal illegal ⇒ live audit_plan surface zeros
        // out entirely.
        assert!(
            review_audit_dismissal_legal(&request) == false,
            "test precondition: dismissal must be illegal"
        );
        assert_eq!(
            request.review_contract["audit_plan"],
            Value::Null,
            "review_contract.audit_plan must be null when dismissal is illegal"
        );
        assert_eq!(
            request.review_contract["audit_plan_contract"],
            Value::Null,
            "audit_plan_contract must be null when dismissal is illegal"
        );
    }

    #[test]
    fn review_schema_example_includes_dismiss_fields_in_legal_window() {
        // Section A: `stuck_math_audit.active=true` + phase
        // ProofFormalization + audit_plan present = legal window per
        // model.rs:3087-3115. Schema example must surface
        // dismiss_audit_plan / dismissed_tasks so the reviewer sees the
        // affordance.
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: false,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        assert!(
            map.contains_key("dismiss_audit_plan"),
            "dismiss_audit_plan must be present in the legal window (ProofFormalization + active StuckMathAudit)"
        );
        assert!(
            map.contains_key("dismissed_tasks"),
            "dismissed_tasks must be present in the legal window (ProofFormalization + active StuckMathAudit)"
        );
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        let optional_names: Vec<&str> = optional.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            optional_names.contains(&"dismiss_audit_plan"),
            "dismiss_audit_plan must be listed in optional_fields in the legal window"
        );
        assert!(
            optional_names.contains(&"dismissed_tasks"),
            "dismissed_tasks must be listed in optional_fields in the legal window"
        );
        // A-followup: audit_plan_contract MUST surface dismiss field names
        // in the legal window so the reviewer knows the affordance exists.
        let apc = &request.review_contract["audit_plan_contract"];
        let apc_map = apc.as_object().expect("audit_plan_contract object");
        assert!(apc_map.contains_key("dismiss_audit_plan_field"));
        assert!(apc_map.contains_key("dismissed_tasks_field"));
        assert!(apc_map.contains_key("dismissed_tasks_shape"));
        assert_eq!(
            apc_map.get("dismissal_legal").and_then(|v| v.as_bool()),
            Some(true)
        );
        // Option A: dismissal legal ⇒ live audit_plan surface present
        // (visibility ⇔ dismissability).
        assert!(
            review_audit_dismissal_legal(&request),
            "test precondition: dismissal must be legal"
        );
        assert!(
            !request.review_contract["audit_plan"].is_null(),
            "review_contract.audit_plan must be non-null in the legal window"
        );
    }

    #[test]
    fn review_schema_example_includes_dismiss_fields_on_need_input_audit_plan() {
        // Section A: `audit_plan.need_input_audit=true` opens the
        // dismissal window even outside ProofFormalization /
        // TheoremStating (matches the model.rs:3107-3115 `|| plan.need_input_audit`
        // branch). Use Cleanup phase to confirm the phase check is
        // bypassed by the need_input_audit flag.
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::Cleanup,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: true,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let schema = &request.review_contract["artifact_contract"]["prompt_schema_example"];
        let map = schema.as_object().expect("prompt_schema_example object");
        assert!(
            map.contains_key("dismiss_audit_plan"),
            "dismiss_audit_plan must be present when audit_plan.need_input_audit=true"
        );
        assert!(
            map.contains_key("dismissed_tasks"),
            "dismissed_tasks must be present when audit_plan.need_input_audit=true"
        );
        let optional = request.review_contract["artifact_contract"]["optional_fields"]
            .as_array()
            .expect("optional_fields array");
        let optional_names: Vec<&str> = optional.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            optional_names.contains(&"dismiss_audit_plan"),
            "dismiss_audit_plan must be listed in optional_fields with a need_input_audit plan"
        );
        assert!(
            optional_names.contains(&"dismissed_tasks"),
            "dismissed_tasks must be listed in optional_fields with a need_input_audit plan"
        );
        // A-followup: audit_plan_contract gating under the need-input branch.
        let apc = &request.review_contract["audit_plan_contract"];
        let apc_map = apc.as_object().expect("audit_plan_contract object");
        assert!(apc_map.contains_key("dismiss_audit_plan_field"));
        assert!(apc_map.contains_key("dismissed_tasks_field"));
        assert!(apc_map.contains_key("dismissed_tasks_shape"));
        assert_eq!(
            apc_map.get("dismissal_legal").and_then(|v| v.as_bool()),
            Some(true)
        );
        // Option A: dismissal legal ⇒ live audit_plan surface present
        // (visibility ⇔ dismissability).
        assert!(
            review_audit_dismissal_legal(&request),
            "test precondition: dismissal must be legal"
        );
        assert!(
            !request.review_contract["audit_plan"].is_null(),
            "review_contract.audit_plan must be non-null on a need_input_audit plan"
        );
    }

    #[test]
    fn review_prompt_includes_recent_burst_history_fragment() {
        // Pin the fragment id so the assembly list never silently drops
        // the cross-cycle history pointer for the reviewer. The fragment
        // must appear AFTER the verifier reasoning fragment and BEFORE
        // the contract fragment so the reviewer reads history while
        // forming its decision shape.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        let idx_history = fragments
            .iter()
            .position(|item| item == "review/common/26_recent_burst_history.md")
            .expect("review/common/26_recent_burst_history.md must be included");
        let idx_verifier = fragments
            .iter()
            .position(|item| item == "review/common/25_verifier_reasoning.md")
            .expect("verifier reasoning fragment must be included");
        let idx_contract = fragments
            .iter()
            .position(|item| item == "review/common/30_contract.md")
            .expect("contract fragment must be included");
        assert!(
            idx_verifier < idx_history,
            "recent_burst_history should sit after verifier reasoning"
        );
        assert!(
            idx_history < idx_contract,
            "recent_burst_history should sit before the contract fragment"
        );
    }

    #[test]
    fn review_prompt_kernel_scheduled_sound_fragment_is_gated_on_marked_nodes() {
        // Without kernel-scheduled sound results the fragment is absent
        // and the request_summary omits the key (byte-identical baseline).
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(
            !fragments
                .iter()
                .any(|item| item == "review/common/25a_kernel_scheduled_sound.md"),
            "provenance fragment must be absent without kernel-scheduled sound results"
        );
        assert!(request.review_contract["request_summary"]
            .get("kernel_scheduled_sound_nodes")
            .is_none());

        // With a kernel-scheduled sound result the fragment renders right
        // after the verifier-reasoning block and the request_summary names
        // the nodes.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            kernel_scheduled_sound_review_nodes: std::collections::BTreeSet::from([
                NodeId::from("a"),
            ]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        let idx_provenance = fragments
            .iter()
            .position(|item| item == "review/common/25a_kernel_scheduled_sound.md")
            .expect("provenance fragment must be included");
        let idx_verifier = fragments
            .iter()
            .position(|item| item == "review/common/25_verifier_reasoning.md")
            .expect("verifier reasoning fragment must be included");
        let idx_history = fragments
            .iter()
            .position(|item| item == "review/common/26_recent_burst_history.md")
            .expect("recent burst history fragment must be included");
        assert!(idx_verifier < idx_provenance && idx_provenance < idx_history);
        assert_eq!(
            request.review_contract["request_summary"]["kernel_scheduled_sound_nodes"],
            serde_json::json!(["a"])
        );
    }

    #[test]
    fn worker_prompt_includes_recent_burst_history_fragment() {
        // Pin the fragment id so the assembly list never silently drops
        // the cross-cycle history pointer for the worker. Order check
        // is light: history must appear after the request and before
        // the contract.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            active_node: Some(NodeId::from("Foo")),
            ..WrapperRequest::default()
        };
        request.worker_acceptance.validation_kind = crate::model::WorkerValidationKind::ProofLocal;
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        let idx_history = fragments
            .iter()
            .position(|item| item == "worker/common/36_recent_burst_history.md")
            .expect("worker/common/36_recent_burst_history.md must be included");
        let idx_request = fragments
            .iter()
            .position(|item| item == "worker/common/30_request.md")
            .expect("request fragment must be included");
        let idx_contract = fragments
            .iter()
            .position(|item| item == "worker/common/40_contract.md")
            .expect("contract fragment must be included");
        assert!(
            idx_request < idx_history,
            "recent_burst_history should sit after the request fragment"
        );
        assert!(
            idx_history < idx_contract,
            "recent_burst_history should sit before the contract fragment"
        );
    }

    #[test]
    fn review_prompt_includes_stuck_math_audit_fragment_when_active() {
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(
            fragments
                .iter()
                .any(|item| item == "review/common/29_stuck_math_audit.md"),
            "active StuckMathAudit reviews must include the StuckMathAudit prompt fragment"
        );
        assert_eq!(
            request.review_contract["stuck_math_audit_contract"]["response_field"],
            json!("stuck_math_audit")
        );
        assert!(
            request.review_contract["artifact_contract"]["prompt_schema_example"]
                .as_object()
                .expect("schema object")
                .contains_key("stuck_math_audit"),
            "active StuckMathAudit reviews must render the response field shape"
        );
    }

    #[test]
    fn review_prompt_splits_need_input_auditor_plan_fragments() {
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "need input recovery".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: true,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/29_need_input_auditor.md"));
        assert!(fragments
            .iter()
            .any(|item| item == "review/common/29b_need_input_audit_plan.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/29_stuck_math_audit.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/29b_audit_plan.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "review/common/29b_planner_plan.md"));
    }

    /// A reviewer request carrying a live audit plan, parameterized by the
    /// plan's origin flags, returns the selected 29b-family plan fragment names.
    fn review_audit_plan_fragments(plan: crate::model::AuditPlan) -> Vec<String> {
        let mut allowed = BTreeSet::new();
        allowed.insert(crate::model::ReviewDecisionKind::Continue);
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            allowed_decisions: allowed,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "plan".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            audit_plan: Some(plan),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        request.review_contract["prompt_fragments"]
            .as_array()
            .expect("review prompt fragments")
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn review_prompt_selects_planner_plan_for_initial_plan() {
        let frags = review_audit_plan_fragments(crate::model::AuditPlan {
            initial_plan: true,
            ..crate::model::AuditPlan::default()
        });
        assert!(frags.iter().any(|f| f == "review/common/29b_planner_plan.md"));
        assert!(!frags.iter().any(|f| f == "review/common/29b_audit_plan.md"));
        assert!(!frags
            .iter()
            .any(|f| f == "review/common/29b_need_input_audit_plan.md"));
    }

    #[test]
    fn review_prompt_selects_planner_plan_for_revision_plan() {
        let frags = review_audit_plan_fragments(crate::model::AuditPlan {
            revision_audit: true,
            ..crate::model::AuditPlan::default()
        });
        assert!(frags.iter().any(|f| f == "review/common/29b_planner_plan.md"));
        assert!(!frags.iter().any(|f| f == "review/common/29b_audit_plan.md"));
    }

    #[test]
    fn review_prompt_selects_stagnation_plan_for_plain_plan() {
        let frags = review_audit_plan_fragments(crate::model::AuditPlan::default());
        assert!(frags.iter().any(|f| f == "review/common/29b_audit_plan.md"));
        assert!(!frags.iter().any(|f| f == "review/common/29b_planner_plan.md"));
    }

    #[test]
    fn worker_prompt_includes_neutral_reviewer_lean_product_handoff() {
        let product = json!({"kind": "sufficient_statement", "statement": "add invariant H"});
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                last_reviewer_lean_product: Some(product.clone()),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(
            fragments
                .iter()
                .any(|item| item == "worker/common/34b_stuck_math_reviewer_lean_product.md"),
            "worker prompts must include the neutral StuckMathAudit handoff fragment when a reviewer Lean product exists"
        );
        assert_eq!(request.worker_contract["reviewer_lean_product"], product);
    }

    #[test]
    fn worker_prompt_splits_need_input_auditor_plan_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            audit_plan: Some(crate::model::AuditPlan {
                need_input_audit: true,
                ..crate::model::AuditPlan::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments");
        assert!(fragments
            .iter()
            .any(|item| item == "worker/common/34c_need_input_audit_plan.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/34c_audit_plan.md"));
        assert!(!fragments
            .iter()
            .any(|item| item == "worker/common/34c_planner_plan.md"));
    }

    /// A worker request carrying a live audit plan, parameterized by the plan's
    /// origin flags, returns the selected 34c-family plan fragment names.
    fn worker_audit_plan_fragments(plan: crate::model::AuditPlan) -> Vec<String> {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            audit_plan: Some(plan),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        request.worker_contract["prompt_fragments"]
            .as_array()
            .expect("worker prompt fragments")
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn worker_prompt_selects_planner_plan_for_initial_plan() {
        let frags = worker_audit_plan_fragments(crate::model::AuditPlan {
            initial_plan: true,
            ..crate::model::AuditPlan::default()
        });
        assert!(frags.iter().any(|f| f == "worker/common/34c_planner_plan.md"));
        assert!(!frags.iter().any(|f| f == "worker/common/34c_audit_plan.md"));
        assert!(!frags
            .iter()
            .any(|f| f == "worker/common/34c_need_input_audit_plan.md"));
    }

    #[test]
    fn worker_prompt_selects_planner_plan_for_revision_plan() {
        let frags = worker_audit_plan_fragments(crate::model::AuditPlan {
            revision_audit: true,
            ..crate::model::AuditPlan::default()
        });
        assert!(frags.iter().any(|f| f == "worker/common/34c_planner_plan.md"));
        assert!(!frags.iter().any(|f| f == "worker/common/34c_audit_plan.md"));
    }

    #[test]
    fn worker_prompt_selects_stagnation_plan_for_plain_plan() {
        let frags = worker_audit_plan_fragments(crate::model::AuditPlan::default());
        assert!(frags.iter().any(|f| f == "worker/common/34c_audit_plan.md"));
        assert!(!frags.iter().any(|f| f == "worker/common/34c_planner_plan.md"));
    }

    #[test]
    fn review_request_summary_includes_recent_burst_history_path() {
        // The kernel-authored review request_summary surfaces the
        // ledger path so the prompt fragment has a single discovery
        // point and operator-side tooling can reference the same
        // canonical location.
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.review_contract["request_summary"]["recent_burst_history_path"],
            serde_json::json!(".trellis/logs/burst-history.jsonl"),
        );
    }

    #[test]
    fn theorem_worker_prompt_includes_helper_policy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.worker_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            frag_strs
                .iter()
                .any(|f| f == "worker/theorem_stating/17_helper_policy.md"),
            "worker TheoremStating prompt is missing 17_helper_policy.md; got: {frag_strs:?}"
        );
    }

    #[test]
    fn revision_worker_prompt_includes_revision_scope_fragment_and_summary() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::RevisionStating,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            revision_context: Some(crate::model::RequestRevisionContext::default()),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.worker_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            frag_strs
                .iter()
                .any(|f| f == "worker/revision_stating/05_revision_scope.md"),
            "worker RevisionStating prompt is missing 05_revision_scope.md; got: {frag_strs:?}"
        );
        assert!(
            request.worker_contract["request_summary"]
                .get("revision_scope")
                .map(|v| v.is_object())
                .unwrap_or(false),
            "worker RevisionStating request_summary is missing a revision_scope object; got: {:?}",
            request.worker_contract["request_summary"]
        );
    }

    #[test]
    fn theorem_review_prompt_includes_helper_policy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.review_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            frag_strs
                .iter()
                .any(|f| f == "review/common/33c_theorem_helper_policy.md"),
            "review TheoremStating prompt is missing 33c_theorem_helper_policy.md; got: {frag_strs:?}"
        );
    }

    #[test]
    fn theorem_stuck_math_audit_prompt_includes_helper_policy_fragment() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::TheoremStating,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            frag_strs
                .iter()
                .any(|f| f == "review/common/33c_theorem_helper_policy.md"),
            "TheoremStating audit prompt is missing helper policy; got: {frag_strs:?}"
        );
    }

    #[test]
    fn proof_formalization_stuck_math_audit_omits_theorem_helper_policy() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::ProofFormalization,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            !frag_strs
                .iter()
                .any(|f| f == "review/common/33c_theorem_helper_policy.md"),
            "ProofFormalization audit should NOT include the TheoremStating helper policy"
        );
    }

    // ===== Fresh-run initial-planner contract selection =====

    fn initial_planner_request(
        is_pv: bool,
        source: crate::model::InitialPlanningSource,
    ) -> WrapperRequest {
        WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::TheoremStating,
            is_pv,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "initial planning".into(),
                initial_planning: Some(crate::model::InitialPlanningContext {
                    source,
                    configured_target_ids: std::collections::BTreeSet::from(["t".to_string()]),
                    ..Default::default()
                }),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        }
    }

    fn initial_planner_fragments(request: &mut WrapperRequest) -> Vec<String> {
        populate_request_prompt_contracts(request, None);
        assert_eq!(
            request.stuck_math_audit_contract["burst_role"], "initial_planner",
            "the initial-planning lane must select the initial_planner role"
        );
        assert!(
            request.stuck_math_audit_contract["initial_planning"].is_object(),
            "the planner packet must ride the contract"
        );
        request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .expect("prompt_fragments array")
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn initial_planner_contract_selects_math_role_fragment() {
        let mut request = initial_planner_request(
            false,
            crate::model::InitialPlanningSource::PaperManuscript {
                paper_tex_path: "paper/main.tex".into(),
            },
        );
        let frags = initial_planner_fragments(&mut request);
        assert!(
            frags.contains(&"stuck_math_audit/common/01c_initial_planner_role.md".to_string()),
            "math run must select the common initial-planner role; got {frags:?}"
        );
        // The selected math role fragment resolves to a real file on disk.
        assert!(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../trellis/prompt_fragments/stuck_math_audit/common/01c_initial_planner_role.md")
                .exists(),
            "the math initial-planner role fragment must exist on disk"
        );
        // The fixed planner fragment list must not carry the theorem-stating
        // framing fragment (avoids duplicated statements in one rendered
        // prompt).
        assert!(
            !frags
                .iter()
                .any(|f| f.ends_with("01b_theorem_stating_framing.md")),
            "planner prompt must not duplicate theorem-stating framing; got {frags:?}"
        );
        // No PV role fragment leaks into a math prompt.
        assert!(
            !frags.iter().any(|f| f.starts_with("pv/stuck_audit/01c_initial_planner")),
            "math prompt must not carry a PV planner role; got {frags:?}"
        );
    }

    #[test]
    fn initial_planner_contract_selects_pv_prose_role_fragment() {
        let mut request = initial_planner_request(
            true,
            crate::model::InitialPlanningSource::PvGoalProse {
                goal_path: "GOAL.md".into(),
            },
        );
        let frags = initial_planner_fragments(&mut request);
        assert!(
            frags.contains(&"pv/stuck_audit/01c_initial_planner_role.md".to_string()),
            "PV prose run must select the PV prose initial-planner role; got {frags:?}"
        );
    }

    // ===== Coverage re-planning contract selection =====

    fn coverage_planner_request(is_pv: bool) -> WrapperRequest {
        let mut request = initial_planner_request(
            is_pv,
            crate::model::InitialPlanningSource::PaperManuscript {
                paper_tex_path: "paper/main.tex".into(),
            },
        );
        let ctx = request
            .stuck_math_audit
            .initial_planning
            .as_mut()
            .expect("carrier");
        ctx.coverage_replanning = true;
        ctx.uncovered_target_ids = std::collections::BTreeSet::from(["t".to_string()]);
        ctx.covered_targets = std::collections::BTreeMap::from([(
            "s".to_string(),
            std::collections::BTreeSet::from(["Base".to_string()]),
        )]);
        request
    }

    #[test]
    fn coverage_planner_contract_selects_coverage_role_fragment_math() {
        let mut request = coverage_planner_request(false);
        // Same burst_role + scenario as the cycle-1 planner (asserted inside
        // the helper): no downstream consumer needs the distinction.
        let frags = initial_planner_fragments(&mut request);
        assert!(
            frags.contains(
                &"stuck_math_audit/common/01c_coverage_planner_role.md".to_string()
            ),
            "coverage carrier must select the coverage-planner role; got {frags:?}"
        );
        assert!(
            !frags.contains(
                &"stuck_math_audit/common/01c_initial_planner_role.md".to_string()
            ),
            "coverage carrier must not also carry the cycle-1 role; got {frags:?}"
        );
        assert_eq!(
            request.stuck_math_audit_contract["request_summary"]["scenario"],
            "initial_planning"
        );
        // The report schema example swaps to the coverage wording.
        let report_example = request.stuck_math_audit_contract["artifact_contract"]
            ["prompt_schema_example"]["report"]
            .as_str()
            .expect("report example");
        assert!(
            report_example.starts_with("coverage re-plan:"),
            "coverage schema example must use the coverage wording; got {report_example}"
        );
        // Both coverage role fragments resolve to real files on disk.
        for path in [
            "../trellis/prompt_fragments/stuck_math_audit/common/01c_coverage_planner_role.md",
            "../trellis/prompt_fragments/pv/stuck_audit/01c_coverage_planner_role.md",
        ] {
            assert!(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path).exists(),
                "coverage role fragment must exist on disk: {path}"
            );
        }
    }

    #[test]
    fn coverage_planner_contract_selects_pv_role_fragment() {
        let mut request = coverage_planner_request(true);
        let frags = initial_planner_fragments(&mut request);
        assert!(
            frags.contains(&"pv/stuck_audit/01c_coverage_planner_role.md".to_string()),
            "PV coverage carrier must select the PV coverage role; got {frags:?}"
        );
    }

    #[test]
    fn coverage_replanning_false_contract_is_byte_identical_to_cycle_one() {
        // A carrier whose coverage fields are explicitly default produces the
        // exact contract of a pre-feature carrier (the new fields are
        // skip-when-default, so the embedded context JSON is also identical).
        let source = crate::model::InitialPlanningSource::PaperManuscript {
            paper_tex_path: "paper/main.tex".into(),
        };
        let mut request = initial_planner_request(false, source.clone());
        let mut explicit = initial_planner_request(false, source);
        {
            let ctx = explicit
                .stuck_math_audit
                .initial_planning
                .as_mut()
                .expect("carrier");
            ctx.coverage_replanning = false;
            ctx.uncovered_target_ids.clear();
            ctx.covered_targets.clear();
        }
        populate_request_prompt_contracts(&mut request, None);
        populate_request_prompt_contracts(&mut explicit, None);
        assert_eq!(
            request.stuck_math_audit_contract, explicit.stuck_math_audit_contract,
            "default-valued coverage fields must not change the contract"
        );
        let frags: Vec<&str> = request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            frags.contains(&"stuck_math_audit/common/01c_initial_planner_role.md"),
            "coverage_replanning=false keeps the cycle-1 role; got {frags:?}"
        );
        assert!(
            serde_json::to_string(&request.stuck_math_audit_contract["initial_planning"])
                .expect("context json")
                .find("coverage_replanning")
                .is_none(),
            "default coverage fields must stay off the contract wire"
        );
    }

    // ===== Process-rules pointer fragment (03c) =====

    const PROCESS_RULES_FRAGMENT: &str = "stuck_math_audit/common/03c_process_rules.md";

    fn stuck_audit_fragments_of(mut request: WrapperRequest) -> Vec<String> {
        populate_request_prompt_contracts(&mut request, None);
        request.stuck_math_audit_contract["prompt_fragments"]
            .as_array()
            .expect("prompt_fragments array")
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect()
    }

    /// Build a StuckMathAudit request for each of the eight audit scenarios
    /// (the coverage planner rides the initial-planning carrier, so it is a
    /// ninth request shape here).
    fn all_audit_scenario_requests(is_pv: bool) -> Vec<(&'static str, WrapperRequest)> {
        let base = || WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::TheoremStating,
            is_pv,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "test".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        let mut out = Vec::new();
        out.push(("structural", base()));
        let mut need_input = base();
        need_input.stuck_math_audit.need_input_audit =
            Some(crate::model::NeedInputAuditContext::default());
        out.push(("need_input", need_input));
        let mut global_repair = base();
        global_repair.pending_global_repair_request =
            Some(crate::model::PendingGlobalRepairRequest::default());
        out.push(("global_repair", global_repair));
        let mut gap_research = base();
        gap_research.stuck_math_audit.gap_research =
            Some(crate::model::GapResearchContext::default());
        out.push(("gap_research", gap_research));
        let mut gap_critic = base();
        gap_critic.stuck_math_audit.gap_plan_critique =
            Some(crate::model::GapPlanCritiqueContext::default());
        out.push(("gap_plan_critic", gap_critic));
        let mut revision = base();
        revision.phase = Phase::RevisionStating;
        revision.stuck_math_audit.revision_planning =
            Some(crate::model::RevisionPlanningContext::default());
        out.push(("revision_planning", revision));
        let mut initial = base();
        initial.stuck_math_audit.initial_planning =
            Some(crate::model::InitialPlanningContext::default());
        out.push(("initial_planning", initial));
        let mut coverage = base();
        coverage.stuck_math_audit.initial_planning =
            Some(crate::model::InitialPlanningContext {
                coverage_replanning: true,
                ..Default::default()
            });
        out.push(("coverage_replanning", coverage));
        let mut assumptions = base();
        assumptions.stuck_math_audit.assumptions_lane =
            Some(crate::model::AssumptionsLaneContext::default());
        out.push(("assumptions_lane", assumptions));
        out
    }

    #[test]
    fn process_rules_pointer_rides_every_audit_scenario_math_and_pv() {
        for is_pv in [false, true] {
            for (label, request) in all_audit_scenario_requests(is_pv) {
                let frags = stuck_audit_fragments_of(request);
                assert!(
                    frags.iter().any(|f| f == PROCESS_RULES_FRAGMENT),
                    "audit scenario {label} (is_pv={is_pv}) must carry the process-rules pointer; got {frags:?}"
                );
            }
        }
        // The pointer file resolves on disk.
        assert!(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../trellis/prompt_fragments/stuck_math_audit/common/03c_process_rules.md")
                .exists(),
            "the process-rules pointer fragment must exist on disk"
        );
    }

    #[test]
    fn process_rules_pointer_absent_from_worker_reviewer_and_verifier_lists() {
        let shapes: Vec<(&str, crate::model::RequestKind)> = vec![
            ("worker", crate::model::RequestKind::Worker),
            ("review", crate::model::RequestKind::Review),
            ("paper", crate::model::RequestKind::Paper),
            ("corr", crate::model::RequestKind::Corr),
            ("sound", crate::model::RequestKind::Sound),
        ];
        for (label, kind) in shapes {
            let mut request = WrapperRequest {
                kind,
                phase: Phase::TheoremStating,
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            for contract in [
                &request.worker_contract,
                &request.review_contract,
                &request.paper_contract,
                &request.corr_contract,
                &request.sound_contract,
            ] {
                if let Some(frags) = contract["prompt_fragments"].as_array() {
                    assert!(
                        !frags
                            .iter()
                            .any(|f| f.as_str() == Some(PROCESS_RULES_FRAGMENT)),
                        "{label} request must not carry the audit process-rules pointer"
                    );
                }
            }
        }
    }

    #[test]
    fn proof_formalization_review_omits_theorem_helper_policy() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = request.review_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<String> = frags
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect();
        assert!(
            !frag_strs
                .iter()
                .any(|f| f == "review/common/33c_theorem_helper_policy.md"),
            "ProofFormalization review should NOT include theorem helper policy; got: {frag_strs:?}"
        );
    }

    /// D7 merge. Every PV correspondence contract — either phase, any node
    /// role, including no role at all — carries the single merged PV
    /// correspondence rubric; the all-math contract never does. This
    /// replaces the role-gated pair (`09_spec_correspondence.md` for Spec,
    /// `10_model_correspondence.md` for Correctness/Safety): the role
    /// gating meant a Correctness/Safety corr verdict could be produced
    /// with no model-correspondence rubric in the prompt whenever no such
    /// node was on the frontier, and prose statement nodes carry no pinned
    /// role to key on at all.
    #[test]
    fn pv_corr_contract_always_carries_merged_pv_correspondence() {
        let corr_frags = |request: &WrapperRequest| -> Vec<String> {
            request.corr_contract["prompt_fragments"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        for phase in [Phase::TheoremStating, Phase::ProofFormalization] {
            for role in [
                None,
                Some(PvRole::Spec),
                Some(PvRole::Correctness),
                Some(PvRole::Safety),
            ] {
                let n = NodeId::from("Gcd");
                let mut node_role = BTreeMap::new();
                if let Some(role) = role {
                    node_role.insert(n.clone(), role);
                }
                let mut request = WrapperRequest {
                    kind: crate::model::RequestKind::Corr,
                    phase,
                    is_pv: true,
                    verify_nodes: BTreeSet::from([n.clone()]),
                    corr_verify_nodes: BTreeSet::from([n.clone()]),
                    node_role,
                    ..WrapperRequest::default()
                };
                populate_request_prompt_contracts(&mut request, None);
                let frags = corr_frags(&request);
                assert!(
                    frags
                        .iter()
                        .any(|f| f == "pv/verifier/correspondence/09_pv_correspondence.md"),
                    "PV corr ({phase:?}, role {:?}) must carry the merged PV \
                     correspondence rubric; got {frags:?}",
                    role
                );
                assert!(
                    !frags.iter().any(|f| f
                        == "pv/verifier/correspondence/09_spec_correspondence.md"
                        || f == "pv/verifier/correspondence/10_model_correspondence.md"),
                    "the deleted role-gated fragments must never be selected; got {frags:?}"
                );
            }
        }

        // Math counterpart: identical request shape, is_pv false.
        let n = NodeId::from("Gcd");
        let mut math = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::TheoremStating,
            verify_nodes: BTreeSet::from([n.clone()]),
            corr_verify_nodes: BTreeSet::from([n.clone()]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut math, None);
        let math_frags = corr_frags(&math);
        assert!(
            !math_frags
                .iter()
                .any(|f| f == "pv/verifier/correspondence/09_pv_correspondence.md"),
            "all-math corr must NOT carry the PV correspondence rubric; got {math_frags:?}"
        );
        assert!(
            math_frags
                .iter()
                .any(|f| f == "verifier/correspondence/05_frontier.md"),
            "all-math corr keeps the standard frontier fragment; got {math_frags:?}"
        );
    }

    #[test]
    fn under_model_assumptions_corr_contract_marks_axiom_correspondence() {
        let assumptions = NodeId::from("Assumptions");
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::ProofFormalization,
            is_pv: true,
            verify_nodes: BTreeSet::from([assumptions.clone()]),
            corr_verify_nodes: BTreeSet::from([assumptions.clone()]),
            node_role: BTreeMap::from([(
                assumptions.clone(),
                PvRole::UnderModelAssumptions,
            )]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(
            &mut request,
            Some(std::path::Path::new("/path/to/trellis/src/trellis")),
        );

        assert_eq!(
            request.corr_contract["request_summary"]["under_model_assumptions"],
            json!({
                "nodes": ["Assumptions"],
                "lean_form": "axiom",
                "correspondence": "lean_axiom_proposition_to_tex_statement",
                "lean_axiom_policy": UNDER_MODEL_VALIDITY_HOOK_POLICY,
            })
        );
        assert_eq!(
            request.corr_contract["request_summary"]["tooling_hints"],
            json!({
                "aeneas_lean_sources": "/path/to/trellis/pv_spike/tools/aeneas/backends/lean/Aeneas",
            })
        );
        assert_eq!(
            request.corr_contract["rubric"]["definition_hygiene_exception_roles"],
            json!(["under_model_assumptions"])
        );
        assert!(request.corr_contract["rubric"]["definition_hygiene"]
            .as_array()
            .expect("definition_hygiene")
            .contains(&json!("reject_axiom")));
    }

    #[test]
    fn under_model_assumptions_corr_fields_are_absent_outside_pv_role_case() {
        let assumptions = NodeId::from("Assumptions");
        let other = NodeId::from("Spec");
        let mut cases = vec![
            WrapperRequest {
                kind: crate::model::RequestKind::Corr,
                phase: Phase::ProofFormalization,
                verify_nodes: BTreeSet::from([assumptions.clone()]),
                corr_verify_nodes: BTreeSet::from([assumptions.clone()]),
                ..WrapperRequest::default()
            },
            WrapperRequest {
                kind: crate::model::RequestKind::Corr,
                phase: Phase::ProofFormalization,
                verify_nodes: BTreeSet::from([assumptions.clone()]),
                corr_verify_nodes: BTreeSet::from([assumptions.clone()]),
                node_role: BTreeMap::from([(
                    assumptions.clone(),
                    PvRole::UnderModelAssumptions,
                )]),
                ..WrapperRequest::default()
            },
            WrapperRequest {
                kind: crate::model::RequestKind::Corr,
                phase: Phase::ProofFormalization,
                is_pv: true,
                verify_nodes: BTreeSet::from([assumptions.clone()]),
                corr_verify_nodes: BTreeSet::from([assumptions.clone()]),
                node_role: BTreeMap::from([(assumptions.clone(), PvRole::Spec)]),
                ..WrapperRequest::default()
            },
            WrapperRequest {
                kind: crate::model::RequestKind::Corr,
                phase: Phase::ProofFormalization,
                is_pv: true,
                verify_nodes: BTreeSet::from([other.clone()]),
                corr_verify_nodes: BTreeSet::from([other.clone()]),
                node_role: BTreeMap::from([(
                    assumptions.clone(),
                    PvRole::UnderModelAssumptions,
                )]),
                ..WrapperRequest::default()
            },
        ];

        for request in &mut cases {
            populate_request_prompt_contracts(request, None);
            let request_summary = request.corr_contract["request_summary"]
                .as_object()
                .expect("request_summary");
            assert!(
                !request_summary.contains_key("under_model_assumptions"),
                "request_summary leaked under_model_assumptions: {request_summary:?}"
            );
            assert!(
                !request_summary.contains_key("tooling_hints"),
                "request_summary leaked tooling_hints: {request_summary:?}"
            );

            let rubric = request.corr_contract["rubric"]
                .as_object()
                .expect("rubric");
            assert!(
                !rubric.contains_key("definition_hygiene_exception_roles"),
                "rubric leaked definition_hygiene_exception_roles: {rubric:?}"
            );
        }
    }

    #[test]
    fn assumption_authoring_worker_contract_surfaces_validity_hook_policy() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            is_pv: true,
            assumption_authoring: Some(crate::model::AssumptionAuthoringContext {
                candidate_invariant: "every slice length is at most isize::MAX".into(),
                needed_by: vec!["goal:correct".into()],
                ..crate::model::AssumptionAuthoringContext::default()
            }),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let authoring_contract = &request.worker_contract["authored_assumption_contract"];
        let authoring_summary = &request.worker_contract["request_summary"]["assumption_authoring"];
        assert_eq!(
            authoring_contract["lean_axiom_policy"],
            json!(UNDER_MODEL_VALIDITY_HOOK_POLICY)
        );
        assert_eq!(
            authoring_contract["theorem_statement_policy"],
            json!(UNDER_MODEL_THEOREM_STATEMENT_POLICY)
        );
        assert_eq!(
            authoring_summary["lean_axiom_policy"],
            json!(UNDER_MODEL_VALIDITY_HOOK_POLICY)
        );
        assert_eq!(
            authoring_summary["theorem_statement_policy"],
            json!(UNDER_MODEL_THEOREM_STATEMENT_POLICY)
        );
    }

    #[test]
    fn assumptions_lane_contract_surfaces_hook_scope_rule() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::ProofFormalization,
            is_pv: true,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "assumptions lane".into(),
                assumptions_lane: Some(crate::model::AssumptionsLaneContext {
                    assumption_id: "rust_valid_property".into(),
                    candidate_invariant: "Rust-valid values satisfy property C".into(),
                    axiom_name: "Tablet.Assumptions.c".into(),
                    lean_statement: "axiom Tablet.Assumptions.c : True".into(),
                    nl_statement: "C holds for Rust-valid values.".into(),
                    citation_locator: "rust-docs locator".into(),
                    rust_justification: "Rust validity guarantees C.".into(),
                    needed_by: vec!["goal:correct".into()],
                    gate_from_invalid_attempt: false,
                    claim_class: String::new(),
                }),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);

        let fragments = contract_fragments(&request.stuck_math_audit_contract);
        assert!(fragments
            .iter()
            .any(|fragment| fragment == "pv/stuck_audit/08_assumptions_lane_role.md"));

        assert!(request.stuck_math_audit_contract["lane_posture"]
            .as_str()
            .expect("lane_posture")
            .contains("Quantified over-approximating container variables sit under their validity hook"));
        // The lane sees the claim class (normalized) and the probe field.
        assert_eq!(
            request.stuck_math_audit_contract["candidate_under_model_assumption"]["claim_class"],
            "behavior"
        );
        assert!(request.stuck_math_audit_contract["artifact_contract"]["prompt_schema_example"]
            ["assumptions_lane_probe_result"]
            .as_str()
            .expect("probe schema entry")
            .contains("unconditional form"));
        assert!(request.stuck_math_audit_contract["artifact_contract"]["prompt_schema_example"]
            ["assumptions_lane_reason"]
            .as_str()
            .expect("assumptions_lane_reason")
            .contains("hook-scope"));

    }

    #[test]
    fn under_model_adjudication_fragment_is_selected_for_pv() {
        let mut request = WrapperRequest {
                kind: crate::model::RequestKind::StuckMathAudit,
                phase: Phase::TheoremStating,
                is_pv: true,
                stuck_math_audit: crate::model::StuckMathAuditState {
                    active: true,
                    trigger: "under-model adjudication".into(),
                    need_input_audit: Some(crate::model::NeedInputAuditContext {
                        under_model_disproof: "bound target is false in the model".into(),
                        ..crate::model::NeedInputAuditContext::default()
                    }),
                    ..crate::model::StuckMathAuditState::default()
                },
                ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let fragments = contract_fragments(&request.stuck_math_audit_contract);
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment == "pv/stuck_audit/07_under_model_adjudication.md"),
            "PV must receive the adjudication rules: {fragments:?}"
        );
    }

    /// PV worker critics are keyed on the ACTIVE node's `node_role`. Each PV
    /// proof/spec role pulls its critic (plus the shared proof-method strategy
    /// for the proof roles); the byte-pinned / verifier-critic / library roles
    /// pull only the PV intro. An empty `node_role` (all-math) pulls no PV
    /// fragment, so the worker contract bytes are identical.
    #[test]
    fn pv_active_node_role_pulls_its_worker_critic() {
        let active = NodeId::from("Active");
        let worker_request = |role: Option<PvRole>, phase: Phase| {
            let (worker_profile, validation_kind) = if phase.is_theorem_stating_like() {
                (WorkerProfile::Theorem, WorkerValidationKind::TheoremTargeted)
            } else {
                (WorkerProfile::ProofEasy, WorkerValidationKind::ProofLocal)
            };
            let mut request = WrapperRequest {
                kind: crate::model::RequestKind::Worker,
                phase,
                is_pv: role.is_some(),
                active_node: Some(active.clone()),
                worker_context: crate::model::WorkerContext {
                    worker_profile,
                    validation_kind,
                    ..crate::model::WorkerContext::default()
                },
                ..WrapperRequest::default()
            };
            if let Some(role) = role {
                request.node_role = BTreeMap::from([(active.clone(), role)]);
            }
            populate_request_prompt_contracts(&mut request, None);
            request.worker_contract["prompt_fragments"]
                .as_array()
                .expect("worker prompt fragments")
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<String>>()
        };

        let cases = [
            (
                PvRole::Spec,
                Phase::TheoremStating,
                vec!["pv/worker/theorem_stating/10_spec_authoring.md"],
            ),
            (
                PvRole::Safety,
                Phase::ProofFormalization,
                vec![
                    "pv/worker/proof_formalization/20_safety.md",
                    "pv/worker/proof_formalization/50_proof_method.md",
                ],
            ),
            (
                PvRole::Invariant,
                Phase::ProofFormalization,
                vec![
                    "pv/worker/proof_formalization/30_invariant.md",
                    "pv/worker/proof_formalization/50_proof_method.md",
                ],
            ),
            (
                PvRole::Correctness,
                Phase::ProofFormalization,
                vec![
                    "pv/worker/proof_formalization/40_correctness.md",
                    "pv/worker/proof_formalization/50_proof_method.md",
                ],
            ),
        ];
        for (role, phase, expected) in cases {
            let frags = worker_request(Some(role), phase);
            assert!(
                frags.iter().any(|f| f == "pv/worker/common/00_intro.md"),
                "{role:?} active node must carry the PV intro; got {frags:?}"
            );
            for want in expected {
                assert!(
                    frags.iter().any(|f| f == want),
                    "{role:?} active node must carry {want}; got {frags:?}"
                );
            }
        }

        // Byte-pinned / verifier-critic / library roles: PV intro only, no
        // role critic.
        for role in [
            PvRole::ExtractionModel,
            PvRole::ExternalModel,
            PvRole::LibraryLemma,
        ] {
            let frags = worker_request(Some(role), Phase::ProofFormalization);
            assert!(
                frags.iter().any(|f| f == "pv/worker/common/00_intro.md"),
                "{role:?} active node still carries the PV intro; got {frags:?}"
            );
            assert!(
                !frags.iter().any(|f| f.starts_with("pv/worker/proof_formalization/")
                    || f.starts_with("pv/worker/theorem_stating/")),
                "{role:?} active node must carry no PV role critic; got {frags:?}"
            );
        }

        // All-math: empty node_role pulls no PV worker fragment at all.
        let math_frags = worker_request(None, Phase::ProofFormalization);
        assert!(
            !math_frags.iter().any(|f| f.starts_with("pv/worker/")),
            "all-math worker must carry no PV fragment; got {math_frags:?}"
        );
    }

    /// DEAD-PATH test. The Spec-Critic substantiveness push is dead code (Spec
    /// role only lands on substantiveness-waived pinned challenge targets, never
    /// on the substantiveness frontier) and the fragment is deleted. Even with a
    /// Spec node directly on the substantiveness frontier — the shape the live
    /// guard checks — the critic fragment must NOT be pushed.
    #[test]
    fn spec_role_does_not_push_dead_spec_critic_fragment() {
        let spec = NodeId::from("Spec");
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            substantiveness_verify_nodes: BTreeSet::from([spec.clone()]),
            node_role: BTreeMap::from([(spec.clone(), PvRole::Spec)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags: Vec<&str> = request.paper_contract["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            !frags.contains(&"pv/verifier/substantiveness/16_spec_critic.md"),
            "Spec-Critic fragment is dead code and must NOT be pushed; got {frags:?}"
        );

        // Math counterpart: identical request shape, empty node_role.
        let mut math = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            substantiveness_verify_nodes: BTreeSet::from([spec.clone()]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut math, None);
        let math_frags: Vec<&str> = math.paper_contract["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            !math_frags.contains(&"pv/verifier/substantiveness/16_spec_critic.md"),
            "all-math substantiveness must NOT carry the Spec-Critic fragment; got {math_frags:?}"
        );
        assert!(
            math_frags.contains(&"verifier/substantiveness/05_fresh_node_frontier.md"),
            "all-math substantiveness keeps the standard node-frontier fragment; got {math_frags:?}"
        );
    }

    /// DEAD-PATH test. The External-Model-Critic pushes (correspondence and
    /// substantiveness) are dead code — `PvRole::ExternalModel` is never assigned
    /// in production and both fragments are deleted. Even with an ExternalModel
    /// node directly on each frontier — the shape the live guards check — neither
    /// critic fragment must be pushed.
    #[test]
    fn external_model_role_does_not_push_dead_critic_fragments() {
        let n = NodeId::from("ExtModel");
        let mut corr = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::TheoremStating,
            verify_nodes: BTreeSet::from([n.clone()]),
            corr_verify_nodes: BTreeSet::from([n.clone()]),
            node_role: BTreeMap::from([(n.clone(), PvRole::ExternalModel)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut corr, None);
        let corr_frags: Vec<&str> = corr.corr_contract["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            !corr_frags.contains(&"pv/verifier/correspondence/11_external_model_critic.md"),
            "External-Model-Critic corr fragment is dead code and must NOT be pushed; got {corr_frags:?}"
        );

        let mut subst = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            substantiveness_verify_nodes: BTreeSet::from([n.clone()]),
            node_role: BTreeMap::from([(n.clone(), PvRole::ExternalModel)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut subst, None);
        let subst_frags: Vec<&str> = subst.paper_contract["prompt_fragments"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            !subst_frags.contains(&"pv/verifier/substantiveness/17_external_model_critic.md"),
            "External-Model-Critic substantiveness fragment is dead code and must NOT be pushed; got {subst_frags:?}"
        );
    }

    #[test]
    fn paper_contract_renders_deviation_authorization_scenario() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            deviation_verify_id: Some(crate::model::DeviationId::from("const_loss")),
            deviation_verify_path: "reference/deviations/const_loss.tex".into(),
            verify_lanes: BTreeSet::from(["paper".to_string()]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        assert_eq!(
            request.paper_contract["request_summary"]["scenario"],
            "deviation_authorization"
        );
        let frags = request.paper_contract["prompt_fragments"]
            .as_array()
            .unwrap();
        let frag_strs: Vec<&str> = frags.iter().filter_map(|v| v.as_str()).collect();
        assert!(frag_strs.contains(&"verifier/deviation/05_single_file.md"));
        assert!(frag_strs.contains(&"canonical/DEVIATIONS.md"));
        assert_eq!(
            request.paper_contract["artifact_contract"]["result_type"],
            "deviation_authorization_result_v1"
        );
    }

    #[test]
    fn post_advance_routing_review_emits_dedicated_primary_fragment() {
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 1,
            phase: Phase::ProofFormalization,
            post_advance_routing: true,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_post_advance_routing.md",
            "post_advance_routing=true must select the routing primary fragment, \
             overriding the retry_outcome_kind / blocker chain"
        );
    }

    #[test]
    fn non_routing_review_still_uses_legacy_primary_fragment() {
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 1,
            phase: Phase::ProofFormalization,
            post_advance_routing: false,
            ..WrapperRequest::default()
        };
        // Default RetryOutcomeKind::None + no blockers → clean fragment.
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_clean_verification.md",
        );
    }

    #[test]
    fn corr_blocker_adjudicable_selects_failed_correspondence_fragment() {
        // A NodeCorr blocker the reviewer was handed real Fail evidence for
        // (corr_blocker_adjudicable=true) keeps the failed-correspondence
        // framing.
        let n = NodeId::from("FullEdgeCleanCollarFiniteLabelTransport");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 155,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::NodeCorr,
                object: BlockerObject::Node { node: n },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            corr_blocker_adjudicable: true,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_failed_correspondence.md",
            "adjudicable NodeCorr Fail must keep the failed-correspondence framing"
        );
    }

    #[test]
    fn corr_blocker_unknown_selects_unverified_correspondence_fragment() {
        // A NodeCorr blocker that is Unknown / not-yet-verified
        // (corr_blocker_adjudicable=false, no split evidence) must NOT be
        // framed as a failed correspondence — there is no failure on record.
        // This is the cycle-155 halt shape.
        let n = NodeId::from("FullEdgeCleanCollarFiniteLabelTransport");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 155,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::NodeCorr,
                object: BlockerObject::Node { node: n },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            corr_blocker_adjudicable: false,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_unverified_correspondence.md",
            "Unknown / not-yet-verified NodeCorr blocker must be framed for \
             re-verification, not as a failed correspondence"
        );
    }

    fn substantiveness_fail_request(phase: Phase) -> WrapperRequest {
        let n = NodeId::from("FullEdgeCleanCollarFiniteLabelTransport");
        WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 160,
            phase,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::Substantiveness,
                object: BlockerObject::Node { node: n },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            substantiveness_blocker_adjudicable: true,
            ..WrapperRequest::default()
        }
    }

    #[test]
    fn substantiveness_blocker_adjudicable_selects_failed_fragment_theorem_phase() {
        // Theorem/revision phase: every failed blocker kind is resettable, so
        // the shared reset-capable fragment is correct.
        let request = substantiveness_fail_request(Phase::TheoremStating);
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_failed_substantiveness.md",
            "theorem-phase substantiveness Fail keeps the shared framing"
        );
    }

    #[test]
    fn substantiveness_blocker_adjudicable_selects_proof_phase_fragment() {
        // Proof phase keeps its own framing: worker `.tex` repair first, with
        // the Substantiveness-only reset for a stale verdict.
        let request = substantiveness_fail_request(Phase::ProofFormalization);
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_proof_phase_substantiveness.md",
            "proof-phase substantiveness Fail selects the proof-phase framing"
        );
    }

    #[test]
    fn substantiveness_blocker_unknown_selects_unverified_fragment() {
        let n = NodeId::from("FullEdgeCleanCollarFiniteLabelTransport");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 160,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::Substantiveness,
                object: BlockerObject::Node { node: n },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            substantiveness_blocker_adjudicable: false,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_unverified_substantiveness.md",
            "Unknown / not-yet-verified substantiveness blocker must be framed for \
             re-verification, not as a failed substantiveness"
        );
    }

    #[test]
    fn deviation_blocker_adjudicable_selects_failed_fragment() {
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 160,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::Deviation,
                object: BlockerObject::Deviation {
                    deviation: crate::model::DeviationId::from("dev-1"),
                },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            deviation_blocker_adjudicable: true,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_failed_deviation.md",
            "a real deviation Fail must keep the failed-deviation framing"
        );
    }

    #[test]
    fn deviation_blocker_unknown_selects_unverified_fragment() {
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 160,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::Deviation,
                object: BlockerObject::Deviation {
                    deviation: crate::model::DeviationId::from("dev-1"),
                },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            deviation_blocker_adjudicable: false,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_unverified_deviation.md",
            "Unknown / not-yet-verified deviation blocker must be framed for \
             re-verification, not as a failed deviation"
        );
    }

    #[test]
    fn sound_blocker_adjudicable_selects_failed_fragment() {
        let n = NodeId::from("FullEdgeCleanCollarFiniteLabelTransport");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 160,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::Soundness,
                object: BlockerObject::Node { node: n },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            sound_blocker_adjudicable: true,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_failed_soundness.md",
            "a real soundness Fail must keep the failed-soundness framing"
        );
    }

    #[test]
    fn sound_blocker_unknown_selects_unverified_fragment() {
        let n = NodeId::from("FullEdgeCleanCollarFiniteLabelTransport");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 160,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::Soundness,
                object: BlockerObject::Node { node: n },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            sound_blocker_adjudicable: false,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_unverified_soundness.md",
            "Unknown / not-yet-verified soundness blocker must be framed for \
             re-verification, not as a failed soundness"
        );
    }

    #[test]
    fn paper_blocker_adjudicable_selects_failed_fragment() {
        // A real paper Fail (e.g. a definite empty-coverage Fail, surfaced via
        // paper_blocker_adjudicable=true) keeps the failed-paper framing.
        let t = TargetId::from("MainTheoremTarget");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 160,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::PaperFaithfulness,
                object: BlockerObject::Target { target: t },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            paper_blocker_adjudicable: true,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_failed_paper_faithfulness.md",
            "a real paper Fail (including empty-coverage) must keep the failed-paper framing"
        );
    }

    #[test]
    fn paper_blocker_unknown_selects_unverified_fragment() {
        let t = TargetId::from("MainTheoremTarget");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Review,
            cycle: 160,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::PaperFaithfulness,
                object: BlockerObject::Target { target: t },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            paper_blocker_adjudicable: false,
            ..WrapperRequest::default()
        };
        assert_eq!(
            review_primary_scenario_prompt_fragment(&request),
            "review/common/05_after_unverified_paper_faithfulness.md",
            "Unknown / not-yet-verified paper blocker must be framed for \
             re-verification, not as a failed paper-faithfulness"
        );
    }

    #[test]
    fn soundness_contract_emits_reverification_fragment_when_context_present() {
        let target = NodeId::from("SomeTargetNode");
        let request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Sound,
            cycle: 178,
            phase: Phase::ProofFormalization,
            sound_verify_node: Some(target.clone()),
            sound_verify_nodes: BTreeSet::from([target.clone()]),
            sound_reverification_context: Some(crate::model::SoundReverificationContext {
                target: target.clone(),
                prior_status: crate::model::SoundAssessmentStatus::VerifierPass,
                current_status: crate::model::SoundAssessmentStatus::DepEditOnlyStalePassDeferred,
                own_tex_changed: false,
                deps_changed: vec![crate::model::SoundDepHashDriftEntry {
                    dep: NodeId::from("SomeDepNode"),
                    prior_hash: "abc123def456".to_string(),
                    current_hash: "fedcba654321".to_string(),
                }],
                prior_lane_evidence: BTreeMap::new(),
            }),
            ..WrapperRequest::default()
        };
        let payload = soundness_contract_payload(&request, None);
        let fragments: Vec<&str> = payload["prompt_fragments"]
            .as_array()
            .expect("prompt_fragments array")
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        assert!(
            fragments.contains(&"verifier/common/15a_reverification_context.md"),
            "expected reverification fragment to be emitted; got {:?}",
            fragments,
        );
        // The reverification_context block must be present and carry
        // the dep-drift entry verbatim (truncated hashes flow through
        // the request struct, not this layer).
        let reverif = &payload["reverification_context"];
        assert_eq!(reverif["target"], json!("SomeTargetNode"));
        assert_eq!(reverif["own_tex_changed"], json!(false));
        assert_eq!(
            reverif["current_status"],
            json!("DepEditOnlyStalePassDeferred"),
        );
        assert_eq!(reverif["prior_status"], json!("VerifierPass"));
        let deps = reverif["deps_changed"]
            .as_array()
            .expect("deps_changed array");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0]["dep"], json!("SomeDepNode"),);
        assert_eq!(deps[0]["prior_hash"], json!("abc123def456"));
        assert_eq!(deps[0]["current_hash"], json!("fedcba654321"));
        assert!(
            reverif["git_access_hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("git -C")),
            "git_access_hint should mention git invocation",
        );
    }

    #[test]
    fn soundness_contract_omits_reverification_fragment_when_context_absent() {
        let target = NodeId::from("FreshUnknownNode");
        let request = WrapperRequest {
            id: 2,
            kind: crate::model::RequestKind::Sound,
            cycle: 5,
            phase: Phase::ProofFormalization,
            sound_verify_node: Some(target.clone()),
            sound_verify_nodes: BTreeSet::from([target.clone()]),
            // sound_reverification_context omitted -> None (default)
            ..WrapperRequest::default()
        };
        let payload = soundness_contract_payload(&request, None);
        let fragments: Vec<&str> = payload["prompt_fragments"]
            .as_array()
            .expect("prompt_fragments array")
            .iter()
            .map(|item| item.as_str().expect("fragment string"))
            .collect();
        assert!(
            !fragments.contains(&"verifier/common/15a_reverification_context.md"),
            "reverification fragment must NOT be emitted for fresh-Unknown target; got {:?}",
            fragments,
        );
        assert!(
            payload["reverification_context"].is_null(),
            "reverification_context payload must be null when no context exists",
        );
    }

    #[test]
    fn dep_statement_hash_diff_helper_handles_added_removed_and_changed() {
        use crate::model::{dep_statement_hash_diff, truncate_fingerprint_for_display};
        let stored = BTreeMap::from([
            (NodeId::from("A"), "aaaaaaaaaaaa1111".to_string()),
            (NodeId::from("B"), "bbbbbbbbbbbb2222".to_string()),
            // C is absent (will be added)
            (NodeId::from("D"), "dddddddddddd4444".to_string()), // unchanged
        ]);
        let current = BTreeMap::from([
            (NodeId::from("A"), "aaaaaaaaaaaa1111".to_string()), // unchanged
            // B removed
            (NodeId::from("C"), "cccccccccccc3333".to_string()), // added
            (NodeId::from("D"), "dddddddddddd4444".to_string()), // unchanged
        ]);
        let diff = dep_statement_hash_diff(&stored, &current);
        // Sorted by NodeId: B (removed), C (added).
        assert_eq!(diff.len(), 2);
        assert_eq!(diff[0].dep, NodeId::from("B"));
        assert_eq!(diff[0].prior_hash, "bbbbbbbbbbbb\u{2026}");
        assert_eq!(diff[0].current_hash, "(absent)");
        assert_eq!(diff[1].dep, NodeId::from("C"));
        assert_eq!(diff[1].prior_hash, "(absent)");
        assert_eq!(diff[1].current_hash, "cccccccccccc\u{2026}");
        // Bounded display: full hashes are truncated to 12 chars + ellipsis.
        assert_eq!(
            truncate_fingerprint_for_display(&"0123456789abcdef".to_string()),
            "0123456789ab\u{2026}",
        );
        // Short hashes pass through unchanged.
        assert_eq!(
            truncate_fingerprint_for_display(&"short".to_string()),
            "short",
        );
        assert_eq!(
            truncate_fingerprint_for_display(&"".to_string()),
            "(absent)",
        );
    }

    #[test]
    fn worker_blocker_status_block_names_placeholder_definition_rule() {
        let n = NodeId::from("Foo");
        let request = WrapperRequest {
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::NodeCorr,
                object: BlockerObject::Node { node: n.clone() },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            placeholder_definition_nodes: BTreeSet::from([n.clone()]),
            ..WrapperRequest::default()
        };
        let block = worker_blocker_status_block(&request);
        assert!(
            block
                .md
                .contains("Placeholder-definition correspondence auto-fail"),
            "placeholder note must name the rule; got:\n{}",
            block.md
        );
        assert!(block.md.contains("Foo"));
        assert!(block.md.contains("`True`"));
    }

    #[test]
    fn worker_blocker_status_block_omits_note_without_placeholder() {
        // No placeholder definitions => no footnote (byte-identical to the
        // pre-placeholder rendering).
        let n = NodeId::from("Foo");
        let request = WrapperRequest {
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::NodeCorr,
                object: BlockerObject::Node { node: n },
                fingerprint: "fp".to_string(),
                deferred: false,
            }]),
            ..WrapperRequest::default()
        };
        let block = worker_blocker_status_block(&request);
        assert!(!block
            .md
            .contains("Placeholder-definition correspondence auto-fail"));
    }

    // ── PV prompt-tree migration (§C test plan) ──────────────────────────────

    fn contract_fragments(contract: &Value) -> Vec<String> {
        contract["prompt_fragments"]
            .as_array()
            .expect("prompt_fragments array")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    }

    /// True iff any fragment file rendered into this contract contains a literal
    /// `GOAL.md` token whose surrounding text is not a disclaimer. Resolves
    /// fragment paths against the on-disk `trellis/prompt_fragments/` tree.
    fn contract_text_mentions_goal_md(contract: &Value) -> bool {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../trellis/prompt_fragments");
        for frag in contract_fragments(contract) {
            let path = base.join(&frag);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            for line in text.lines() {
                if line.contains("GOAL.md")
                    && !line.contains("no `GOAL.md`")
                    && !line.contains("there is no GOAL.md")
                {
                    return true;
                }
            }
        }
        false
    }

    /// §C.2 — the motivating-bug regression. A PV `Worker` request at
    /// theorem-stating cycle 1 (only ExtractionModel defs present, no goal
    /// theorems yet) selects the PV worker scheme / source-of-truth /
    /// decomposition fragments and NONE of the math/paper ones. `is_pv` is true
    /// on cycle 1 (the seeded bool), the §0.4 requirement.
    #[test]
    fn pv_worker_theorem_stating_cycle1_selects_pv_fragments() {
        let spec = NodeId::from("Spec");
        let mut request = WrapperRequest {
            id: 1,
            kind: crate::model::RequestKind::Worker,
            cycle: 1,
            phase: Phase::TheoremStating,
            is_pv: true,
            active_node: Some(spec.clone()),
            node_role: BTreeMap::from([(spec, PvRole::Spec)]),
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Theorem,
                validation_kind: WorkerValidationKind::TheoremGlobal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = contract_fragments(&request.worker_contract);
        for want in [
            "pv/common/TRELLIS_FORMALIZATION_SCHEME.md",
            "pv/common/18_source_of_truth.md",
            "pv/worker/common/00_intro.md",
            "pv/worker/theorem_stating/05_frontier_work.md",
            "pv/worker/theorem_stating/10_spec_authoring.md",
            "pv/worker/theorem_stating/12_first_request_decomposition.md",
            "pv/canonical/SUBSTANTIVENESS.md",
            "pv/canonical/SOUNDNESS.md",
        ] {
            assert!(frags.iter().any(|fragment| fragment == want),
                "PV prose worker must carry {want}; got {frags:?}");
        }
        for forbidden in [
            "common/TRELLIS_FORMALIZATION_SCHEME.md",
            "worker/common/18_reference_paper.md",
            "canonical/FAITHFULNESS.md",
            "worker/theorem_stating/10_spec_proving.md",
        ] {
            assert!(!frags.iter().any(|fragment| fragment == forbidden),
                "PV prose worker must not carry {forbidden}; got {frags:?}");
        }
        assert!(contract_text_mentions_goal_md(&request.worker_contract));
    }

    #[test]
    fn theorem_stating_worker_never_receives_pv_formalization_overlay() {
        let roles = [
            PvRole::Spec,
            PvRole::ExtractionModel,
            PvRole::ExternalModel,
            PvRole::Invariant,
            PvRole::Safety,
            PvRole::Correctness,
            PvRole::LibraryLemma,
            PvRole::UnderModelAssumptions,
        ];
        let decide_targets: BTreeMap<_, _> =
            decide_pair_specs("goal", "PinnedGoal").into_iter().collect();

        let blocker_kinds = [
            BlockerKind::PaperFaithfulness,
            BlockerKind::Deviation,
            BlockerKind::NodeCorr,
            BlockerKind::Soundness,
            BlockerKind::Substantiveness,
            BlockerKind::ChallengeCoverage,
        ];

        for role in roles {
            for blocker_mask in 0..(1_u32 << blocker_kinds.len()) {
                let active = NodeId::from(format!("Active{role:?}"));
                let blockers = blocker_kinds
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| blocker_mask & (1 << index) != 0)
                    .map(|(_, kind)| Blocker {
                        kind: *kind,
                        object: crate::model::BlockerObject::Node {
                            node: active.clone(),
                        },
                        fingerprint: format!("{kind:?}-fp"),
                        deferred: false,
                    })
                    .collect();
                let request = WrapperRequest {
                    kind: crate::model::RequestKind::Worker,
                    phase: Phase::TheoremStating,
                    is_pv: true,
                    active_node: Some(active.clone()),
                    blockers,
                    configured_challenge_targets: decide_targets.clone(),
                    node_role: BTreeMap::from([(active, role)]),
                    worker_context: crate::model::WorkerContext {
                        worker_profile: WorkerProfile::Theorem,
                        validation_kind: WorkerValidationKind::TheoremTargeted,
                        ..crate::model::WorkerContext::default()
                    },
                    ..WrapperRequest::default()
                };
                let fragments = worker_prompt_fragments(&request, crate::backend::BackendId::Lean);
                assert!(
                    fragments
                        .iter()
                        .all(|fragment| !fragment.contains("proof_formalization/")),
                    "TheoremStating role {role:?} with blocker mask {blocker_mask:#08b} received a formalization overlay: {fragments:?}"
                );
            }
        }

        let formalization_spec = NodeId::from("FormalizationSpec");
        let formalization_spec_request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            is_pv: true,
            active_node: Some(formalization_spec.clone()),
            node_role: BTreeMap::from([(formalization_spec, PvRole::Spec)]),
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        let formalization_spec_fragments = worker_prompt_fragments(&formalization_spec_request, crate::backend::BackendId::Lean);
        assert!(
            formalization_spec_fragments
                .iter()
                .all(|fragment| !fragment.contains("pv/worker/theorem_stating/10_spec_")),
            "ProofFormalization Spec worker received a theorem-stating-only Spec overlay: {formalization_spec_fragments:?}"
        );

        let refutation = NodeId::from("PinnedGoal__Refutation");
        let refutation_request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            is_pv: true,
            active_node: Some(refutation.clone()),
            configured_challenge_targets: decide_targets.clone(),
            node_role: BTreeMap::from([(refutation, PvRole::Spec)]),
            ..WrapperRequest::default()
        };
        let fragments = worker_prompt_fragments(&refutation_request, crate::backend::BackendId::Lean);
        assert!(
            fragments
                .iter()
                .all(|fragment| !fragment.contains("proof_formalization/")),
            "TheoremStating refutation received a formalization overlay: {fragments:?}"
        );
        assert!(
            fragments
                .contains(&"pv/worker/theorem_stating/60_disprove_direction.md"),
            "TheoremStating refutation must receive its phase-specific disprove guidance: {fragments:?}"
        );

        let trust_request = WrapperRequest {
            trust_base_required_v1: true,
            ..refutation_request
        };
        let trust_fragments = worker_prompt_fragments(&trust_request, crate::backend::BackendId::Lean);
        assert!(
            trust_fragments
                .iter()
                .all(|fragment| !fragment.contains("proof_formalization/")),
            "TheoremStating trust refutation received a formalization overlay: {trust_fragments:?}"
        );
        assert!(
            trust_fragments
                .iter()
                .any(|fragment| fragment.ends_with("72_side_by_side_probes_trust_v1.md")),
            "phase-neutral side-by-side guidance must remain available: {trust_fragments:?}"
        );

        let target_false_request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            is_pv: true,
            trust_base_required_v1: true,
            configured_challenge_targets: decide_targets,
            ..WrapperRequest::default()
        };
        let target_false_fragments = worker_prompt_fragments(&target_false_request, crate::backend::BackendId::Lean);
        for phase_neutral in [
            "71_target_false_trust_v1.md",
            "72_side_by_side_probes_trust_v1.md",
        ] {
            assert!(
                target_false_fragments
                    .iter()
                    .any(|fragment| fragment.ends_with(phase_neutral)),
                "phase-neutral {phase_neutral} guidance must remain available: {target_false_fragments:?}"
            );
        }

        let assumption_request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            is_pv: true,
            assumption_authoring: Some(crate::model::AssumptionAuthoringContext::default()),
            ..WrapperRequest::default()
        };
        let assumption_fragments = worker_prompt_fragments(&assumption_request, crate::backend::BackendId::Lean);
        assert!(
            assumption_fragments
                .iter()
                .all(|fragment| !fragment.contains("proof_formalization/")),
            "TheoremStating assumption burst received a formalization overlay: {assumption_fragments:?}"
        );
        assert!(
            assumption_fragments
                .contains(&"pv/worker/common/71_author_assumption.md"),
            "TheoremStating assumption burst must receive phase-neutral authoring guidance: {assumption_fragments:?}"
        );

        let prose_assumption_request = WrapperRequest {
            ..assumption_request.clone()
        };
        let prose_assumption_fragments = worker_prompt_fragments(
            &prose_assumption_request,
            crate::backend::BackendId::Lean,
        );
        assert!(
            prose_assumption_fragments
                .contains(&"pv/worker/common/71_author_assumption.md"),
            "prose assumption bursts must receive run-authored-statement guidance: {prose_assumption_fragments:?}"
        );
        let formalization_assumption_request = WrapperRequest {
            phase: Phase::ProofFormalization,
            ..assumption_request
        };
        let formalization_assumption_fragments =
            worker_prompt_fragments(&formalization_assumption_request, crate::backend::BackendId::Lean);
        assert!(
            formalization_assumption_fragments
                .contains(&"pv/worker/common/71_author_assumption.md"),
            "ProofFormalization assumption burst must receive the same phase-neutral authoring guidance: {formalization_assumption_fragments:?}"
        );
    }

    #[test]
    fn nodecorr_correspondence_overlay_is_proof_formalization_only() {
        let corr_blocker = crate::model::Blocker {
            kind: BlockerKind::NodeCorr,
            object: crate::model::BlockerObject::Node {
                node: NodeId::from("CorrHelper"),
            },
            fingerprint: "corr-fp".to_string(),
            deferred: false,
        };
        let theorem_corr_request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            is_pv: true,
            blockers: BTreeSet::from([corr_blocker]),
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofHard,
                validation_kind: WorkerValidationKind::ProofLocal,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        let theorem_corr_fragments = worker_prompt_fragments(&theorem_corr_request, crate::backend::BackendId::Lean);
        assert!(
            !theorem_corr_fragments
                .contains(&"pv/worker/proof_formalization/05_after_correspondence_review.md"),
            "TheoremStating NodeCorr worker received the proof-formalization correspondence overlay: {theorem_corr_fragments:?}"
        );

        let formalization_corr_request = WrapperRequest {
            phase: Phase::ProofFormalization,
            ..theorem_corr_request
        };
        let formalization_corr_fragments = worker_prompt_fragments(&formalization_corr_request, crate::backend::BackendId::Lean);
        assert!(
            formalization_corr_fragments
                .contains(&"pv/worker/proof_formalization/05_after_correspondence_review.md"),
            "ProofFormalization NodeCorr worker must receive its phase-specific correspondence overlay: {formalization_corr_fragments:?}"
        );
    }

    #[test]
    fn math_worker_request_receives_no_pv_fragment() {
        let active = NodeId::from("ImpossiblePvRoleCarrier");
        let request = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::TheoremStating,
            is_pv: false,
            active_node: Some(active.clone()),
            assumption_authoring: Some(crate::model::AssumptionAuthoringContext::default()),
            node_role: BTreeMap::from([(active, PvRole::Safety)]),
            ..WrapperRequest::default()
        };

        assert!(
            pv_worker_prompt_fragments(&request).is_empty(),
            "the PV selector must be structurally empty for math mode"
        );
        let fragments = worker_prompt_fragments(&request, crate::backend::BackendId::Lean);
        assert!(
            fragments.iter().all(|fragment| !fragment.starts_with("pv/")),
            "math-mode worker received a PV fragment: {fragments:?}"
        );
    }

    /// §C.3 — PV reviewer overlay assertion.
    #[test]
    fn pv_reviewer_selects_pv_fragments() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            is_pv: true,
            node_role: BTreeMap::from([(NodeId::from("Spec"), PvRole::Spec)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = contract_fragments(&request.review_contract);
        for want in [
            "pv/review/27_source_of_truth.md",
            "pv/review/33b_theorem_target_orientation.md",
            "pv/review/25_verifier_reasoning.md",
            "pv/review/31_need_input.md",
            "pv/review/33_routing_hints.md",
            "pv/canonical/SUBSTANTIVENESS.md",
            "pv/canonical/SOUNDNESS.md",
            "pv/shared/91_structured_request_pointer.md",
            "pv/review/30a_blocker_actions.md",
        ] {
            assert!(frags.iter().any(|fragment| fragment == want),
                "PV prose reviewer must carry {want}; got {frags:?}");
        }
        for forbidden in [
            "review/common/27_reference_paper.md",
            "canonical/FAITHFULNESS.md",
            "review/common/33_routing_hints.md",
        ] {
            assert!(!frags.iter().any(|fragment| fragment == forbidden),
                "PV prose reviewer must not carry {forbidden}; got {frags:?}");
        }
        assert!(contract_text_mentions_goal_md(&request.review_contract));
    }

    /// PV-substitutive forks of the shared/review/stuck-audit common fragments
    /// that the holistic prompt read flagged as all-math paper leaks: C1
    /// (`shared/91`), the reviewer Sound-gate (`30a`), the ProofFormalization
    /// `36_authorized_nodes` + `33d_challenge_targets` paper-coverage analogies,
    /// the correspondence host-`lake env` scratchpad, and the stuck-audit
    /// `03/04/05` paper leaks. Each PV variant is selected and the math original
    /// is absent.
    #[test]
    fn pv_shared_review_audit_forks_are_selected() {
        let mut review = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::ProofFormalization,
            is_pv: true,
            configured_challenge_targets: BTreeMap::from([(
                crate::ChallengeTargetId::from("target"),
                crate::ChallengeTargetSpec::default(),
            )]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut review, None);
        let review_fragments = contract_fragments(&review.review_contract);
        for want in [
            "pv/review/36_authorized_nodes.md",
            "pv/review/33d_challenge_targets.md",
            "pv/review/30a_blocker_actions.md",
            "pv/review/33_routing_hints.md",
            "pv/shared/91_structured_request_pointer.md",
        ] {
            assert!(review_fragments.iter().any(|fragment| fragment == want),
                "PV reviewer must carry {want}; got {review_fragments:?}");
        }

        let spec = NodeId::from("Spec");
        let mut corr = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::TheoremStating,
            is_pv: true,
            verify_nodes: BTreeSet::from([spec.clone()]),
            corr_verify_nodes: BTreeSet::from([spec.clone()]),
            node_role: BTreeMap::from([(spec, PvRole::Spec)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut corr, None);
        let corr_fragments = contract_fragments(&corr.corr_contract);
        assert!(corr_fragments.iter().any(|fragment|
            fragment == "pv/verifier/correspondence/07_scratchpad.md"));
        assert!(!corr_fragments.iter().any(|fragment|
            fragment == "verifier/correspondence/07_scratchpad.md"));

        let mut audit = WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::ProofFormalization,
            is_pv: true,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "stuck".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut audit, None);
        let audit_fragments = contract_fragments(&audit.stuck_math_audit_contract);
        for want in [
            "pv/stuck_audit/03_history_access.md",
            "pv/stuck_audit/04_scratchpad.md",
            "pv/stuck_audit/05_output_contract.md",
            "pv/stuck_audit/01_role.md",
            "pv/stuck_audit/02_source_of_truth.md",
        ] {
            assert!(audit_fragments.iter().any(|fragment| fragment == want),
                "PV audit must carry {want}; got {audit_fragments:?}");
        }
        assert!(contract_text_mentions_goal_md(&audit.stuck_math_audit_contract));
    }

    /// §C.4 — the newly-wired PV soundness lane.
    #[test]
    fn pv_soundness_selects_pv_floor_and_def() {
        let mut request = WrapperRequest {
            kind: crate::model::RequestKind::Sound,
            phase: Phase::TheoremStating,
            is_pv: true,
            node_role: BTreeMap::from([(NodeId::from("Spec"), PvRole::Spec)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let frags = contract_fragments(&request.sound_contract);
        assert!(frags.iter().any(|fragment|
            fragment == "pv/verifier/soundness/05_pv_floor.md"));
        assert!(frags.iter().any(|fragment|
            fragment == "pv/canonical/SOUNDNESS.md"));
        assert!(!frags.iter().any(|fragment| fragment == "canonical/SOUNDNESS.md"));
        assert!(contract_text_mentions_goal_md(&request.sound_contract));

        let mut math = WrapperRequest {
            kind: crate::model::RequestKind::Sound,
            phase: Phase::TheoremStating,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut math, None);
        let math_frags = contract_fragments(&math.sound_contract);
        assert!(math_frags.iter().any(|fragment| fragment == "canonical/SOUNDNESS.md"));
        assert!(!math_frags.iter().any(|fragment|
            fragment == "pv/verifier/soundness/05_pv_floor.md"));
    }

    /// FIX 1 — the lane-COMMON frontier fragments. A PV substantiveness
    /// (`Paper`-kind) per-node request and a PV correspondence (`Corr`-kind)
    /// request select the `pv/` frontier fragments and NONE of the math ones
    /// (those carry "the tex paper being formalized", "substantiveness
    /// paper-basis inputs", and "not the paper-faithfulness task").
    #[test]
    fn pv_verifier_frontier_fragments_are_pv_variants() {
        let spec = NodeId::from("Spec");
        let mut subst = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            phase: Phase::TheoremStating,
            is_pv: true,
            substantiveness_verify_nodes: BTreeSet::from([spec.clone()]),
            node_role: BTreeMap::from([(spec.clone(), PvRole::Spec)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut subst, None);
        let subst_frags = contract_fragments(&subst.paper_contract);
        for want in [
            "pv/verifier/substantiveness/05_fresh_node_frontier.md",
            "pv/verifier/substantiveness/20_node_frontier.md",
            "pv/verifier/substantiveness/30_contract.md",
            "pv/verifier/substantiveness/40_rubric.md",
            "pv/verifier/substantiveness/50_authority_prose.md",
        ] {
            assert!(subst_frags.iter().any(|fragment| fragment == want),
                "PV prose substantiveness must carry {want}; got {subst_frags:?}");
        }
        assert!(contract_text_mentions_goal_md(&subst.paper_contract));

        let mut corr = WrapperRequest {
            kind: crate::model::RequestKind::Corr,
            phase: Phase::TheoremStating,
            is_pv: true,
            verify_nodes: BTreeSet::from([spec.clone()]),
            corr_verify_nodes: BTreeSet::from([spec.clone()]),
            node_role: BTreeMap::from([(spec, PvRole::Spec)]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut corr, None);
        let corr_frags = contract_fragments(&corr.corr_contract);
        assert!(corr_frags.iter().any(|fragment|
            fragment == "pv/verifier/correspondence/05_frontier.md"));
        assert!(!corr_frags.iter().any(|fragment|
            fragment == "verifier/correspondence/05_frontier.md"));
    }

    /// §C.5 — `project_invariants_payload` branch.
    #[test]
    fn project_invariants_payload_pv_branch() {
        let pv = project_invariants_payload(true);
        let pv_modes = pv["progress_modes"].as_array().unwrap();
        assert!(pv_modes.iter().any(|m| m == "goal_coverage_dag_improvement"));
        assert!(!pv_modes.iter().any(|m| m == "paper_faithful_dag_improvement"));

        let math = project_invariants_payload(false);
        let math_modes = math["progress_modes"].as_array().unwrap();
        assert!(math_modes.iter().any(|m| m == "paper_faithful_dag_improvement"));
        assert!(!math_modes.iter().any(|m| m == "goal_coverage_dag_improvement"));
    }

    /// §C.1 — all-math zero-diff: no PV fragment leaks into any all-math role
    /// list. (The byte-for-byte contract identity is enforced by the separate
    /// `contract_baseline` integration test; this guards fragment selection.)
    #[test]
    fn all_math_lists_carry_no_pv_fragment() {
        let assert_no_pv = |contract: &Value, label: &str| {
            for f in contract_fragments(contract) {
                assert!(
                    !f.starts_with("pv/"),
                    "all-math {label} list must carry no pv/ fragment; found {f}"
                );
            }
        };
        for (kind, phase) in [
            (crate::model::RequestKind::Worker, Phase::TheoremStating),
            (crate::model::RequestKind::Review, Phase::TheoremStating),
            (crate::model::RequestKind::Paper, Phase::TheoremStating),
            (crate::model::RequestKind::Corr, Phase::TheoremStating),
            (crate::model::RequestKind::Sound, Phase::ProofFormalization),
        ] {
            let mut request = WrapperRequest {
                kind,
                phase,
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            assert_no_pv(&request.worker_contract, "worker");
            assert_no_pv(&request.review_contract, "review");
            assert_no_pv(&request.paper_contract, "paper");
            assert_no_pv(&request.corr_contract, "corr");
            assert_no_pv(&request.sound_contract, "sound");
        }
    }

    /// §C.7 — fragment-file existence smoke. Every PV path the kernel can select
    /// resolves to a real file under `trellis/prompt_fragments/`.
    #[test]
    fn pv_selected_fragments_exist_on_disk() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../trellis/prompt_fragments");
        // The union of every PV path a kernel branch can select.
        let pv_paths = [
            "pv/common/TRELLIS_FORMALIZATION_SCHEME.md",
            "pv/common/TRELLIS_FORMALIZATION_SCHEME_verifier.md",
            "pv/common/18_source_of_truth.md",
            "pv/canonical/SUBSTANTIVENESS.md",
            "pv/canonical/SOUNDNESS.md",
            "pv/worker/common/00_intro.md",
            "pv/worker/common/06_trust_conditional_candidates.md",
            "pv/worker/common/37_field_guidance.md",
            "pv/worker/theorem_stating/05_frontier_work.md",
            "pv/worker/theorem_stating/10_spec_authoring.md",
            "pv/worker/theorem_stating/12_first_request_decomposition.md",
            "pv/worker/theorem_stating/10_mode_guidance.md",
            "pv/worker/theorem_stating/15_initial_dag_size.md",
            "pv/worker/theorem_stating/17_helper_policy.md",
            "pv/worker/theorem_stating/18_targets.md",
            "pv/worker/theorem_stating/20_common_failure_modes.md",
            "pv/worker/proof_formalization/20_safety.md",
            "pv/worker/proof_formalization/30_invariant.md",
            "pv/worker/proof_formalization/40_correctness.md",
            "pv/worker/proof_formalization/50_proof_method.md",
            "pv/review/25_verifier_reasoning.md",
            "pv/review/27_source_of_truth.md",
            "pv/review/05_after_clean_verification.md",
            "pv/review/29d_assumption_authoring_enact.md",
            "pv/review/31_need_input.md",
            "pv/review/33b_theorem_target_orientation.md",
            "pv/review/33b_proof_target_orientation.md",
            "pv/review/37_restructure_strategy.md",
            "pv/verifier/correspondence/05_frontier.md",
            "pv/verifier/correspondence/09_pv_correspondence.md",
            "pv/verifier/correspondence/11_conditional_theorem.md",
            "pv/verifier/substantiveness/05_fresh_node_frontier.md",
            "pv/verifier/substantiveness/05_revisit_node_frontier.md",
            "pv/verifier/substantiveness/06_with_preamble.md",
            "pv/verifier/substantiveness/20_node_frontier.md",
            "pv/verifier/substantiveness/30_contract.md",
            "pv/verifier/substantiveness/40_rubric.md",
            "pv/verifier/substantiveness/50_authority.md",
            "pv/verifier/substantiveness/50_authority_prose.md",
            "pv/verifier/soundness/05_pv_floor.md",
            "pv/verifier/correspondence/07_scratchpad.md",
            "pv/human_gate/05_pv_monotonicity.md",
            "pv/stuck_audit/01_role.md",
            "pv/stuck_audit/01b_theorem_stating_framing.md",
            "pv/stuck_audit/01_gap_research_role.md",
            "pv/stuck_audit/01_gap_research_role_trust_v1.md",
            "pv/stuck_audit/01_gap_research_role_trust_v1.md",
            "pv/stuck_audit/01_gap_plan_critic_role.md",
            "pv/stuck_audit/01_need_input_auditor_role.md",
            "pv/stuck_audit/01_revision_planner_role.md",
            "pv/stuck_audit/01c_initial_planner_role.md",
            "pv/stuck_audit/01c_initial_planner_role.md",
            "pv/stuck_audit/02_source_of_truth.md",
            "pv/stuck_audit/03_history_access.md",
            "pv/stuck_audit/04_scratchpad.md",
            "pv/stuck_audit/05_output_contract.md",
            "pv/shared/91_structured_request_pointer.md",
            "pv/review/30a_blocker_actions.md",
            "pv/review/33_routing_hints.md",
            "pv/review/33d_challenge_targets.md",
            "pv/review/36_authorized_nodes.md",
            // Stage 7 (plan doc 32, N3+N8): the trust-run deviation-lane
            // fragments.
            "pv/canonical/DEVIATIONS_trust_v1.md",
            "pv/worker/common/21_deviations_trust_v1.md",
            "pv/worker/common/22_conditional_theorem_proposal.md",
            "pv/worker/common/23_model_repair_trust_v1.md",
            "pv/worker/common/71_author_assumption.md",
            "pv/worker/common/71_author_assumption.md",
            "pv/worker/theorem_stating/05_after_substantiveness_review_trust_v1.md",
            "pv/worker/common/70_target_false_under_model.md",
            "pv/worker/common/71_target_false_trust_v1.md",
            "pv/worker/common/72_side_by_side_probes_trust_v1.md",
            "pv/worker/theorem_stating/60_disprove_direction.md",
            "pv/verifier/substantiveness/15_deviations_trust_v1.md",
            "pv/verifier/deviation/05_seam_repair_trust_v1.md",
            "pv/verifier/deviation/30_contract_trust_v1.md",
            "pv/review/05_after_failed_deviation_trust_v1.md",
            "pv/review/05_after_unverified_deviation_trust_v1.md",
            "pv/stuck_audit/05_output_contract_trust_v1.md",
            "pv/stuck_audit/05_gap_plan_output_contract_trust_v1.md",
            "pv/stuck_audit/06_deviation_eligibility.md",
            "pv/stuck_audit/07_under_model_adjudication.md",
            "pv/stuck_audit/08_assumptions_lane_role.md",
            "pv/stuck_audit/08_model_refutation_adjudication_trust_v1.md",
            "pv/stuck_audit/09_conditional_theorem_proposal.md",
        ];
        for path in pv_paths {
            assert!(
                base.join(path).exists(),
                "PV fragment {path} must exist on disk at {}",
                base.join(path).display()
            );
        }
    }

    #[test]
    fn trust_stuck_audit_selects_the_closed_lane_output_contract() {
        // Trust protocol v1 closes the assumption-authoring lane, so a
        // trust auditor must never receive the retired pinned-goal output contract
        // that instructs populating `under_model_candidate_invariant`.
        let mut audit = WrapperRequest {
            kind: crate::model::RequestKind::StuckMathAudit,
            phase: Phase::ProofFormalization,
            is_pv: true,
            trust_base_required_v1: true,
            stuck_math_audit: crate::model::StuckMathAuditState {
                active: true,
                trigger: "stuck".into(),
                ..crate::model::StuckMathAuditState::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut audit, None);
        let afrags = contract_fragments(&audit.stuck_math_audit_contract);
        assert!(
            afrags
                .iter()
                .any(|f| f == "pv/stuck_audit/05_output_contract_trust_v1.md"),
            "trust stuck-audit must carry the closed-lane output contract; got {afrags:?}"
        );
        for forbidden in [
            "pv/stuck_audit/05_output_contract.md",
            "pv/stuck_audit/05_output_contract.md",
        ] {
            assert!(
                !afrags.iter().any(|f| f == forbidden),
                "trust stuck-audit must NOT carry {forbidden}; got {afrags:?}"
            );
        }
    }

    #[test]
    fn required_v1_gap_contract_advertises_routes_or_terminal_refusal_not_human_gate() {
        let mut request = WrapperRequest {
                kind: crate::model::RequestKind::StuckMathAudit,
                phase: Phase::ProofFormalization,
                is_pv: true,
                trust_base_required_v1: true,
                stuck_math_audit: crate::model::StuckMathAuditState {
                    active: true,
                    trigger: "confirmed architecture-dependent gap".into(),
                    gap_research: Some(crate::model::GapResearchContext::default()),
                    ..crate::model::StuckMathAuditState::default()
                },
                ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut request, None);
        let contract = &request.stuck_math_audit_contract;
        let fragments = contract_fragments(contract);

        assert!(fragments
            .iter()
            .any(|fragment| fragment == "pv/stuck_audit/01_gap_research_role_trust_v1.md"));
            assert!(fragments
                .iter()
                .any(|fragment| fragment == "pv/stuck_audit/05_gap_plan_output_contract_trust_v1.md"));
            for forbidden in [
                "pv/stuck_audit/01_gap_research_role.md",
                "pv/stuck_audit/01_gap_research_role.md",
                "pv/stuck_audit/05_gap_plan_output_contract.md",
            ] {
                assert!(
                    !fragments.iter().any(|fragment| fragment == forbidden),
                    "RequiredV1 gap contract leaked HumanGate fragment {forbidden}: {fragments:?}"
                );
            }

            let resolution = &contract["required_v1_resolution_contract"];
            assert_eq!(resolution["human_gate_available"], json!(false));
            assert_eq!(
                resolution["route_needs_human_semantics"],
                json!("terminal_refusal")
            );
            assert!(resolution["legal_routes"]
                .as_array()
                .is_some_and(|routes| routes.iter().any(|route| route == "structured_adjudication")
                    && routes.iter().any(|route| route == "statement_binding")));
    }


    // ------------------------------------------------------------------
    // Stage 7 (plan doc 32, N3+N8): PV deviation-lane enablement.
    // ------------------------------------------------------------------

    /// Stage 7: the PV deviation fragments appear EXACTLY on trust-required
    /// PV requests — never on math, never on untrusted PV (whose lane stays
    /// dropped).
    #[test]
    fn pv_deviation_fragments_present_only_on_pv_requests() {
        let worker_request = |is_pv: bool, trust: bool| {
            let mut request = WrapperRequest {
                kind: crate::model::RequestKind::Worker,
                phase: Phase::TheoremStating,
                is_pv,
                trust_base_required_v1: trust,
                worker_context: crate::model::WorkerContext {
                    worker_profile: WorkerProfile::Theorem,
                    validation_kind: WorkerValidationKind::TheoremGlobal,
                    ..crate::model::WorkerContext::default()
                },
                configured_challenge_targets: BTreeMap::from([(
                    crate::model::ChallengeTargetId::from("goal:conditional"),
                    crate::model::ChallengeTargetSpec {
                        kind: crate::model::ChallengeTargetKind::Theorem,
                        name: "ConditionalTarget".into(),
                        lean: "theorem ConditionalTarget : True := by".into(),
                        resolution: crate::model::ChallengeResolution::Decide,
                        statement_provenance:
                            crate::model::StatementProvenance::KernelDerived,
                        ..crate::model::ChallengeTargetSpec::default()
                    },
                )]),
                ..WrapperRequest::default()
            };
            populate_request_prompt_contracts(&mut request, None);
            contract_fragments(&request.worker_contract)
        };
        let trust_pv = worker_request(true, true);
        for want in [
            "pv/worker/common/21_deviations_trust_v1.md",
            "pv/worker/common/22_conditional_theorem_proposal.md",
        ] {
            assert!(trust_pv.iter().any(|f| f == want), "{want}: {trust_pv:?}");
        }
        assert!(!trust_pv.iter().any(|f| f == "worker/common/21_deviations.md"));
        let untrusted_pv = worker_request(true, false);
        assert!(
            !untrusted_pv.iter().any(|f| f.contains("deviations")),
            "the untrusted-PV lane stays dropped: {untrusted_pv:?}"
        );
        let math = worker_request(false, false);
        assert!(math.iter().any(|f| f == "worker/common/21_deviations.md"));
        assert!(
            !math.iter().any(|f| f.contains("trust_v1")),
            "math carries no trust fragment: {math:?}"
        );

        let mut cleanup = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::Cleanup,
            is_pv: true,
            trust_base_required_v1: true,
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::Cleanup,
                validation_kind: WorkerValidationKind::Cleanup,
                ..crate::model::WorkerContext::default()
            },
            configured_challenge_targets: BTreeMap::from([(
                crate::model::ChallengeTargetId::from("goal:conditional"),
                crate::model::ChallengeTargetSpec {
                    kind: crate::model::ChallengeTargetKind::Theorem,
                    name: "ConditionalTarget".into(),
                    lean: "theorem ConditionalTarget : True := by".into(),
                    resolution: crate::model::ChallengeResolution::Decide,
                    statement_provenance: crate::model::StatementProvenance::KernelDerived,
                    ..crate::model::ChallengeTargetSpec::default()
                },
            )]),
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut cleanup, None);
        let cleanup_fragments = contract_fragments(&cleanup.worker_contract);
        assert!(!cleanup_fragments
            .iter()
            .any(|fragment| fragment == "pv/worker/common/22_conditional_theorem_proposal.md"));

        // Deviation-authorization verifier scenario.
        let paper_request = |is_pv: bool, trust: bool| WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            is_pv,
            trust_base_required_v1: trust,
            deviation_verify_id: Some(crate::model::DeviationId::from("dev-a")),
            deviation_verify_path: "reference/dev-a.tex".into(),
            ..WrapperRequest::default()
        };
        let trust_fragments =
            paper_prompt_fragments(&paper_request(true, true), crate::backend::BackendId::Lean);
        for want in [
            "pv/verifier/deviation/05_seam_repair_trust_v1.md",
            "pv/verifier/deviation/30_contract_trust_v1.md",
            "pv/canonical/DEVIATIONS_trust_v1.md",
        ] {
            assert!(
                trust_fragments.iter().any(|f| *f == want),
                "{want}: {trust_fragments:?}"
            );
        }
        assert!(!trust_fragments.iter().any(|f| *f == "canonical/DEVIATIONS.md"));
        let math_fragments =
            paper_prompt_fragments(&paper_request(false, false), crate::backend::BackendId::Lean);
        for want in [
            "verifier/deviation/05_single_file.md",
            "verifier/deviation/30_contract.md",
            "canonical/DEVIATIONS.md",
        ] {
            assert!(
                math_fragments.iter().any(|f| *f == want),
                "{want}: {math_fragments:?}"
            );
        }
        assert!(!math_fragments.iter().any(|f| f.contains("trust_v1")));

        // Substantiveness verifier fragment.
        let mut sub_request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            is_pv: true,
            trust_base_required_v1: true,
            substantiveness_verify_nodes: BTreeSet::from([NodeId::from("N")]),
            ..WrapperRequest::default()
        };
        let fragments = paper_prompt_fragments(&sub_request, crate::backend::BackendId::Lean);
        assert!(fragments
            .iter()
            .any(|f| *f == "pv/verifier/substantiveness/15_deviations_trust_v1.md"));
        sub_request.trust_base_required_v1 = false;
        let fragments = paper_prompt_fragments(&sub_request, crate::backend::BackendId::Lean);
        assert!(!fragments.iter().any(|f| f.contains("15_deviations")));
    }

    /// Stage 7: the Lane-1 model-repair fragment + summary projection ride
    /// exactly the worker requests carrying a pending model-repair binding.

    /// Stage 7, renamed by the S7 audit (Codex 5): this asserts FRAGMENT
    /// SELECTION, not bytes — the four flipped drop sites all keep their
    /// exact math fragment lists and no math contract names a Stage-7
    /// fragment.  The former name promised a byte-identity snapshot and
    /// the former comment deferred that proof to the `contract_baseline`
    /// fixtures, which carry no prompt data at all.  A durable
    /// fragment-CONTENT gate, if ever wanted, belongs with the A1 fixture
    /// set, not here.
    #[test]
    fn math_prompt_fragment_selectors_unchanged_across_pv_deviation_enablement() {
        // Site 3 (worker common list) + site 2 (after-substantiveness).
        let mut worker = WrapperRequest {
            kind: crate::model::RequestKind::Worker,
            phase: Phase::ProofFormalization,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::Substantiveness,
                object: BlockerObject::Node {
                    node: NodeId::from("N"),
                },
                fingerprint: String::new(),
                deferred: false,
            }]),
            worker_context: crate::model::WorkerContext {
                worker_profile: WorkerProfile::ProofEasy,
                validation_kind: WorkerValidationKind::ProofEasy,
                ..crate::model::WorkerContext::default()
            },
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut worker, None);
        let fragments = contract_fragments(&worker.worker_contract);
        assert!(fragments.iter().any(|f| f == "worker/common/21_deviations.md"));
        assert!(fragments
            .iter()
            .any(|f| f == "worker/theorem_stating/05_after_substantiveness_review.md"));
        assert!(!fragments.iter().any(|f| f.starts_with("pv/")));

        // Site 1 (substantiveness verifier).
        let sub_request = WrapperRequest {
            kind: crate::model::RequestKind::Paper,
            substantiveness_verify_nodes: BTreeSet::from([NodeId::from("N")]),
            ..WrapperRequest::default()
        };
        let fragments = paper_prompt_fragments(&sub_request, crate::backend::BackendId::Lean);
        assert!(fragments
            .iter()
            .any(|f| *f == "verifier/substantiveness/15_deviations.md"));
        assert!(!fragments.iter().any(|f| f.starts_with("pv/")));

        // Site 4 (review retry routing): the math deviation-blocker review
        // keeps its exact fragments.
        let mut review = WrapperRequest {
            kind: crate::model::RequestKind::Review,
            phase: Phase::TheoremStating,
            blockers: BTreeSet::from([Blocker {
                kind: BlockerKind::Deviation,
                object: BlockerObject::Deviation {
                    deviation: crate::model::DeviationId::from("dev-a"),
                },
                fingerprint: String::new(),
                deferred: false,
            }]),
            deviation_blocker_adjudicable: true,
            ..WrapperRequest::default()
        };
        populate_request_prompt_contracts(&mut review, None);
        let fragments = contract_fragments(&review.review_contract);
        assert!(fragments
            .iter()
            .any(|f| f == "review/common/05_after_failed_deviation.md"));
        assert!(!fragments.iter().any(|f| f.contains("trust_v1")));
        review.deviation_blocker_adjudicable = false;
        populate_request_prompt_contracts(&mut review, None);
        let fragments = contract_fragments(&review.review_contract);
        assert!(fragments
            .iter()
            .any(|f| f == "review/common/05_after_unverified_deviation.md"));
    }

    /// Stage 7 (audit F6): the rewritten fragment 06 names no deleted
    /// machinery and keeps the honest-reporting sentence verbatim.
    #[test]
    fn fragment_06_names_no_deleted_machinery() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../trellis/prompt_fragments/pv/worker/common/06_trust_conditional_candidates.md");
        let text = std::fs::read_to_string(&path).expect("fragment 06 exists");
        for banned in [
            "qualification profile",
            "seed catalog",
            "reflection method",
            "not_defined_for_claim_shape_v1",
            "trust-revision lifecycle",
        ] {
            assert!(
                !text.contains(banned),
                "fragment 06 must not name deleted machinery `{banned}`"
            );
        }
        assert!(
            text.contains(
                "Never describe a model-admissible witness as a real Rust execution, \
                 allocation, or source counterexample unless the target's frozen validation \
                 method produces the corresponding authenticated source result."
            ),
            "fragment 06 keeps the honest-reporting sentence verbatim"
        );
    }

    #[test]
    fn prompt_contract_version_63_is_pinned_at_all_tla_sites() {
        assert_eq!(prompt_contract_version(), 64);
        let spec = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../spec/SupervisorProtocol.tla"),
        )
        .expect("read SupervisorProtocol.tla prompt-contract mirror");
        let sites = [
            "promptContractVersion |-> IF kind = \"none\" THEN 0 ELSE 64",
            "inFlightRequest.promptContractVersion \\in 0..64",
            "IF inFlightRequest.kind = \"none\" THEN 0 ELSE 64",
        ];
        assert!(sites.iter().all(|site| spec.contains(site)));
        for site in sites {
            let drifted = spec.replacen(site, &site.replace("64", "63"), 1);
            assert!(
                !sites.iter().all(|expected| drifted.contains(expected)),
                "moving one TLA prompt-contract pin must break the coupled check"
            );
        }
    }
}

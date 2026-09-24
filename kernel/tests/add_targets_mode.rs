//! Add-targets mode end-to-end fixture (audited scope; see `ADD_TARGETS.md`).
//!
//! Chains the committed steps through the kernel's public APIs, mirroring
//! `revision_mode_integration.rs`:
//!
//!   add_paper_targets_to_state        (pure Complete-revival mutation)
//!     -> RevisionStating / Start, planner lane seeded, approvals untouched
//!   apply_event(StartCycle)           (planner dispatch routes)
//!   apply_event(planner response)     (revision plan accepted)
//!   model-level statement work        (new covering node accepted;
//!                                      claim-only tie of a frozen node
//!                                      accepted; frozen-node text edit
//!                                      excluded by the edit-scope envelope)
//!   paper lane                        (only the added targets verify)
//!   apply_event(HumanGate Approve)    (approved_targets grows; pre-existing
//!                                      entries byte-equal; PF resumes)
//!
//! Plus: the precondition matrix, the byte-untouched JSON-diff whitelist,
//! LastClean-rewind preservation, normalize_target_claims ordering, and the
//! runtime-CLI action driven end-to-end against a real runtime root.
//!
//! Constraints: small synthetic fixture, no host `lake`, temp dirs rooted
//! under the build area via `common::project_tempdir` (never `/tmp`).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use trellis_kernel::engine::{apply_event, ProtocolCommand, ProtocolEvent};
use trellis_kernel::{
    add_paper_targets_to_state, check_add_targets_preconditions, AddedTargetSpec, CorrStatus,
    EventLogRecord, GateKind, HumanChoice, HumanGateResponse, LocalClosureRecord, NodeId, NodeKind,
    PendingTask, Phase, ProofEditMode, ProtocolState, RequestKind, ResponseStatus, RevisionActions,
    RevisionContext, RevisionKind, RevisionNodeAction, RevisionNodeDisposition,
    RevisionPlanningContext, RevisionTargetAction, RevisionTargetDeltaKind, RuntimeMetadata,
    RuntimePaths, Stage, StuckMathAuditResponse, SupervisorRuntime, TargetEditMode, TargetId,
    WorkerValidationExecutionPlanStep, AUDIT_REPORT_TEXT_MIN_CHARS,
};

fn nid(s: &str) -> NodeId {
    NodeId::from(s)
}

fn tid(s: &str) -> TargetId {
    TargetId::from(s)
}

/// A COMPLETED mode-A run: two approved targets (`thm:main` covered by
/// `MainTheorem`; `lem:aux` covered by `Aux` with `Helper` in its protected
/// closure), every lane Pass with approved == current, clean mirrors
/// captured, cycle 42. Passes `validate()`.
fn complete_fixture() -> ProtocolState {
    let mut state = ProtocolState::default();
    state.max_theorem_invalid_attempt = 2;
    state.proof_invalid_review_threshold = 2;
    state.easy_max_retries = 2;

    let present: BTreeSet<NodeId> = ["Preamble", "MainTheorem", "Aux", "Helper"]
        .iter()
        .map(|n| nid(n))
        .collect();
    let mut node_kinds: BTreeMap<NodeId, NodeKind> = BTreeMap::new();
    for n in &present {
        node_kinds.insert(
            n.clone(),
            if n.as_str() == "Preamble" {
                NodeKind::Preamble
            } else {
                NodeKind::Proof
            },
        );
    }
    let proof_nodes: BTreeSet<NodeId> = ["MainTheorem", "Aux", "Helper"]
        .iter()
        .map(|n| nid(n))
        .collect();
    let mut target_claims: BTreeMap<NodeId, BTreeSet<TargetId>> = BTreeMap::new();
    target_claims.insert(nid("MainTheorem"), BTreeSet::from([tid("thm:main")]));
    target_claims.insert(nid("Aux"), BTreeSet::from([tid("lem:aux")]));

    state.configured_targets = BTreeSet::from([tid("thm:main"), tid("lem:aux")]);
    state.node_kinds = node_kinds.clone();
    state.committed_node_kinds = node_kinds;
    state.proof_nodes = proof_nodes.clone();
    state.committed_proof_nodes = proof_nodes;
    state.target_claims = target_claims.clone();
    state.committed_target_claims = target_claims;
    state.live.present_nodes = present.clone();
    state
        .live
        .protected_closure_nodes_per_target
        .insert(tid("lem:aux"), BTreeSet::from([nid("Helper")]));
    for (t, fp) in [("thm:main", "m=fp"), ("lem:aux", "a=fp")] {
        state
            .live
            .paper_current_fingerprints
            .insert(tid(t), fp.to_string());
        state.paper_status.insert(tid(t), CorrStatus::Pass);
        state
            .paper_approved_fingerprints
            .insert(tid(t), fp.to_string());
    }
    for n in &present {
        let corr = format!("corr-{}", n.as_str());
        let sound = format!("sound-{}", n.as_str());
        let subst = format!("subst-{}", n.as_str());
        state
            .live
            .corr_current_fingerprints
            .insert(n.clone(), corr.clone());
        state.corr_approved_fingerprints.insert(n.clone(), corr);
        state.corr_status.insert(n.clone(), CorrStatus::Pass);
        state
            .live
            .sound_current_fingerprints
            .insert(n.clone(), sound.clone());
        state.sound_approved_fingerprints.insert(n.clone(), sound);
        state.sound_status.insert(n.clone(), trellis_kernel::SoundStatus::Pass);
        state
            .live
            .substantiveness_current_fingerprints
            .insert(n.clone(), subst.clone());
        state
            .substantiveness_approved_fingerprints
            .insert(n.clone(), subst);
        state
            .substantiveness_status
            .insert(n.clone(), trellis_kernel::SubstantivenessStatus::Pass);
    }
    state.normalize_all_structural_state();
    for proof in state.proof_nodes.clone() {
        let mut record = LocalClosureRecord::default();
        record.node = proof.clone();
        record.closure_version = "add-targets-fixture-v1".to_string();
        record.toolchain_hash = "toolchain".to_string();
        record.lake_manifest_hash = "lake".to_string();
        record.preamble_hash = "preamble".to_string();
        record.approved_axioms_hash = "axioms".to_string();
        record.active_decl_hash = format!("decl-{}", proof.as_str());
        record.active_statement_hash = format!("statement-{}", proof.as_str());
        state.local_closure_records.insert(proof, record);
    }
    // Snapshot committed + the LastClean mirrors at a clean checkpoint
    // (global blockers are empty: every lane Pass with approved == current).
    state.commit_live();
    state.phase = Phase::Complete;
    state.stage = Stage::Complete;
    state.cycle = 42;
    state.ensure_node_metadata();
    state.validate().expect("fixture must be valid");
    assert!(
        state.global_blockers().is_empty(),
        "fixture must be clean (all lanes Pass)"
    );
    assert!(
        state.last_clean_mirrors_populated(),
        "fixture must have LastClean mirrors captured"
    );
    state
}

fn added_thm_new() -> Vec<AddedTargetSpec> {
    vec![AddedTargetSpec {
        target: tid("thm:new"),
        label: "thm:new".to_string(),
        start_line: 9,
        end_line: 11,
    }]
}

fn revive(state: &mut ProtocolState) -> trellis_kernel::AddTargetsSummary {
    add_paper_targets_to_state(
        state,
        &added_thm_new(),
        "paper/main.tex",
        "papersha",
        "add-targets:2026-07-14",
    )
    .expect("add_paper_targets_to_state succeeds on the Complete fixture")
}

// ===== Precondition matrix =================================================

/// Every rejection path errors AND mutates nothing (byte-identical state).
#[test]
fn precondition_matrix_rejects_and_mutates_nothing() {
    struct Case {
        name: &'static str,
        prep: fn(&mut ProtocolState) -> Vec<AddedTargetSpec>,
        expect: &'static str,
    }
    let cases = [
        Case {
            name: "phase_not_complete",
            prep: |state| {
                state.phase = Phase::ProofFormalization;
                state.stage = Stage::Start;
                added_thm_new()
            },
            expect: "requires a COMPLETED run",
        },
        Case {
            name: "in_flight_request",
            prep: |state| {
                let request = state.issue_request(RequestKind::Review);
                state.in_flight_request = Some(Box::new(request));
                added_thm_new()
            },
            expect: "no in-flight request",
        },
        Case {
            name: "active_human_gate",
            prep: |state| {
                state.gate_kind = GateKind::Advance;
                added_thm_new()
            },
            expect: "no active human gate",
        },
        Case {
            name: "pending_task",
            prep: |state| {
                state.pending_task = Some(Default::default());
                added_thm_new()
            },
            expect: "no pending worker task",
        },
        Case {
            name: "pending_protected_reapproval",
            prep: |state| {
                state
                    .pending_protected_reapproval_nodes
                    .insert(nid("MainTheorem"));
                added_thm_new()
            },
            expect: "no pending protected-reapproval nodes",
        },
        Case {
            name: "pv_run",
            prep: |state| {
                state.pv_tablet_configured = true;
                added_thm_new()
            },
            expect: "program-verification run",
        },
        Case {
            name: "mode_b_no_configured_targets",
            prep: |state| {
                state.configured_targets.clear();
                state.target_claims.clear();
                state.committed_target_claims.clear();
                state.paper_status.clear();
                state.normalize_all_structural_state();
                added_thm_new()
            },
            expect: "non-empty configured-target set",
        },
        Case {
            name: "added_empty",
            prep: |_| Vec::new(),
            expect: "no new targets to add",
        },
        Case {
            name: "added_label_already_configured",
            prep: |_| {
                vec![AddedTargetSpec {
                    target: tid("thm:main"),
                    label: "thm:main".to_string(),
                    start_line: 1,
                    end_line: 3,
                }]
            },
            expect: "already configured",
        },
        Case {
            name: "non_positive_block_lines",
            prep: |_| {
                vec![AddedTargetSpec {
                    target: tid("thm:new"),
                    label: "thm:new".to_string(),
                    start_line: 0,
                    end_line: 0,
                }]
            },
            expect: "non-positive/inverted paper block lines",
        },
        Case {
            name: "empty_label",
            prep: |_| {
                vec![AddedTargetSpec {
                    target: tid("thm:new"),
                    label: "  ".to_string(),
                    start_line: 9,
                    end_line: 11,
                }]
            },
            expect: "empty tex label",
        },
        Case {
            name: "duplicate_added_target",
            prep: |_| {
                let one = AddedTargetSpec {
                    target: tid("thm:new"),
                    label: "thm:new".to_string(),
                    start_line: 9,
                    end_line: 11,
                };
                vec![one.clone(), one]
            },
            expect: "more than once",
        },
    ];

    for case in &cases {
        let mut state = complete_fixture();
        let added = (case.prep)(&mut state);
        let before = serde_json::to_value(&state).expect("serialize");
        let err = add_paper_targets_to_state(
            &mut state,
            &added,
            "paper/main.tex",
            "papersha",
            "add-targets:2026-07-14",
        )
        .expect_err(&format!("case `{}` must be rejected", case.name));
        assert!(
            err.contains(case.expect),
            "case `{}`: error must contain `{}`; got: {err}",
            case.name,
            case.expect
        );
        let after = serde_json::to_value(&state).expect("serialize");
        assert_eq!(
            before, after,
            "case `{}`: a rejected action must mutate NOTHING",
            case.name
        );
    }
}

/// Idempotence pins (amendment 1 semantics): a second identical run on the
/// original Complete state errors with the added-∅ message; a run on the
/// already-revived state errors on the phase precondition.
#[test]
fn second_identical_run_errors() {
    // Labels already configured on a Complete run -> added is empty.
    let mut state = complete_fixture();
    let err = add_paper_targets_to_state(
        &mut state,
        &[],
        "paper/main.tex",
        "papersha",
        "add-targets:2026-07-14",
    )
    .expect_err("added-∅ must error");
    assert!(err.contains("no new targets to add"), "got: {err}");

    // Re-running the SAME add on the revived state errors on phase.
    let mut state = complete_fixture();
    revive(&mut state);
    let err = add_paper_targets_to_state(
        &mut state,
        &added_thm_new(),
        "paper/main.tex",
        "papersha",
        "add-targets:2026-07-14",
    )
    .expect_err("revived state is not Complete");
    assert!(err.contains("requires a COMPLETED run"), "got: {err}");

    // check_add_targets_preconditions agrees (pure check, no mutation).
    let state = complete_fixture();
    assert!(check_add_targets_preconditions(&state, &added_thm_new()).is_ok());
}

// ===== Byte-untouched whitelist ===========================================

/// Collect JSON pointer-ish paths where `a` and `b` differ. Objects recurse;
/// everything else records the path.
fn diff_paths(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    if a == b {
        return;
    }
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            let keys: BTreeSet<&String> = ma.keys().chain(mb.keys()).collect();
            for key in keys {
                let sub_a = ma.get(key.as_str()).unwrap_or(&Value::Null);
                let sub_b = mb.get(key.as_str()).unwrap_or(&Value::Null);
                diff_paths(sub_a, sub_b, &format!("{path}/{key}"), out);
            }
        }
        _ => out.push(path.to_string()),
    }
}

/// The action's serialized-state diff is confined to the whitelisted revival
/// paths; in particular every approved-fingerprint map is byte-untouched.
#[test]
fn state_diff_confined_to_whitelist() {
    let mut state = complete_fixture();
    let before = serde_json::to_value(&state).expect("serialize before");
    revive(&mut state);
    let after = serde_json::to_value(&state).expect("serialize after");

    let mut changed = Vec::new();
    diff_paths(&before, &after, "", &mut changed);
    assert!(!changed.is_empty(), "the action must change SOMETHING");

    let added = "thm:new";
    let allowed_prefixes: Vec<String> = [
        // Revival core.
        "/phase".to_string(),
        "/stage".to_string(),
        "/configured_targets".to_string(),
        "/revision_context".to_string(),
        "/stuck_math_audit".to_string(),
        "/progress_history".to_string(),
        // Added-target derived map entries (live + committed + LastClean
        // mirror), amendment: per-target additions only.
        format!("/paper_status/{added}"),
        format!("/live/coverage/{added}"),
        format!("/committed/coverage/{added}"),
        format!("/last_clean_live/coverage/{added}"),
        format!("/live/paper_current_fingerprints/{added}"),
        format!("/committed/paper_current_fingerprints/{added}"),
        format!("/last_clean_live/paper_current_fingerprints/{added}"),
        // Active seats / edit modes / gate resets.
        "/active_node".to_string(),
        "/active_coarse_node".to_string(),
        "/cycles_in_coarse_repair_mode".to_string(),
        "/held_target".to_string(),
        "/target_edit_mode".to_string(),
        "/proof_edit_mode".to_string(),
        "/human_input_outstanding".to_string(),
        "/gate_kind".to_string(),
        "/gate_from_invalid_attempt".to_string(),
        "/in_flight_request".to_string(),
        "/pending_task".to_string(),
        // Cleanup latch set + pending carriers.
        "/cleanup_audit_tasks".to_string(),
        "/cleanup_audit_scratchpad".to_string(),
        "/cleanup_audit_burst_count".to_string(),
        "/cleanup_audit_round".to_string(),
        "/cleanup_consecutive_invalid_workers".to_string(),
        "/cleanup_active_task".to_string(),
        "/cleanup_force_done".to_string(),
        "/latest_audit_rejection_reason".to_string(),
        "/audit_burst_retry_count".to_string(),
        "/pending_global_repair_request".to_string(),
        "/pending_global_repair_grant".to_string(),
        "/latest_global_repair_audit_decline_reason".to_string(),
        "/latest_global_repair_audit_decline_cycle".to_string(),
        "/pending_node_retirement".to_string(),
        "/latest_node_retirement_decline".to_string(),
        "/pending_audit_request".to_string(),
        "/pending_worker_audit_request".to_string(),
    ]
    .to_vec();

    for path in &changed {
        assert!(
            allowed_prefixes
                .iter()
                .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/"))),
            "non-whitelisted state diff at `{path}` — the action must leave everything \
             else byte-untouched. Full diff: {changed:#?}"
        );
    }

    // Belt-and-braces: the approval surfaces and the run cursor are
    // byte-identical.
    for key in [
        "corr_approved_fingerprints",
        "sound_approved_fingerprints",
        "paper_approved_fingerprints",
        "substantiveness_approved_fingerprints",
        "approved_targets",
        "cycle",
        "node_kinds",
        "deps",
    ] {
        assert_eq!(
            before.get(key),
            after.get(key),
            "`{key}` must be byte-untouched"
        );
    }
}

// ===== Revival shape ======================================================

#[test]
fn revival_seeds_planner_and_keeps_cycle_continuous() {
    let mut state = complete_fixture();
    let summary = revive(&mut state);

    assert_eq!(state.phase, Phase::RevisionStating);
    assert_eq!(state.stage, Stage::Start);
    assert_eq!(state.cycle, 42, "cycle stays CONTINUOUS (no reset to 0)");
    assert!(state.stuck_math_audit.active);
    assert_eq!(state.stuck_math_audit.active_since_cycle, 42);
    assert!(!state.stuck_math_audit.trigger.is_empty());
    let planning = state
        .stuck_math_audit
        .revision_planning
        .as_ref()
        .expect("planner lane seeded");
    // Sibling lane carriers all None (the at-most-one-lane mutex).
    assert!(state.stuck_math_audit.need_input_audit.is_none());
    assert!(state.stuck_math_audit.gap_research.is_none());
    assert!(state.stuck_math_audit.gap_plan_critique.is_none());
    assert!(state.stuck_math_audit.assumptions_lane.is_none());
    assert!(state.pending_global_repair_request.is_none());

    let ctx = state.revision_context.as_ref().expect("revision context");
    // Same paper on both sides; deltas: existing Unchanged, added Added.
    assert_eq!(ctx.old_paper_path, ctx.new_paper_path);
    assert_eq!(ctx.old_paper_sha, ctx.new_paper_sha);
    assert_eq!(
        ctx.target_deltas[&tid("thm:main")].kind,
        RevisionTargetDeltaKind::Unchanged
    );
    assert_eq!(
        ctx.target_deltas[&tid("lem:aux")].kind,
        RevisionTargetDeltaKind::Unchanged
    );
    assert_eq!(
        ctx.target_deltas[&tid("thm:new")].kind,
        RevisionTargetDeltaKind::Added
    );
    assert_eq!(ctx.invalidated_targets, BTreeSet::from([tid("thm:new")]));
    assert_eq!(planning.target_deltas, ctx.target_deltas);

    // Frozen: existing coverage + protected closure + Preamble; editable =
    // present - frozen (nothing else is present here, so editable empty).
    for frozen in ["Preamble", "MainTheorem", "Aux", "Helper"] {
        assert!(ctx.frozen_nodes.contains(&nid(frozen)), "{frozen} frozen");
        assert_eq!(
            ctx.node_dispositions.get(&nid(frozen)),
            Some(&RevisionNodeDisposition::Freeze)
        );
    }
    assert!(ctx.editable_nodes.is_empty());
    assert_eq!(summary.added_targets, vec![tid("thm:new")]);
    assert_eq!(summary.total_targets, 3);

    // The added target enters the paper lane fresh; existing approvals hold.
    assert_eq!(state.paper_status.get(&tid("thm:new")), Some(&CorrStatus::Unknown));
    assert!(!state.paper_approved_fingerprints.contains_key(&tid("thm:new")));
    assert_eq!(
        state.paper_approved_fingerprints.get(&tid("thm:main")),
        Some(&"m=fp".to_string())
    );

    // Edit modes / seats reset.
    assert_eq!(state.target_edit_mode, TargetEditMode::Global);
    assert_eq!(state.proof_edit_mode, ProofEditMode::Local);
    assert!(state.active_node.is_none());
    assert!(state.held_target.is_none());
    assert!(!state.human_input_outstanding);

    // Paper lane frontier: ONLY the added target is blocked (existing
    // targets do not re-verify — per-target fingerprints).
    assert_eq!(state.blocked_targets(), BTreeSet::from([tid("thm:new")]));
}

// ===== RevisionKind: planner role fragment + serde default ================

/// The revived context is marked `TargetAddition` (both the persistent
/// `RevisionContext` and the planner packet), and the dispatched planner
/// contract selects the target-addition role fragment — the two-paper
/// revision role (whose "read both papers" framing is false here) is absent.
/// The kind-neutral output contract rides along unchanged.
#[test]
fn revival_selects_target_addition_planner_fragment() {
    let mut state = complete_fixture();
    revive(&mut state);

    let ctx = state.revision_context.as_ref().expect("revision context");
    assert_eq!(ctx.revision_kind, RevisionKind::TargetAddition);
    let planning = state
        .stuck_math_audit
        .revision_planning
        .as_ref()
        .expect("planner lane seeded");
    assert_eq!(planning.revision_kind, RevisionKind::TargetAddition);

    let outcome = apply_event(state, ProtocolEvent::StartCycle).expect("StartCycle");
    let request = match outcome.commands.as_slice() {
        [ProtocolCommand::IssueRequest { request }] => request.clone(),
        other => panic!("expected a StuckMathAudit request, got {other:?}"),
    };
    assert_eq!(request.kind, RequestKind::StuckMathAudit);
    let fragments: Vec<&str> = request.stuck_math_audit_contract["prompt_fragments"]
        .as_array()
        .expect("prompt_fragments array")
        .iter()
        .map(|f| f.as_str().expect("fragment string"))
        .collect();
    assert!(
        fragments.contains(&"stuck_math_audit/common/01b_target_addition_planner_role.md"),
        "target-addition planner role fragment missing; got: {fragments:?}"
    );
    assert!(
        !fragments.contains(&"stuck_math_audit/common/01_revision_planner_role.md"),
        "two-paper revision planner role must not be pushed on a target addition"
    );
    assert!(
        fragments.contains(&"stuck_math_audit/common/05_revision_plan_output_contract.md"),
        "kind-neutral revision plan output contract missing; got: {fragments:?}"
    );
}

/// Legacy persisted revision contexts predate `revision_kind`: a kind-less
/// JSON context deserializes as `PaperRevision` (the serde default) and
/// round-trips unchanged. The snake_case alias is accepted on input.
#[test]
fn kindless_revision_context_defaults_to_paper_revision_and_round_trips() {
    let raw = serde_json::json!({
        "old_paper_path": "paper/revision/old.tex",
        "new_paper_path": "paper/revision/new.tex",
    });
    let ctx: RevisionContext =
        serde_json::from_value(raw).expect("kind-less RevisionContext deserializes");
    assert_eq!(ctx.revision_kind, RevisionKind::PaperRevision);
    let round: RevisionContext =
        serde_json::from_str(&serde_json::to_string(&ctx).expect("serialize"))
            .expect("round-trip deserializes");
    assert_eq!(round, ctx);

    let planning: RevisionPlanningContext =
        serde_json::from_value(serde_json::json!({}))
            .expect("kind-less RevisionPlanningContext deserializes");
    assert_eq!(planning.revision_kind, RevisionKind::PaperRevision);
    let round: RevisionPlanningContext =
        serde_json::from_str(&serde_json::to_string(&planning).expect("serialize"))
            .expect("round-trip deserializes");
    assert_eq!(round, planning);

    let aliased: RevisionContext =
        serde_json::from_value(serde_json::json!({ "revision_kind": "target_addition" }))
            .expect("snake_case alias deserializes");
    assert_eq!(aliased.revision_kind, RevisionKind::TargetAddition);
}

// ===== LastClean rewind after add =========================================

#[test]
fn last_clean_rewind_after_add_preserves_targets_and_phase() {
    let mut state = complete_fixture();
    revive(&mut state);
    assert_eq!(
        state.apply_last_clean_reset(),
        Ok(true),
        "LastClean mirrors were captured pre-add and patched by the action"
    );
    assert_eq!(state.phase, Phase::RevisionStating, "phase survives");
    assert_eq!(
        state.configured_targets,
        BTreeSet::from([tid("thm:main"), tid("lem:aux"), tid("thm:new")]),
        "configured targets survive the rewind"
    );
    for target in &state.configured_targets {
        assert!(
            state.live.coverage.contains_key(target),
            "restored coverage must include {target}"
        );
        assert!(
            state.live.paper_current_fingerprints.contains_key(target),
            "restored paper fingerprints must include {target}"
        );
    }
    state.validate().expect("post-rewind state validates");
}

// ===== normalize_target_claims ordering ===================================

/// A claim naming the new target is STRIPPED by structural normalize before
/// the action (target unconfigured) and RETAINED after it (target
/// configured).
#[test]
fn normalize_target_claims_ordering_around_the_action() {
    // Before: claim on the not-yet-configured target is stripped.
    let mut state = complete_fixture();
    state
        .target_claims
        .insert(nid("Helper"), BTreeSet::from([tid("thm:new")]));
    state.normalize_all_structural_state();
    assert!(
        state
            .target_claims
            .get(&nid("Helper"))
            .map(|claims| claims.is_empty())
            .unwrap_or(true),
        "claim on an unconfigured target must be stripped"
    );
    assert!(!state.live.coverage.contains_key(&tid("thm:new")));

    // After: the same claim survives normalize and feeds coverage.
    let mut state = complete_fixture();
    revive(&mut state);
    state
        .target_claims
        .insert(nid("Helper"), BTreeSet::from([tid("thm:new")]));
    state.normalize_all_structural_state();
    assert_eq!(
        state.target_claims.get(&nid("Helper")),
        Some(&BTreeSet::from([tid("thm:new")])),
        "claim on the newly configured target must be retained"
    );
    assert_eq!(
        state.live.coverage.get(&tid("thm:new")),
        Some(&BTreeSet::from([nid("Helper")]))
    );
    state.validate().expect("claim-tied state validates");
}

// ===== Full pipeline: planner -> statement work -> gate -> PF =============

fn planner_response() -> StuckMathAuditResponse {
    StuckMathAuditResponse {
        status: ResponseStatus::Ok,
        report: "## Claim being audited\n".to_string() + &"x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS),
        revision_actions: RevisionActions {
            targets: vec![RevisionTargetAction {
                target: tid("thm:new"),
                classification: "add".into(),
                covering_nodes: vec![nid("WeakTheorem")],
            }],
            nodes: vec![RevisionNodeAction {
                node: nid("WeakTheorem"),
                action: "new".into(),
                reason: "new target needs a fresh covering statement".into(),
            }],
        },
        ..StuckMathAuditResponse::default()
    }
}

#[test]
fn full_flow_planner_statement_gate_reaches_proof_formalization() {
    let mut state = complete_fixture();
    revive(&mut state);

    // -- StartCycle dispatches the revision planner; cycle continues. ------
    let outcome = apply_event(state, ProtocolEvent::StartCycle).expect("StartCycle");
    let state = outcome.state;
    assert_eq!(state.cycle, 43, "cycle continues from 42, not from 0");
    assert_eq!(state.stage, Stage::StuckMathAudit);
    let request = match outcome.commands.as_slice() {
        [ProtocolCommand::IssueRequest { request }] => {
            assert_eq!(request.kind, RequestKind::StuckMathAudit);
            assert!(request.stuck_math_audit.revision_planning.is_some());
            request.clone()
        }
        other => panic!("expected a StuckMathAudit request, got {other:?}"),
    };

    // -- Planner accepted: plan recorded, lane cleared, routes to review. --
    let mut response = planner_response();
    response.request_id = request.id;
    response.cycle = request.cycle;
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::StuckMathAudit(response),
        },
    )
    .expect("planner response accepted");
    let mut state = outcome.state;
    assert_eq!(
        state.latest_stuck_math_audit_rejection_reason, "",
        "planner response must be accepted"
    );
    assert!(state.audit_plan.as_ref().is_some_and(|p| p.revision_audit));
    assert_eq!(state.stage, Stage::Reviewer);
    assert!(state.stuck_math_audit.revision_planning.is_none());

    // -- Frozen-node text edits are excluded by the edit-scope envelope. ---
    state.target_edit_mode = TargetEditMode::Targeted;
    state.active_node = Some(nid("WeakTheorem"));
    // (WeakTheorem does not exist yet; the envelope reports authorized ∩
    // editable over PRESENT nodes — the frozen carried tablet.)
    let plan = state.current_worker_validation_execution_plan();
    let (authorized_editable, frozen) = plan
        .iter()
        .find_map(|step| match step {
            WorkerValidationExecutionPlanStep::RevisionStatementEditScope {
                authorized_editable_nodes,
                frozen_nodes,
                ..
            } => Some((authorized_editable_nodes.clone(), frozen_nodes.clone())),
            _ => None,
        })
        .expect("RevisionStatementEditScope present in RevisionStating");
    for frozen_node in ["Preamble", "MainTheorem", "Aux", "Helper"] {
        assert!(frozen.contains(&nid(frozen_node)));
        assert!(
            !authorized_editable.contains(&nid(frozen_node)),
            "frozen node {frozen_node} must not be statement-editable"
        );
    }
    state.target_edit_mode = TargetEditMode::Global;
    state.active_node = None;

    // -- Statement work (model level): the worker states a NEW covering
    // node for thm:new AND claim-only-ties the frozen Helper into it.
    // Frozen nodes may gain target_claim_updates on ANY target
    // (reuse-via-claim); duplication is forbidden by substantiveness, not
    // by the freeze.
    state.live.present_nodes.insert(nid("WeakTheorem"));
    // The theorem-stating worker authors the statement and leaves its proof
    // body open for ProofFormalization; it is not a sorry-free closure record
    // owner yet.
    state.live.open_nodes.insert(nid("WeakTheorem"));
    state.node_kinds.insert(nid("WeakTheorem"), NodeKind::Proof);
    state.proof_nodes.insert(nid("WeakTheorem"));
    state
        .target_claims
        .insert(nid("WeakTheorem"), BTreeSet::from([tid("thm:new")]));
    state
        .target_claims
        .insert(nid("Helper"), BTreeSet::from([tid("thm:new")]));
    state.normalize_all_structural_state();
    state.ensure_node_metadata();
    assert_eq!(
        state.live.coverage.get(&tid("thm:new")),
        Some(&BTreeSet::from([nid("Helper"), nid("WeakTheorem")])),
        "new covering node + frozen-node claim-only tie both accepted"
    );

    // -- Paper lane verifies only the new target. --------------------------
    assert_eq!(state.blocked_targets(), BTreeSet::from([tid("thm:new")]));

    // Emulate the verifier lanes passing the new statement work (paper for
    // thm:new; corr/sound/substantiveness for WeakTheorem), as the normal
    // pipeline would before the Advance gate becomes legal.
    state
        .live
        .paper_current_fingerprints
        .insert(tid("thm:new"), "w=fp".to_string());
    state.paper_status.insert(tid("thm:new"), CorrStatus::Pass);
    state
        .paper_approved_fingerprints
        .insert(tid("thm:new"), "w=fp".to_string());
    let w = nid("WeakTheorem");
    state
        .live
        .corr_current_fingerprints
        .insert(w.clone(), "corr-w".into());
    state
        .corr_approved_fingerprints
        .insert(w.clone(), "corr-w".into());
    state.corr_status.insert(w.clone(), CorrStatus::Pass);
    state
        .live
        .sound_current_fingerprints
        .insert(w.clone(), "sound-w".into());
    state
        .sound_approved_fingerprints
        .insert(w.clone(), "sound-w".into());
    state.sound_status.insert(w.clone(), trellis_kernel::SoundStatus::Pass);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(w.clone(), "subst-w".into());
    state
        .substantiveness_approved_fingerprints
        .insert(w.clone(), "subst-w".into());
    state.substantiveness_status.insert(w.clone(), trellis_kernel::SubstantivenessStatus::Pass);
    state.commit_live();
    assert!(state.global_blockers().is_empty(), "gate-legal state");

    // Snapshot the pre-gate approved paper pins for byte-equality.
    let pre_gate_approved = state.paper_approved_fingerprints.clone();

    // -- Advance HumanGate -> ProofFormalization. --------------------------
    state.stage = Stage::HumanGate;
    state.gate_kind = GateKind::Advance;
    let gate_request = state.issue_request(RequestKind::HumanGate);
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::HumanGate(HumanGateResponse {
                request_id: gate_request.id,
                cycle: gate_request.cycle,
                status: ResponseStatus::Ok,
                choice: HumanChoice::Approve,
            }),
        },
    )
    .expect("HumanGate approve");
    let state = outcome.state;

    assert_eq!(state.phase, Phase::ProofFormalization, "PF resumes");
    // approved_targets gains the new entries…
    assert!(state
        .approved_targets
        .configured_targets
        .contains(&tid("thm:new")));
    assert_eq!(
        state.approved_targets.coverage.get(&tid("thm:new")),
        Some(&BTreeSet::from([nid("Helper"), nid("WeakTheorem")]))
    );
    assert_eq!(
        state.paper_approved_fingerprints.get(&tid("thm:new")),
        Some(&"w=fp".to_string())
    );
    // …while every pre-existing approved entry compares byte-equal.
    for (target, fp) in &pre_gate_approved {
        assert_eq!(
            state.paper_approved_fingerprints.get(target),
            Some(fp),
            "pre-existing approved pin for {target} must be byte-equal"
        );
    }
    // Fresh full Cleanup at the end is accepted by design: the run re-enters
    // PF and will pass through Cleanup again before Complete.
    assert!(state.revision_context.is_some());
}

/// Live-confirmed newborn-editability bug (simplex run): a node BORN during
/// RevisionStating is in neither `frozen_nodes` nor the synthesis-time
/// `editable_nodes` snapshot, and the statement-edit envelope used to
/// intersect reviewer authorization with the snapshot — so every edit to a
/// newborn was rejected with authorization ∩ editable = ∅. Editability is
/// dynamic (present − frozen): a node stated in burst N is editable in burst
/// N+1 (Restructure, exactly the live incident shape), the frozen envelope
/// is unchanged, the rendered `revision_scope` view lists the newborn, and
/// the stored snapshot stays what it was (display/serde only).
#[test]
fn newborn_node_stated_in_burst_n_is_editable_in_burst_n_plus_1() {
    let mut state = complete_fixture();
    revive(&mut state);

    // Planner round (as in the sibling full-flow test).
    let outcome = apply_event(state, ProtocolEvent::StartCycle).expect("StartCycle");
    let state = outcome.state;
    let request = match outcome.commands.as_slice() {
        [ProtocolCommand::IssueRequest { request }] => request.clone(),
        other => panic!("expected a StuckMathAudit request, got {other:?}"),
    };
    let mut response = planner_response();
    response.request_id = request.id;
    response.cycle = request.cycle;
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::StuckMathAudit(response),
        },
    )
    .expect("planner response accepted");
    let mut state = outcome.state;

    // The synthesis-time snapshot is empty here: the carried tablet is fully
    // frozen at revival, and the snapshot never learns about later births.
    let snapshot = state
        .revision_context
        .as_ref()
        .expect("revision context present")
        .editable_nodes
        .clone();
    assert!(snapshot.is_empty(), "fixture: fully-frozen carried tablet");

    // -- Burst N: the worker states the newborn covering node. -------------
    state.live.present_nodes.insert(nid("WeakTheorem"));
    state.node_kinds.insert(nid("WeakTheorem"), NodeKind::Proof);
    state.proof_nodes.insert(nid("WeakTheorem"));
    state
        .target_claims
        .insert(nid("WeakTheorem"), BTreeSet::from([tid("thm:new")]));
    state.normalize_all_structural_state();
    state.ensure_node_metadata();

    // -- Burst N+1: the reviewer authorizes the newborn (Restructure). -----
    state.target_edit_mode = TargetEditMode::Restructure;
    state.active_node = Some(nid("WeakTheorem"));
    state.pending_task = Some(PendingTask {
        authorized_nodes: BTreeSet::from([nid("WeakTheorem")]),
        ..PendingTask::default()
    });
    let plan = state.current_worker_validation_execution_plan();
    let (authorized_editable, frozen) = plan
        .iter()
        .find_map(|step| match step {
            WorkerValidationExecutionPlanStep::RevisionStatementEditScope {
                authorized_editable_nodes,
                frozen_nodes,
                ..
            } => Some((authorized_editable_nodes.clone(), frozen_nodes.clone())),
            _ => None,
        })
        .expect("RevisionStatementEditScope present in RevisionStating");
    assert!(
        authorized_editable.contains(&nid("WeakTheorem")),
        "newborn stated in burst N must be statement-editable in burst N+1 \
         (dynamic present − frozen rule); got {authorized_editable:?}"
    );
    // The frozen envelope is untouched by the dynamic rule.
    for frozen_node in ["Preamble", "MainTheorem", "Aux", "Helper"] {
        assert!(frozen.contains(&nid(frozen_node)));
        assert!(
            !authorized_editable.contains(&nid(frozen_node)),
            "frozen node {frozen_node} must not become statement-editable"
        );
    }

    // The rendered worker/review payload lists the newborn as editable, so
    // the prompt-side `request_summary.revision_scope` matches enforcement.
    for kind in [RequestKind::Worker, RequestKind::Review] {
        let view = state
            .expected_request(0, kind)
            .revision_context
            .expect("revision view on RevisionStating Worker/Review");
        assert!(
            view.editable_nodes.contains(&nid("WeakTheorem")),
            "{kind:?} revision_scope must list the newborn as editable"
        );
        assert!(!view.editable_nodes.contains(&nid("MainTheorem")));
        assert!(view.frozen_nodes.contains(&nid("MainTheorem")));
    }

    // The stored snapshot is untouched (display/serde only).
    assert_eq!(
        state
            .revision_context
            .as_ref()
            .expect("revision context present")
            .editable_nodes,
        snapshot
    );
}

// ===== Uncovered-target orphan window (layered construction) =============

/// Full-flow revival shape for the uncovered-target orphan window: while
/// the added target is uncovered, support-first bursts may land live
/// orphan nodes; the covering statement closes the window (judged by the
/// PRE-burst window, so its own leftovers are tolerated); after the
/// close, a fresh orphan-creating burst is rejected exactly as b67ccbf
/// prescribes while the window-era leftover stays tolerated and
/// sweepable.
#[test]
fn full_flow_window_permits_support_first_then_closes_on_covering_statement() {
    let mut state = complete_fixture();
    revive(&mut state);

    // Revival leaves thm:new configured and uncovered: window OPEN.
    assert!(state.orphan_construction_window_open(&state.live));

    // Planner round (as in the sibling full-flow test).
    let outcome = apply_event(state, ProtocolEvent::StartCycle).expect("StartCycle");
    let state = outcome.state;
    let request = match outcome.commands.as_slice() {
        [ProtocolCommand::IssueRequest { request }] => request.clone(),
        other => panic!("expected a StuckMathAudit request, got {other:?}"),
    };
    let mut response = planner_response();
    response.request_id = request.id;
    response.cycle = request.cycle;
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::StuckMathAudit(response),
        },
    )
    .expect("planner response accepted");
    let mut state = outcome.state;
    assert_eq!(state.stage, Stage::Reviewer);

    // -- Burst 1 (window open): support-first. The worker states
    // SupportLemma with NO claim and NO importer — a live orphan the
    // closed-window contract would reject — and is accepted.
    state.stage = Stage::Worker;
    let request = state.issue_request(RequestKind::Worker);
    let mut snapshot = state.live.clone();
    snapshot.present_nodes.insert(nid("SupportLemma"));
    snapshot.open_nodes.insert(nid("SupportLemma"));
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::Worker(trellis_kernel::WorkerResponse {
                request_id: request.id,
                cycle: request.cycle,
                status: ResponseStatus::Ok,
                outcome: trellis_kernel::WorkerOutcome::Valid,
                snapshot,
                dep_updates: BTreeMap::from([(
                    nid("SupportLemma"),
                    trellis_kernel::Update::Set(BTreeSet::from([nid("Preamble")])),
                )]),
                ..trellis_kernel::WorkerResponse::default()
            }),
        },
    )
    .expect("support-first burst applies");
    let mut state = outcome.state;
    assert!(
        state.deterministic_worker_rejection_reasons.is_empty(),
        "window-open support-first burst must be accepted; got {:?}",
        state.deterministic_worker_rejection_reasons
    );
    assert_ne!(state.stage, Stage::Worker, "no retry bounce");
    assert!(
        state.orphan_cleanup_needed(),
        "raw detection still sees the layered orphan"
    );
    assert!(
        !state.orphan_cleanup_active(),
        "no orphan-cleanup task parked while the window is open"
    );
    assert!(state.orphan_construction_window_open(&state.live));

    // -- Burst 2 (window open): the covering statement lands WITHOUT
    // importing SupportLemma. Judged by the PRE-burst window (open), so
    // the burst that closes the window is accepted even though it leaves
    // SupportLemma orphaned.
    state.stage = Stage::Worker;
    let request = state.issue_request(RequestKind::Worker);
    let mut snapshot = state.live.clone();
    snapshot.present_nodes.insert(nid("WeakTheorem"));
    snapshot.open_nodes.insert(nid("WeakTheorem"));
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::Worker(trellis_kernel::WorkerResponse {
                request_id: request.id,
                cycle: request.cycle,
                status: ResponseStatus::Ok,
                outcome: trellis_kernel::WorkerOutcome::Valid,
                snapshot,
                dep_updates: BTreeMap::from([(
                    nid("WeakTheorem"),
                    trellis_kernel::Update::Set(BTreeSet::from([nid("Preamble")])),
                )]),
                target_claim_updates: BTreeMap::from([(
                    nid("WeakTheorem"),
                    trellis_kernel::Update::Set(BTreeSet::from([tid("thm:new")])),
                )]),
                ..trellis_kernel::WorkerResponse::default()
            }),
        },
    )
    .expect("covering burst applies");
    let mut state = outcome.state;
    assert!(
        state.deterministic_worker_rejection_reasons.is_empty(),
        "the window-closing burst must not be judged by the window it closed; got {:?}",
        state.deterministic_worker_rejection_reasons
    );
    assert!(
        !state.orphan_construction_window_open(&state.live),
        "covering statement closes the window"
    );
    assert!(
        state.live.present_nodes.contains(&nid("SupportLemma")),
        "the window-era leftover survives as a tolerated orphan"
    );

    // -- Burst 3 (window closed): a NEW orphan-creating burst is rejected
    // with the b67ccbf reason, while the leftover stays tolerated.
    state.stage = Stage::Worker;
    let request = state.issue_request(RequestKind::Worker);
    let mut snapshot = state.live.clone();
    snapshot.present_nodes.insert(nid("StragglerOrphan"));
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::Worker(trellis_kernel::WorkerResponse {
                request_id: request.id,
                cycle: request.cycle,
                status: ResponseStatus::Ok,
                outcome: trellis_kernel::WorkerOutcome::Valid,
                snapshot,
                dep_updates: BTreeMap::from([(
                    nid("StragglerOrphan"),
                    trellis_kernel::Update::Set(BTreeSet::from([nid("Preamble")])),
                )]),
                ..trellis_kernel::WorkerResponse::default()
            }),
        },
    )
    .expect("post-close orphan burst processed");
    let retry_request = outcome
        .commands
        .iter()
        .find_map(|command| match command {
            ProtocolCommand::IssueRequest { request } if request.kind == RequestKind::Worker => {
                Some(request.clone())
            }
            _ => None,
        })
        .expect("rejection re-issues a Worker retry request");
    let state = outcome.state;
    assert_eq!(state.stage, Stage::Worker, "post-close orphan bursts bounce to retry");
    assert!(
        state
            .deterministic_worker_rejection_reasons
            .iter()
            .any(|r| r.contains("NEW live orphan nodes") && r.contains("StragglerOrphan")),
        "rejection must name only the NEW orphan; got {:?}",
        state.deterministic_worker_rejection_reasons
    );
    assert!(
        !state
            .deterministic_worker_rejection_reasons
            .iter()
            .any(|r| r.contains("SupportLemma")),
        "the tolerated window-era leftover must not be blamed; got {:?}",
        state.deterministic_worker_rejection_reasons
    );

    // -- Burst 4 (window closed): the leftover is sweepable — the retry
    // burst deleting SupportLemma is accepted.
    let mut snapshot = state.live.clone();
    snapshot.present_nodes.remove(&nid("SupportLemma"));
    snapshot.open_nodes.remove(&nid("SupportLemma"));
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::Worker(trellis_kernel::WorkerResponse {
                request_id: retry_request.id,
                cycle: retry_request.cycle,
                status: ResponseStatus::Ok,
                outcome: trellis_kernel::WorkerOutcome::Valid,
                snapshot,
                deleted_nodes: BTreeSet::from([nid("SupportLemma")]),
                ..trellis_kernel::WorkerResponse::default()
            }),
        },
    )
    .expect("sweep burst applies");
    let state = outcome.state;
    assert!(
        state.deterministic_worker_rejection_reasons.is_empty(),
        "sweeping the window-era leftover must be accepted; got {:?}",
        state.deterministic_worker_rejection_reasons
    );
    assert!(!state.live.present_nodes.contains(&nid("SupportLemma")));
    // Only the fixture's own standing orphan remains (`Helper` sits in
    // lem:aux's protected closure with no importer and no claim — a
    // pre-existing orphan since before the revival, untouched by the
    // window machinery).
    assert_eq!(
        state.orphan_nodes(&state.live),
        BTreeSet::from([nid("Helper")]),
        "window-era leftovers swept post-window"
    );
}

// ===== Runtime-CLI action end-to-end ======================================

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, text).expect("write file");
}

fn paper_text() -> &'static str {
    "\\begin{document}\n\
     \\begin{theorem}\\label{thm:main}\nMain bound.\n\\end{theorem}\n\
     \\begin{corollary}\\label{lem:aux}\nAuxiliary fact.\n\\end{corollary}\n\
     \\begin{theorem}\\label{thm:new}\nNew result.\n\\end{theorem}\n\
     \\end{document}\n"
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git")
}

fn run_cli(request: &Value) -> (Value, bool) {
    let exe = env!("CARGO_BIN_EXE_trellis_runtime_cli");
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn runtime cli");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait");
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "cli stdout not JSON: {err}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (value, output.status.success())
}

/// Seed a runtime root + run repo (git worktree, clean tag, Complete fixture,
/// hand-written event log) for the `add_paper_targets` CLI action, with
/// `workflow` as the config's workflow block. Returns (root, repo,
/// config_path).
fn seed_cli_fixture(tmp: &Path, workflow: Value) -> (PathBuf, PathBuf, PathBuf) {
    seed_cli_fixture_under_git_root(tmp, "repo", "repo", workflow)
}

/// As `seed_cli_fixture`, but the git worktree ROOT is `git_root_rel` while the
/// run repo is `repo_rel` — a non-empty `git rev-parse --show-prefix`, which is
/// the layout that distinguishes a pathspec from a toplevel-relative path.
fn seed_cli_fixture_under_git_root(
    tmp: &Path,
    repo_rel: &str,
    git_root_rel: &str,
    workflow: Value,
) -> (PathBuf, PathBuf, PathBuf) {
    let root = tmp.join("runtime");
    let repo = tmp.join(repo_rel);
    let git_root = tmp.join(git_root_rel);
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&git_root).unwrap();
    write_file(&repo.join("paper/main.tex"), paper_text());

    // Config lives in a git worktree (the run-repo pattern) so the action's
    // config commit path is exercised.
    let config_path = repo.join("trellis.config.json");
    write_file(
        &config_path,
        &serde_json::to_string_pretty(&serde_json::json!({
            "repo_path": ".",
            "workflow": workflow,
        }))
        .unwrap(),
    );
    assert!(git(&git_root, &["init", "-q"]).status.success());
    assert!(git(&repo, &["config", "user.email", "t@example.com"]).status.success());
    assert!(git(&repo, &["config", "user.name", "t"]).status.success());
    assert!(git(&repo, &["add", "-A"]).status.success());
    assert!(git(&repo, &["commit", "-q", "-m", "init"]).status.success());
    // The fixture state has LastClean mirrors populated; runtime load's
    // tag-consistency check requires a matching clean tag in the run repo.
    assert!(git(&repo, &["tag", "supervisor2/clean-000001"]).status.success());

    // Runtime root with the Complete fixture + a continuous event log.
    let state = complete_fixture();
    let metadata = RuntimeMetadata {
        repo_path: Some(repo.clone()),
        config_path: Some(config_path.clone()),
        native_history_kinds: Default::default(),
        initial_planning_seeded: false,
        ..RuntimeMetadata::default()
    };
    let runtime =
        SupervisorRuntime::initialize_with_metadata(RuntimePaths::new(root.clone()), state, metadata)
            .expect("initialize runtime");
    let event_count_before = runtime.event_count();
    drop(runtime);
    let event_log = repo.join(".trellis-history/event-log/cycle-000042.jsonl");
    let record = EventLogRecord {
        index: 0,
        event: ProtocolEvent::StartCycle,
        commands: vec![],
        phase: Phase::Complete,
        stage: Stage::Complete,
        cycle: 42,
        ts_ms: 0,
        trust_record: None,
        additional_trust_records: Vec::new(),
    };
    write_file(
        &event_log,
        &(serde_json::to_string(&record).unwrap() + "\n"),
    );
    assert_eq!(event_count_before, 0, "fixture wrote the log by hand");
    (root, repo, config_path)
}

/// Drive the `add_paper_targets` runtime-CLI action against a real runtime
/// root: state revived + persisted, config rewritten with the full resolved
/// target list and committed in git, event log untouched, second run errors.
#[test]
fn cli_action_end_to_end() {
    let tmp = common::project_tempdir();
    let (root, repo, config_path) = seed_cli_fixture(
        tmp.path(),
        serde_json::json!({
            "paper_tex_path": "paper/main.tex",
            "main_result_targets": [
                {"start_line": 2, "end_line": 4, "tex_label": "thm:main"},
                {"start_line": 5, "end_line": 7, "tex_label": "lem:aux"}
            ],
            "main_result_labels": ["thm:main", "lem:aux", "thm:new"]
        }),
    );

    let request = serde_json::json!({
        "action": "add_paper_targets",
        "root": root,
        "config_path": config_path,
    });
    let (response, ok) = run_cli(&request);
    assert!(ok, "action must succeed: {response:#}");
    assert_eq!(response["status"], "add_paper_targets_ok");
    assert_eq!(response["summary"]["added_targets"], serde_json::json!(["thm:new"]));
    assert_eq!(response["state"]["phase"], "RevisionStating");
    assert_eq!(response["state"]["cycle"], 42);
    assert_eq!(response["event_count"], 1, "event log untouched");
    // Loud notes: git commit outcome reported.
    let notes = response["notes"].as_array().expect("notes array");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or_default().contains("committed")),
        "config git-commit note expected; got {notes:?}"
    );

    // Persisted state is the revived one.
    let persisted: ProtocolState = serde_json::from_str(
        &fs::read_to_string(root.join("protocol_state.json")).expect("read persisted state"),
    )
    .expect("parse persisted state");
    assert_eq!(persisted.phase, Phase::RevisionStating);
    assert!(persisted.configured_targets.contains(&tid("thm:new")));
    assert!(persisted.stuck_math_audit.revision_planning.is_some());

    // Config rewritten with the FULL resolved list and committed in git.
    let rewritten: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let targets = rewritten["workflow"]["main_result_targets"]
        .as_array()
        .expect("targets array");
    let labels: BTreeSet<&str> = targets
        .iter()
        .filter_map(|t| t["tex_label"].as_str())
        .collect();
    assert_eq!(labels, BTreeSet::from(["thm:main", "lem:aux", "thm:new"]));
    for target in targets {
        assert!(target["start_line"].as_i64().unwrap() > 0);
        assert!(target["end_line"].as_i64().unwrap() > 0);
    }
    let status = git(&repo, &["status", "--porcelain", "--", "trellis.config.json"]);
    assert!(
        String::from_utf8_lossy(&status.stdout).trim().is_empty(),
        "config rewrite must be committed (clean git status)"
    );
    let log = git(&repo, &["log", "-1", "--pretty=%s"]);
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("add_paper_targets"),
        "config commit present"
    );

    // Second identical run: labels already configured -> added-∅ hard error.
    let (response, ok) = run_cli(&request);
    assert!(!ok, "second run must fail");
    assert_eq!(response["status"], "error");
    assert!(
        response["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no new targets to add"),
        "got: {response:#}"
    );
}

/// The action re-resolves under `workflow.main_result_envs` — the same config
/// key `load_config` reads — so a narrowed set makes an out-of-set label
/// unresolvable instead of silently resolving under the default pair.
#[test]
fn cli_action_resolves_under_the_configured_env_set() {
    let tmp = common::project_tempdir();
    let (root, _repo, config_path) = seed_cli_fixture(
        tmp.path(),
        serde_json::json!({
            "paper_tex_path": "paper/main.tex",
            // `lem:aux` labels a corollary block in the fixture paper.
            "main_result_envs": ["theorem"],
            "main_result_targets": [
                {"start_line": 2, "end_line": 4, "tex_label": "thm:main"},
                {"start_line": 5, "end_line": 7, "tex_label": "lem:aux"}
            ],
            "main_result_labels": ["thm:main", "lem:aux", "thm:new"]
        }),
    );

    let (response, ok) = run_cli(&serde_json::json!({
        "action": "add_paper_targets",
        "root": root,
        "config_path": config_path,
    }));
    assert!(!ok, "narrowed env set must fail loudly: {response:#}");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(message.contains("lem:aux"), "got: {response:#}");
    assert!(
        message.contains("scans theorem environments only"),
        "got: {response:#}"
    );
}

#[test]
fn cli_action_rejects_a_non_canonical_configured_env() {
    let tmp = common::project_tempdir();
    let (root, _repo, config_path) = seed_cli_fixture(
        tmp.path(),
        serde_json::json!({
            "paper_tex_path": "paper/main.tex",
            "main_result_envs": ["theorem", "conjecture"],
            "main_result_labels": ["thm:main", "lem:aux", "thm:new"]
        }),
    );

    let (response, ok) = run_cli(&serde_json::json!({
        "action": "add_paper_targets",
        "root": root,
        "config_path": config_path,
    }));
    assert!(!ok, "non-canonical env must fail: {response:#}");
    assert!(
        response["message"]
            .as_str()
            .unwrap_or_default()
            .contains("`conjecture`"),
        "got: {response:#}"
    );
}

/// Drift tripwire, benign case: the operator edited the paper after setup, so
/// every range below the edit shifts. Content is identical, so the action
/// absorbs it — re-recording the new range and saying so in `notes`.
#[test]
fn cli_action_absorbs_a_benign_paper_line_shift() {
    let tmp = common::project_tempdir();
    let (root, repo, config_path) = seed_cli_fixture(
        tmp.path(),
        serde_json::json!({
            "paper_tex_path": "paper/main.tex",
            "main_result_targets": [
                {"start_line": 2, "end_line": 4, "tex_label": "thm:main"},
                {"start_line": 5, "end_line": 7, "tex_label": "lem:aux"}
            ],
            "main_result_labels": ["thm:main", "lem:aux", "thm:new"]
        }),
    );
    // Two lines of prose inserted above the first theorem: everything below
    // moves down by two, no statement text changes.
    let shifted = paper_text().replacen(
        "\\begin{document}\n",
        "\\begin{document}\nIntroductory paragraph.\n\n",
        1,
    );
    write_file(&repo.join("paper/main.tex"), &shifted);

    let (response, ok) = run_cli(&serde_json::json!({
        "action": "add_paper_targets",
        "root": root,
        "config_path": config_path,
    }));
    assert!(ok, "a pure line shift must not block the action: {response:#}");
    let notes = response["notes"].as_array().expect("notes array");
    let shift_note = notes
        .iter()
        .filter_map(Value::as_str)
        .find(|note| note.contains("`thm:main`"))
        .unwrap_or_else(|| panic!("expected a shift note for thm:main; got {notes:?}"));
    assert!(shift_note.contains("lines 2-4"), "{shift_note}");
    assert!(shift_note.contains("lines 4-6"), "{shift_note}");
    assert!(
        shift_note.contains("identical statement content"),
        "{shift_note}"
    );

    // Re-recorded: the rewritten config carries the NEW ranges.
    let rewritten: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    let main = rewritten["workflow"]["main_result_targets"]
        .as_array()
        .expect("targets array")
        .iter()
        .find(|target| target["tex_label"] == "thm:main")
        .expect("thm:main recorded");
    assert_eq!(main["start_line"], 4);
    assert_eq!(main["end_line"], 6);
}

/// Drift tripwire, hazard case: a widened env set rebinds an existing label to
/// an enclosing block. Content differs, so the action hard-errors naming both
/// ranges and both statements, and mutates nothing.
#[test]
fn cli_action_hard_errors_when_a_target_rebinds_to_different_content() {
    let tmp = common::project_tempdir();
    let (root, repo, config_path) = seed_cli_fixture(
        tmp.path(),
        serde_json::json!({
            "paper_tex_path": "paper/main.tex",
            "main_result_targets": [
                {"start_line": 2, "end_line": 4, "tex_label": "thm:main"},
                {"start_line": 5, "end_line": 7, "tex_label": "lem:aux"}
            ],
            "main_result_labels": ["thm:main", "lem:aux", "thm:new"]
        }),
    );
    // The paper now wraps thm:main in a proposition, and the env set was
    // widened to recognize propositions: the outer block swallows the theorem
    // and inherits its label.
    write_file(
        &repo.join("paper/main.tex"),
        "\\begin{document}\n\
         \\begin{proposition}\n\
         Framing prose.\n\
         \\begin{theorem}\\label{thm:main}\n\
         Main bound.\n\
         \\end{theorem}\n\
         \\end{proposition}\n\
         \\begin{corollary}\\label{lem:aux}\n\
         Auxiliary fact.\n\
         \\end{corollary}\n\
         \\begin{theorem}\\label{thm:new}\n\
         New result.\n\
         \\end{theorem}\n\
         \\end{document}\n",
    );
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config["workflow"]["main_result_envs"] =
        serde_json::json!(["theorem", "corollary", "proposition"]);
    let config_before = serde_json::to_string_pretty(&config).unwrap();
    write_file(&config_path, &config_before);

    let (response, ok) = run_cli(&serde_json::json!({
        "action": "add_paper_targets",
        "root": root,
        "config_path": config_path,
    }));
    assert!(!ok, "a rebinding must fail: {response:#}");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("`thm:main` now denotes DIFFERENT content"),
        "got: {response:#}"
    );
    assert!(message.contains("lines 2-4"), "got: {response:#}");
    assert!(message.contains("lines 2-7"), "got: {response:#}");
    assert!(message.contains("Framing prose."), "got: {response:#}");

    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        config_before,
        "config untouched on drift"
    );
    let persisted: ProtocolState = serde_json::from_str(
        &fs::read_to_string(root.join("protocol_state.json")).expect("read persisted state"),
    )
    .expect("parse persisted state");
    assert_eq!(persisted.phase, Phase::Complete, "state untouched on drift");
}

/// Regression (audit F1): a rebinding that PRESERVES the recorded line range.
/// The one-line-style proposition wrapper resolves `thm:main` to lines 2-4
/// under the default envs and still lines 2-4 once `proposition` is
/// recognized — but it now denotes the wider block. Line numbers must not be
/// mistaken for evidence of an unchanged denotation.
#[test]
fn cli_action_hard_errors_on_a_rebinding_that_keeps_the_same_line_range() {
    let tmp = common::project_tempdir();
    let (root, repo, config_path) = seed_cli_fixture(
        tmp.path(),
        serde_json::json!({
            "paper_tex_path": "paper/main.tex",
            "main_result_targets": [
                {"start_line": 2, "end_line": 4, "tex_label": "thm:main"},
                {"start_line": 5, "end_line": 7, "tex_label": "lem:aux"}
            ],
            "main_result_labels": ["thm:main", "lem:aux", "thm:new"]
        }),
    );
    // `thm:main` still spans lines 2-4 — as the enclosing proposition.
    write_file(
        &repo.join("paper/main.tex"),
        "\\begin{document}\n\
         \\begin{proposition}Framing. \\begin{theorem}\\label{thm:main}\n\
         Main bound.\n\
         \\end{theorem} trailing prose\\end{proposition}\n\
         \\begin{corollary}\\label{lem:aux}\n\
         Auxiliary fact.\n\
         \\end{corollary}\n\
         \\begin{theorem}\\label{thm:new}\n\
         New result.\n\
         \\end{theorem}\n\
         \\end{document}\n",
    );
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config["workflow"]["main_result_envs"] =
        serde_json::json!(["theorem", "corollary", "proposition"]);
    let config_before = serde_json::to_string_pretty(&config).unwrap();
    write_file(&config_path, &config_before);

    let (response, ok) = run_cli(&serde_json::json!({
        "action": "add_paper_targets",
        "root": root,
        "config_path": config_path,
    }));
    assert!(!ok, "a same-range rebinding must fail: {response:#}");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("`thm:main` now denotes DIFFERENT content"),
        "got: {response:#}"
    );
    assert!(message.contains("Framing."), "got: {response:#}");
    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        config_before,
        "config untouched on drift"
    );
}

/// Regression (audit F3): the run repo is a SUBDIRECTORY of its git toplevel,
/// so `git rev-parse --show-prefix` is non-empty. A pathspec resolves against
/// the CWD while `git show <sha>:<path>` is toplevel-relative; conflating them
/// finds no config commit and turns every benign shift into a hard error.
#[test]
fn cli_action_recovers_content_when_the_repo_is_below_the_git_toplevel() {
    let tmp = common::project_tempdir();
    let (root, repo, config_path) = seed_cli_fixture_under_git_root(
        tmp.path(),
        "toplevel/run",
        "toplevel",
        serde_json::json!({
            "paper_tex_path": "paper/main.tex",
            "main_result_targets": [
                {"start_line": 2, "end_line": 4, "tex_label": "thm:main"},
                {"start_line": 5, "end_line": 7, "tex_label": "lem:aux"}
            ],
            "main_result_labels": ["thm:main", "lem:aux", "thm:new"]
        }),
    );
    let prefix = git(&repo, &["rev-parse", "--show-prefix"]);
    assert_eq!(
        String::from_utf8_lossy(&prefix.stdout).trim(),
        "run/",
        "fixture must exercise a non-empty worktree prefix"
    );
    let shifted = paper_text().replacen(
        "\\begin{document}\n",
        "\\begin{document}\nIntroductory paragraph.\n\n",
        1,
    );
    write_file(&repo.join("paper/main.tex"), &shifted);

    let (response, ok) = run_cli(&serde_json::json!({
        "action": "add_paper_targets",
        "root": root,
        "config_path": config_path,
    }));
    assert!(
        ok,
        "the as-recorded paper must be recoverable below the toplevel: {response:#}"
    );
    let notes = response["notes"].as_array().expect("notes array");
    assert!(
        notes
            .iter()
            .filter_map(Value::as_str)
            .any(|note| note.contains("`thm:main`") && note.contains("lines 4-6")),
        "expected a benign-shift note; got {notes:?}"
    );
}

/// Drift tripwire, unclassifiable case: the recorded range cannot be shown to
/// have denoted the target (here it lies past the end of the paper as
/// committed), so the action fails closed rather than guessing.
#[test]
fn cli_action_fails_closed_when_the_recorded_content_is_unrecoverable() {
    let tmp = common::project_tempdir();
    let (root, _repo, config_path) = seed_cli_fixture(
        tmp.path(),
        serde_json::json!({
            "paper_tex_path": "paper/main.tex",
            // `thm:main` really sits at lines 2-4 in the fixture paper.
            "main_result_targets": [
                {"start_line": 112, "end_line": 114, "tex_label": "thm:main"},
                {"start_line": 5, "end_line": 7, "tex_label": "lem:aux"}
            ],
            "main_result_labels": ["thm:main", "lem:aux", "thm:new"]
        }),
    );
    let config_before = fs::read_to_string(&config_path).unwrap();

    let (response, ok) = run_cli(&serde_json::json!({
        "action": "add_paper_targets",
        "root": root,
        "config_path": config_path,
    }));
    assert!(!ok, "an unclassifiable shift must fail: {response:#}");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(message.contains("`thm:main`"), "got: {response:#}");
    assert!(message.contains("could not be recovered"), "got: {response:#}");
    assert!(message.contains("lines 112-114"), "got: {response:#}");
    assert!(message.contains("lines 2-4"), "got: {response:#}");

    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        config_before,
        "config untouched on drift"
    );
    let persisted: ProtocolState = serde_json::from_str(
        &fs::read_to_string(root.join("protocol_state.json")).expect("read persisted state"),
    )
    .expect("parse persisted state");
    assert_eq!(persisted.phase, Phase::Complete, "state untouched on drift");
}

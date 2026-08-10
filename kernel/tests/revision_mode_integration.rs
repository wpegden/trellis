//! Revision-mode end-to-end integration fixture (`revision_plan.md` §15 step 13).
//!
//! Per-step revision-mode behavior is already unit-covered in the lib `mod
//! tests` of `revision_import.rs` / `engine.rs` / `model.rs` /
//! `runtime_cli_observations.rs`. This integration fixture chains the committed
//! steps through the kernel's own public APIs — the same surfaces the
//! supervisor drives — to pin the §15 flow:
//!
//!   import_revision_project                (steps 4-5, revision_import)
//!     -> RevisionStating / Start, frozen/editable, inherited approvals
//!   apply_event(StartCycle)
//!     -> revision-planning StuckMathAudit request
//!   apply_event(StuckMathAudit response)   (step 9, engine::apply_revision_planning_response)
//!     -> revision_audit AuditPlan, routes to Reviewer, dispositions recorded
//!   current_worker_validation_execution_plan (step 11, RevisionStatementEditScope)
//!     -> frozen nodes protected, edits restricted to authorized ∩ editable
//!   apply_event(HumanGate Approve)         (step 12)
//!     -> ProofFormalization, revision edit-scoping stops
//!
//! Constraints (per the task brief): a SMALL synthetic fixture (NOT the real
//! offdiagonal paper, NOT a real Lean build); no external supervisor / codex /
//! synthetic harness; no host `lake`; temp dirs are rooted under the build area
//! via `project_tempdir` (never `/tmp`); the helpers read files only.
//!
//! The §15 example: old paper has `thm:main` + an unchanged auxiliary `lem:aux`;
//! the new paper strengthens `thm:main`, keeps `lem:aux`, and adds `thm:weak`.
//! `MainTheorem` covers (changed) `thm:main`; `Aux` covers (unchanged) `lem:aux`
//! with `Helper` in its protected closure; both stay frozen and carried. The
//! planner restates `MainTheorem` and adds a `WeakTheorem` node covering
//! `thm:weak`.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use trellis_kernel::engine::{apply_event, ProtocolCommand, ProtocolEvent};
use trellis_kernel::revision_import::import_revision_project;
use trellis_kernel::{
    CorrStatus, GateKind, HumanChoice, HumanGateResponse, NodeId, NodeKind, Phase, ProtocolState,
    RequestKind, ResponseStatus, RevisionActions, RevisionNodeAction, RevisionNodeDisposition,
    RevisionTargetAction, RevisionTargetDeltaKind, Stage, StuckMathAuditResponse, TargetId,
    WorkerValidationExecutionPlanStep, WorkingSnapshot, AUDIT_REPORT_TEXT_MIN_CHARS,
};

fn nid(s: &str) -> NodeId {
    NodeId::from(s)
}

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    fs::write(path, text).expect("write fixture file");
}

fn doc(body: &str) -> String {
    format!("\\begin{{document}}\n{body}\n\\end{{document}}\n")
}

// thm:main strengthened (Changed), lem:aux unchanged, thm:weak added.
fn old_body() -> &'static str {
    "\\begin{theorem}\\label{thm:main}\nFor s>=4 the bound holds.\n\\end{theorem}\n\
     \\begin{lemma}\\label{lem:aux}\nAuxiliary fact.\n\\end{lemma}"
}
fn new_body() -> &'static str {
    "\\begin{theorem}\\label{thm:main}\nFor s>=3 the bound holds.\n\\end{theorem}\n\
     \\begin{lemma}\\label{lem:aux}\nAuxiliary fact.\n\\end{lemma}\n\
     \\begin{theorem}\\label{thm:weak}\nWeak result (the old main bound).\n\\end{theorem}"
}

/// A self-contained revision-import scenario on disk, rooted under the build
/// tempdir. Mirrors the proven `build_fixture` shape used by the
/// `revision_import` unit tests, but lives here so the integration test is
/// independent of crate-private helpers.
struct Scenario {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    full_state: PathBuf,
    old_paper: PathBuf,
    new_paper: PathBuf,
}

fn build_scenario() -> Scenario {
    let tmp = common::project_tempdir();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("Tablet")).unwrap();
    fs::create_dir_all(repo.join("paper/revision")).unwrap();

    // Tablet nodes on disk (read by the substantiveness observation only).
    write_file(&repo.join("Tablet/Preamble.lean"), "import Mathlib\n");
    write_file(&repo.join("Tablet/Preamble.tex"), "");
    for node in ["MainTheorem", "Aux", "Helper"] {
        write_file(
            &repo.join(format!("Tablet/{node}.lean")),
            "import Tablet.Preamble\ntheorem t : True := by trivial\n",
        );
        write_file(
            &repo.join(format!("Tablet/{node}.tex")),
            &format!("\\begin{{theorem}}\\label{{lbl}}{node} statement.\\end{{theorem}}\n"),
        );
    }

    let old_paper = repo.join("paper/revision/old.tex");
    let new_paper = repo.join("paper/revision/new.tex");
    write_file(&old_paper, &doc(old_body()));
    write_file(&new_paper, &doc(new_body()));

    // §3 invariant: workflow.paper_tex_path MUST equal new.tex.
    let config = tmp.path().join("trellis.config.json");
    write_file(
        &config,
        &format!(
            "{{\"repo_path\":\"{}\",\"workflow\":{{\"paper_tex_path\":\"paper/revision/new.tex\",\"main_result_targets\":[]}}}}",
            repo.display()
        ),
    );

    // Prior ProtocolState: a Complete run, two targets covered, all lanes
    // approved. `thm:main`/`MainTheorem`, `lem:aux`/`Aux` (+ `Helper` in its
    // protected closure).
    let mut state = ProtocolState::default();
    let present: BTreeSet<NodeId> = ["Preamble", "MainTheorem", "Aux", "Helper"]
        .iter()
        .map(|n| nid(n))
        .collect();
    let targets: BTreeSet<TargetId> = ["thm:main", "lem:aux"]
        .iter()
        .map(|t| TargetId::from(*t))
        .collect();
    let mut coverage: BTreeMap<TargetId, BTreeSet<NodeId>> = BTreeMap::new();
    coverage.insert(TargetId::from("thm:main"), BTreeSet::from([nid("MainTheorem")]));
    coverage.insert(TargetId::from("lem:aux"), BTreeSet::from([nid("Aux")]));
    let mut closure: BTreeMap<TargetId, BTreeSet<NodeId>> = BTreeMap::new();
    closure.insert(TargetId::from("lem:aux"), BTreeSet::from([nid("Helper")]));

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

    // target_claims drives coverage (the `live coverage must be derived from
    // target claims` invariant checked by engine::apply_event). thm:main is
    // covered by MainTheorem, lem:aux by Aux.
    let mut target_claims: BTreeMap<NodeId, BTreeSet<TargetId>> = BTreeMap::new();
    target_claims.insert(nid("MainTheorem"), BTreeSet::from([TargetId::from("thm:main")]));
    target_claims.insert(nid("Aux"), BTreeSet::from([TargetId::from("lem:aux")]));

    // proof_nodes must match the Proof-kind nodes (a validate() invariant).
    let proof_nodes: BTreeSet<NodeId> =
        ["MainTheorem", "Aux", "Helper"].iter().map(|n| nid(n)).collect();

    state.configured_targets = targets;
    state.target_claims = target_claims.clone();
    state.committed_target_claims = target_claims;
    state.node_kinds = node_kinds.clone();
    state.committed_node_kinds = node_kinds;
    state.proof_nodes = proof_nodes.clone();
    state.committed_proof_nodes = proof_nodes;
    // Live paper-faithfulness fingerprints must cover every configured target
    // (a validate() invariant). These are the tablet-side covering-node hashes;
    // their exact value is immaterial here, only their presence.
    let mut paper_fps: BTreeMap<TargetId, String> = BTreeMap::new();
    paper_fps.insert(TargetId::from("thm:main"), "pf-main".into());
    paper_fps.insert(TargetId::from("lem:aux"), "pf-aux".into());

    state.live = WorkingSnapshot {
        present_nodes: present.clone(),
        open_nodes: BTreeSet::new(),
        coverage: coverage.clone(),
        protected_closure_nodes_per_target: closure,
        paper_current_fingerprints: paper_fps,
        ..WorkingSnapshot::default()
    };
    state.committed = state.live.clone();
    for n in &present {
        state
            .corr_approved_fingerprints
            .insert(n.clone(), format!("corr-{}", n.as_str()));
        state.corr_status.insert(n.clone(), CorrStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert(n.clone(), format!("sound-{}", n.as_str()));
    }
    state
        .paper_approved_fingerprints
        .insert(TargetId::from("thm:main"), "paper-main".into());
    state
        .paper_approved_fingerprints
        .insert(TargetId::from("lem:aux"), "paper-aux".into());
    state
        .paper_status
        .insert(TargetId::from("thm:main"), CorrStatus::Pass);
    state
        .paper_status
        .insert(TargetId::from("lem:aux"), CorrStatus::Pass);
    state.phase = Phase::Complete;
    state.stage = Stage::Complete;

    let full_state = tmp.path().join("supervisor_state.json");
    write_file(&full_state, &serde_json::to_string(&state).unwrap());

    Scenario {
        _tmp: tmp,
        config,
        full_state,
        old_paper,
        new_paper,
    }
}

fn import(sc: &Scenario) -> ProtocolState {
    import_revision_project(
        &sc.config,
        &sc.full_state,
        &sc.old_paper,
        &sc.new_paper,
        "arXiv:v1",
        "arXiv:v3",
        None,
    )
    .expect("import_revision_project")
    .state
}

/// A valid revision-planner response for the imported scenario: restate the
/// strengthened `MainTheorem`, add a `WeakTheorem` covering `thm:weak`. The
/// report must clear `AUDIT_REPORT_TEXT_MIN_CHARS`.
fn planner_response() -> StuckMathAuditResponse {
    StuckMathAuditResponse {
        status: ResponseStatus::Ok,
        report: "## Claim being audited\n".to_string()
            + &"x".repeat(AUDIT_REPORT_TEXT_MIN_CHARS),
        revision_actions: RevisionActions {
            targets: vec![RevisionTargetAction {
                target: TargetId::from("thm:main"),
                classification: "strengthen".into(),
                covering_nodes: vec![nid("MainTheorem")],
            }],
            nodes: vec![
                RevisionNodeAction {
                    node: nid("MainTheorem"),
                    action: "restate".into(),
                    reason: "new paper strengthens the exponent".into(),
                },
                RevisionNodeAction {
                    node: nid("WeakTheorem"),
                    action: "new".into(),
                    reason: "old main result preserved as a stepping stone".into(),
                },
            ],
        },
        ..StuckMathAuditResponse::default()
    }
}

/// Full §15 flow: import -> accepted planner -> worker-validation scope ->
/// HumanGate -> ProofFormalization, asserting the protections and inheritance
/// at each transition.
#[test]
fn revision_flow_reaches_proof_formalization_through_human_gate() {
    let sc = build_scenario();

    // -- import (steps 4-5) ------------------------------------------------
    let state = import(&sc);
    assert_eq!(state.phase, Phase::RevisionStating);
    assert_eq!(state.stage, Stage::Start);
    assert!(state.stuck_math_audit.active);
    // The first pending audit IS the revision planner (mutually exclusive with
    // the other StuckMathAudit scenarios). StartCycle performs the actual
    // StuckMathAudit dispatch so cycle bookkeeping and Start cleanup run first.
    let planning = state
        .stuck_math_audit
        .revision_planning
        .as_ref()
        .expect("revision_planning seeded at import");
    assert!(state.stuck_math_audit.need_input_audit.is_none());
    assert!(state.stuck_math_audit.gap_research.is_none());
    assert!(state.stuck_math_audit.gap_plan_critique.is_none());
    assert!(state.pending_global_repair_request.is_none());
    let ctx = state.revision_context.as_ref().unwrap();
    assert_eq!(planning.target_deltas, ctx.target_deltas);

    // Frozen: Preamble + the unchanged lem:aux covering/closure nodes are
    // protected before any worker acts. Changed thm:main's covering node stays
    // editable.
    assert!(ctx.frozen_nodes.contains(&nid("Preamble")));
    assert!(ctx.frozen_nodes.contains(&nid("Aux")));
    assert!(ctx.frozen_nodes.contains(&nid("Helper")));
    assert!(ctx.editable_nodes.contains(&nid("MainTheorem")));
    assert!(!ctx.frozen_nodes.contains(&nid("MainTheorem")));

    // Target deltas: thm:main Changed, lem:aux Unchanged, thm:weak Added.
    assert_eq!(
        ctx.target_deltas[&TargetId::from("thm:main")].kind,
        RevisionTargetDeltaKind::Changed
    );
    assert_eq!(
        ctx.target_deltas[&TargetId::from("lem:aux")].kind,
        RevisionTargetDeltaKind::Unchanged
    );
    assert_eq!(
        ctx.target_deltas[&TargetId::from("thm:weak")].kind,
        RevisionTargetDeltaKind::Added
    );

    // Inheritance: corr/sound carried verbatim for every node; paper inherited
    // for the unchanged target but force-invalidated for the changed one;
    // substantiveness re-baselined to Pass for every present node so a pure
    // paper-version swap reopens nothing.
    assert_eq!(
        state.corr_approved_fingerprints.get(&nid("MainTheorem")),
        Some(&"corr-MainTheorem".to_string())
    );
    assert_eq!(
        state.sound_approved_fingerprints.get(&nid("Aux")),
        Some(&"sound-Aux".to_string())
    );
    assert_eq!(
        state.paper_approved_fingerprints.get(&TargetId::from("lem:aux")),
        Some(&"paper-aux".to_string())
    );
    assert!(!state
        .paper_approved_fingerprints
        .contains_key(&TargetId::from("thm:main")));
    assert_eq!(
        state.paper_status.get(&TargetId::from("thm:main")),
        Some(&CorrStatus::Unknown),
        "changed target re-enters the paper-faithfulness lane"
    );
    for node in &state.live.present_nodes {
        assert!(
            state.current_substantiveness_pass(node),
            "unchanged node {node} inherits substantiveness Pass"
        );
    }

    let outcome = apply_event(state, ProtocolEvent::StartCycle)
        .expect("StartCycle must dispatch the revision planner");
    let state = outcome.state;
    assert_eq!(state.stage, Stage::StuckMathAudit);
    let request = match outcome.commands.as_slice() {
        [ProtocolCommand::IssueRequest { request }] => {
            assert_eq!(request.kind, RequestKind::StuckMathAudit);
            assert!(request.stuck_math_audit.revision_planning.is_some());
            request.clone()
        }
        other => panic!("expected a single StuckMathAudit request, got {other:?}"),
    };

    // -- accepted planner (step 9) ----------------------------------------
    // Apply the planner response through the public engine API.
    let mut response = planner_response();
    response.request_id = request.id;
    response.cycle = request.cycle;

    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::StuckMathAudit(response),
        },
    )
    .expect("accepted revision-planning response");
    let mut state = outcome.state;

    assert_eq!(
        state.latest_stuck_math_audit_rejection_reason, "",
        "planner response must be accepted, not rejected"
    );
    // A revision_audit AuditPlan is written and the run routes to the reviewer,
    // surfacing the plan through the existing audit_plan plumbing.
    let plan = state.audit_plan.as_ref().expect("audit plan written");
    assert!(plan.revision_audit);
    assert!(!plan.need_input_audit);
    assert_eq!(state.stage, Stage::Reviewer);
    match outcome.commands.as_slice() {
        [ProtocolCommand::IssueRequest { request }] => {
            assert_eq!(request.kind, RequestKind::Review);
            assert!(request
                .audit_plan
                .as_ref()
                .is_some_and(|p| p.revision_audit));
        }
        other => panic!("expected a single Review request, got {other:?}"),
    }
    // The lane is cleared; dispositions recorded.
    assert!(state.stuck_math_audit.revision_planning.is_none());
    let ctx = state.revision_context.as_ref().unwrap();
    assert_eq!(
        ctx.node_dispositions.get(&nid("MainTheorem")),
        Some(&RevisionNodeDisposition::Restate)
    );
    assert_eq!(
        ctx.planner_target_actions[&TargetId::from("thm:main")].classification,
        "strengthen"
    );
    // MainTheorem stays editable (restate is not a narrowing); Aux still frozen.
    assert!(ctx.editable_nodes.contains(&nid("MainTheorem")));
    assert!(ctx.frozen_nodes.contains(&nid("Aux")));

    // -- worker-validation scope (step 11) --------------------------------
    // The reviewer routes the restatement burst at MainTheorem (Targeted edit
    // mode). In RevisionStating the worker-validation plan prepends a
    // RevisionStatementEditScope step carrying the frozen set and the
    // authorized ∩ editable envelope, so frozen nodes are protected and edits
    // are restricted.
    state.target_edit_mode = trellis_kernel::TargetEditMode::Targeted;
    state.active_node = Some(nid("MainTheorem"));
    let plan = state.current_worker_validation_execution_plan();
    let scope = plan
        .iter()
        .find_map(|step| match step {
            WorkerValidationExecutionPlanStep::RevisionStatementEditScope {
                authorized_editable_nodes,
                frozen_nodes,
                removed_targets,
                ..
            } => Some((authorized_editable_nodes, frozen_nodes, removed_targets)),
            _ => None,
        })
        .expect("RevisionStatementEditScope must be present in RevisionStating");
    let (authorized_editable, frozen, removed_targets) = scope;
    // MainTheorem is authorized AND editable -> in the envelope; Aux is
    // authorized but frozen -> excluded (protected). Helper/Preamble frozen.
    assert!(authorized_editable.contains(&nid("MainTheorem")));
    assert!(!authorized_editable.contains(&nid("Aux")));
    assert!(frozen.contains(&nid("Aux")));
    assert!(frozen.contains(&nid("Helper")));
    assert!(frozen.contains(&nid("Preamble")));
    // No Removed targets in this scenario.
    assert!(removed_targets.is_empty());

    // -- HumanGate -> ProofFormalization (step 12) ------------------------
    // After the revised statements pass, the reviewer raises the Advance gate.
    // HumanGate Approve transitions RevisionStating -> ProofFormalization and
    // snapshots coarse_dag_nodes from present nodes; the RevisionContext is
    // preserved as historical metadata and revision edit-scoping stops.
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
                trust_actor_authentication_receipt: None,
            }),
        },
    )
    .expect("HumanGate approve");
    let state = outcome.state;

    assert_eq!(state.phase, Phase::ProofFormalization);
    assert_eq!(
        state.coarse_dag_nodes,
        state.live.present_nodes,
        "coarse_dag_nodes snapshotted from present nodes at phase advance"
    );
    // RevisionContext preserved as historical metadata.
    assert!(state.revision_context.is_some());
    // Revision edit-scoping is RevisionStating-only: no RevisionStatementEditScope
    // step in ProofFormalization.
    assert!(
        !state
            .current_worker_validation_execution_plan()
            .iter()
            .any(|step| matches!(
                step,
                WorkerValidationExecutionPlanStep::RevisionStatementEditScope { .. }
            )),
        "revision edit-scoping must stop once ProofFormalization begins"
    );
}

/// HumanGate is mandatory before leaving RevisionStating: a rejected gate
/// (Feedback) keeps the run in RevisionStating rather than advancing or
/// dumping into a from-scratch TheoremStating run (the step-12 rejection-path
/// fix). Pins that the gate is a real barrier, not a pass-through.
#[test]
fn rejected_revision_gate_stays_in_revision_stating() {
    let sc = build_scenario();
    let mut state = import(&sc);
    state.phase = Phase::RevisionStating;
    state.stage = Stage::HumanGate;
    state.gate_kind = GateKind::Advance;
    let request = state.issue_request(RequestKind::HumanGate);

    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::HumanGate(HumanGateResponse {
                request_id: request.id,
                cycle: request.cycle,
                status: ResponseStatus::Ok,
                choice: HumanChoice::Feedback,
                trust_actor_authentication_receipt: None,
            }),
        },
    )
    .expect("HumanGate feedback");
    assert_eq!(
        outcome.state.phase,
        Phase::RevisionStating,
        "a rejected RevisionStating gate must return to RevisionStating review, \
         not advance and not reset to TheoremStating"
    );
    assert_ne!(outcome.state.phase, Phase::ProofFormalization);
}

// ===== Regression: non-revision workflows are unchanged =====

/// A normal fresh TheoremStating run carries no revision context and emits no
/// revision edit-scoping step. (`current_worker_validation_execution_plan`
/// gates `RevisionStatementEditScope` on `phase == RevisionStating`.)
#[test]
fn fresh_theorem_stating_run_has_no_revision_scoping() {
    let mut state = ProtocolState::default();
    state.phase = Phase::TheoremStating;
    state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("X")]);
    state.proof_nodes = BTreeSet::from([nid("X")]);
    assert!(state.revision_context.is_none());
    assert_eq!(
        state.expected_request_kind(),
        None,
        "a Start-stage fresh run dispatches via StartCycle, no in-flight kind yet"
    );
    assert!(
        !state
            .current_worker_validation_execution_plan()
            .iter()
            .any(|step| matches!(
                step,
                WorkerValidationExecutionPlanStep::RevisionStatementEditScope { .. }
            )),
        "TheoremStating must never carry a RevisionStatementEditScope step"
    );
}

/// A ProofFormalization run (the phase a revision run advances INTO) likewise
/// carries no revision edit-scoping, even if a stale RevisionContext is present.
#[test]
fn proof_formalization_has_no_revision_scoping_even_with_context() {
    let mut state = ProtocolState::default();
    state.phase = Phase::ProofFormalization;
    state.live.present_nodes = BTreeSet::from([nid("Preamble"), nid("X")]);
    state.proof_nodes = BTreeSet::from([nid("X")]);
    state.revision_context = Some(trellis_kernel::RevisionContext {
        frozen_nodes: BTreeSet::from([nid("Preamble")]),
        editable_nodes: BTreeSet::from([nid("X")]),
        ..Default::default()
    });
    assert!(
        !state
            .current_worker_validation_execution_plan()
            .iter()
            .any(|step| matches!(
                step,
                WorkerValidationExecutionPlanStep::RevisionStatementEditScope { .. }
            )),
        "ProofFormalization must not edit-scope even with a stale RevisionContext"
    );
}

/// Regression: an ordinary StuckMathAudit scenario (global repair) still routes
/// as before — the revision-planning lane does not perturb the other audit
/// arms. The revision lane is selected only when `revision_planning.is_some()`.
#[test]
fn ordinary_stuck_math_audit_lane_is_not_revision() {
    let mut state = ProtocolState::default();
    state.phase = Phase::TheoremStating;
    state.stage = Stage::StuckMathAudit;
    // No revision_planning set -> not a revision lane.
    assert!(state.stuck_math_audit.revision_planning.is_none());
    assert!(state.revision_context.is_none());
    // A non-revision StuckMathAudit response carrying revision_actions is
    // illegal (revision_actions are only legal for the revision-planning lane).
    let request = state.issue_request(RequestKind::StuckMathAudit);
    let mut response = planner_response();
    response.request_id = request.id;
    response.cycle = request.cycle;
    let outcome = apply_event(
        state,
        ProtocolEvent::WrapperResponse {
            response: trellis_kernel::WrapperResponse::StuckMathAudit(response),
        },
    );
    // The response is rejected (validation failure), not applied as a revision
    // plan: either the engine returns an error, or it records a rejection and
    // keeps the lane. Either way no revision_context is fabricated.
    if let Ok(outcome) = outcome {
        assert!(
            outcome.state.revision_context.is_none(),
            "an ordinary StuckMathAudit must never fabricate a revision context"
        );
        assert!(outcome
            .state
            .audit_plan
            .as_ref()
            .map(|p| !p.revision_audit)
            .unwrap_or(true));
    }
}

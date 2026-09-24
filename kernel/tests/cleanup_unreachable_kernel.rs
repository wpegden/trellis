use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::{Builder, TempDir};
use trellis_kernel::backend::BackendId;
use trellis_kernel::{
    apply_event, normalize_worker_response, CleanupAuditTask, CorrStatus, ElaborationCostRecord,
    LocalClosureRecord, NodeId, NodeKind, NoopCheckpointSink, PendingTask, Phase, ProtocolCommand,
    ProtocolEvent, ProtocolState, RuntimeCheckpoint, RuntimeMetadata, RuntimePaths, Stage,
    SupervisorRuntime, TargetId, WorkerNormalizationInput, WorkerResponse,
};

fn scratch_tempdir() -> TempDir {
    let root = match std::env::var_os("TMPDIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        Some(_) => panic!("explicit TMPDIR must not be empty"),
        None => std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
            .join("test-tmp"),
    };
    assert_ne!(root, Path::new("/"), "explicit TMPDIR must not be `/`");
    fs::create_dir_all(&root).expect("create scratch root");
    assert!(root.is_dir(), "test scratch root must be a directory");
    Builder::new()
        .prefix("cleanup-unreachable-")
        .tempdir_in(root)
        .expect("create scratch tempdir")
}

fn write_pair(repo: &Path, node: &str, lean: &str, tex: &str) {
    fs::write(repo.join("Tablet").join(format!("{node}.lean")), lean).expect("write Lean node");
    fs::write(repo.join("Tablet").join(format!("{node}.tex")), tex).expect("write TeX node");
}

/// Build the graph through the production worker normalizer. Only Preamble is
/// in the pre-burst state; Root and all three support nodes are discovered and
/// classified from their real on-disk pairs.
fn normalized_cleanup_fixture() -> (TempDir, PathBuf, ProtocolState) {
    let temp = scratch_tempdir();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("Tablet")).expect("create Tablet");
    fs::create_dir_all(repo.join(".trellis/scripts")).expect("create script directory");
    fs::write(
        repo.join(".trellis/scripts/check.py"),
        r#"#!/usr/bin/env python3
import json, sys
cmd = sys.argv[1]
if cmd == "sync-supervisor-workspace":
    print(json.dumps({"authoritative_repo_path": sys.argv[2], "supervisor_home": "", "supervisor_cache": ""}))
elif cmd == "sync-tablet-support":
    print(json.dumps({"updated_paths": [], "header_tex_path": "Tablet/header.tex", "index_md_path": "Tablet/INDEX.md", "readme_md_path": "Tablet/README.md"}))
elif cmd in ("prepare-compiled-support", "materialize-tablet-oleans"):
    print(json.dumps({"returncode": 0, "stdout": "", "stderr": "", "timed_out": False, "spawn_error": ""}))
else:
    raise SystemExit(f"unexpected command: {cmd}")
"#,
    )
    .expect("write checker stub");
    write_pair(&repo, "Preamble", "import Mathlib.Data.Nat.Basic\n", "");
    write_pair(
        &repo,
        "Root",
        "import Tablet.Preamble\n\ndef Root : Nat := 0\n",
        "\\begin{definition}Root.\\end{definition}\n",
    );
    write_pair(
        &repo,
        "MJoinIso",
        "import Tablet.AnticompletePairIso\n\ndef MJoinIso : Nat := 1\n",
        "\\begin{definition}MJoinIso.\\end{definition}\n",
    );
    write_pair(
        &repo,
        "AnticompletePairIso",
        "import Tablet.Preamble\n\ndef AnticompletePairIso : Nat := 2\n",
        "\\begin{definition}AnticompletePairIso.\\end{definition}\n",
    );
    write_pair(
        &repo,
        "Loose",
        "import Tablet.Preamble\n\ntheorem Loose : True := by\n  trivial\n",
        "\\begin{theorem}Loose.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
    );

    let target = TargetId::from("paper.root");
    let nodes = ["Root", "MJoinIso", "AnticompletePairIso", "Loose"];
    let target_claim_updates = nodes
        .iter()
        .map(|node| {
            let claims = if *node == "Root" {
                BTreeSet::from([target.clone()])
            } else {
                BTreeSet::new()
            };
            (NodeId::from(*node), claims)
        })
        .collect();
    let target_fingerprints = nodes
        .iter()
        .chain(std::iter::once(&"Preamble"))
        .map(|node| (NodeId::from(*node), format!("corr-{node}")))
        .collect();
    let normalized = normalize_worker_response(&WorkerNormalizationInput {
        repo_path: repo.clone(),
        configured_targets: BTreeSet::from([target.clone()]),
        current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
        current_node_kinds: BTreeMap::from([(NodeId::from("Preamble"), NodeKind::Preamble)]),
        current_target_claims: BTreeMap::from([(NodeId::from("Preamble"), BTreeSet::new())]),
        target_claim_updates,
        target_fingerprints,
        ..WorkerNormalizationInput::default()
    })
    .expect("run production worker normalizer");
    assert!(
        normalized.contract_errors.is_empty(),
        "normalizer rejected fixture: {:?}",
        normalized.contract_errors
    );

    let response = WorkerResponse {
        snapshot: normalized.snapshot,
        proof_node_updates: normalized.proof_node_updates,
        node_kind_updates: normalized.node_kind_updates,
        dep_updates: normalized.dep_updates,
        target_claim_updates: normalized.target_claim_updates,
        challenge_claim_updates: normalized.challenge_claim_updates,
        ..WorkerResponse::default()
    };
    let mut state = ProtocolState::default();
    state.phase = Phase::Cleanup;
    state.stage = Stage::Start;
    state.configured_targets = BTreeSet::from([target.clone()]);
    state
        .node_kinds
        .insert(NodeId::from("Preamble"), NodeKind::Preamble);
    state.live = response.snapshot.clone();
    state.apply_worker_structure_updates(&response);
    state
        .live
        .paper_current_fingerprints
        .insert(target.clone(), "paper-root".into());
    state.paper_status.insert(target.clone(), CorrStatus::Pass);
    state
        .paper_approved_fingerprints
        .insert(target.clone(), "paper-root".into());
    for node in state.live.present_nodes.clone() {
        if node.as_str() == "Preamble" {
            continue;
        }
        let fingerprint = state
            .live
            .corr_current_fingerprints
            .entry(node.clone())
            .or_insert_with(|| format!("corr-{node}"))
            .clone();
        state.corr_status.insert(node.clone(), CorrStatus::Pass);
        state.corr_approved_fingerprints.insert(node, fingerprint);
    }
    state.approved_targets.configured_targets = state.configured_targets.clone();
    state.approved_targets.coverage = state.live.coverage.clone();

    // Seed representative current node-indexed surfaces. The pass must prune
    // them along with the structural graph rather than leave stale telemetry
    // or reviewer evidence behind.
    let loose = NodeId::from("Loose");
    state.local_closure_records.insert(
        loose.clone(),
        LocalClosureRecord {
            node: loose.clone(),
            ..LocalClosureRecord::default()
        },
    );
    state.easy_attempts.insert(loose.clone(), 2);
    state
        .elaboration_cost_records
        .insert(loose.clone(), ElaborationCostRecord::default());
    state.node_target.insert(loose.clone(), BackendId::Lean);
    state
        .latest_corr_reviewer_evidence
        .insert(loose, BTreeMap::new());
    state.ensure_node_metadata();
    state.commit_live();
    state.stage = Stage::Start;
    state.in_flight_request = None;
    state
        .validate()
        .expect("normalized cleanup fixture is valid");
    (temp, repo, state)
}

#[test]
fn cleanup_plan_iterates_importer_frontiers_to_fixpoint() {
    let (_temp, _repo, state) = normalized_cleanup_fixture();
    let deletion = state
        .cleanup_unreachable_deletion_plan()
        .expect("plan")
        .expect("unreachable nodes");
    assert_eq!(
        deletion.rounds,
        vec![
            BTreeSet::from([NodeId::from("Loose"), NodeId::from("MJoinIso")]),
            BTreeSet::from([NodeId::from("AnticompletePairIso")]),
        ]
    );
    assert_eq!(
        deletion.deleted_nodes,
        BTreeSet::from([
            NodeId::from("AnticompletePairIso"),
            NodeId::from("Loose"),
            NodeId::from("MJoinIso"),
        ])
    );
    assert_eq!(deletion.deleted_nodes, state.orphan_nodes(&state.live));
}

#[test]
fn coverage_root_without_an_importer_is_never_deleted() {
    let (_temp, _repo, mut state) = normalized_cleanup_fixture();
    let deletion = state
        .cleanup_unreachable_deletion_plan()
        .expect("plan")
        .expect("unreachable nodes");
    assert!(!deletion.deleted_nodes.contains(&NodeId::from("Root")));
    assert!(!deletion.deleted_nodes.contains(&NodeId::from("Preamble")));

    let mut forged = deletion;
    forged.deleted_nodes.insert(NodeId::from("Root"));
    forged.rounds[0].insert(NodeId::from("Root"));
    forged
        .source_targets
        .insert(NodeId::from("Root"), BackendId::Lean);
    assert!(state
        .apply_cleanup_unreachable_deletion(&forged)
        .expect_err("coverage-root deletion must fail loudly")
        .contains("coverage root"));
}

#[test]
fn cleanup_without_present_coverage_roots_fails_without_planning_deletion() {
    let mut state = ProtocolState::default();
    state.phase = Phase::Cleanup;
    state.stage = Stage::Start;
    state.live.present_nodes = BTreeSet::from([
        NodeId::from("Preamble"),
        NodeId::from("UnrootedA"),
        NodeId::from("UnrootedB"),
    ]);
    state.deps.insert(
        NodeId::from("UnrootedA"),
        BTreeSet::from([NodeId::from("UnrootedB")]),
    );
    let before = state.clone();

    let error = state
        .cleanup_unreachable_deletion_plan()
        .expect_err("rootless Cleanup input must fail loudly");

    assert!(error.contains("no present paper or challenge coverage roots"));
    assert_eq!(state, before, "a rejected pure plan must change no state");
    let transition_error = apply_event(before, ProtocolEvent::StartCycle)
        .expect_err("the production Cleanup boundary must fail loudly too");
    assert!(
        format!("{transition_error:?}").contains("no present paper or challenge coverage roots")
    );
}

#[test]
fn cleanup_pass_is_idempotent_after_first_application() {
    let (_temp, _repo, mut state) = normalized_cleanup_fixture();
    let deletion = state
        .cleanup_unreachable_deletion_plan()
        .expect("plan")
        .expect("unreachable nodes");
    state
        .apply_cleanup_unreachable_deletion(&deletion)
        .expect("apply deletion");
    let clean = state.clone();
    assert_eq!(
        state
            .cleanup_unreachable_deletion_plan()
            .expect("second plan"),
        None
    );
    assert_eq!(state, clean, "the pure second pass must not mutate state");
}

#[test]
fn cleanup_projection_cancels_a_batch_if_any_member_is_deleted() {
    let (_temp, _repo, mut state) = normalized_cleanup_fixture();
    state.cleanup_audit_tasks = vec![
        CleanupAuditTask {
            target_node: NodeId::from("Root"),
            ..CleanupAuditTask::default()
        },
        CleanupAuditTask {
            target_node: NodeId::from("Loose"),
            ..CleanupAuditTask::default()
        },
    ];
    state.cleanup_active_batch = vec![0, 1];
    state.active_node = Some(NodeId::from("Root"));
    state.pending_task = Some(PendingTask {
        node: state.active_node.clone(),
        mode: trellis_kernel::TaskMode::Cleanup,
        authorized_nodes: BTreeSet::from([NodeId::from("Root"), NodeId::from("Loose")]),
        ..PendingTask::default()
    });

    let deletion = state
        .cleanup_unreachable_deletion_plan()
        .expect("plan")
        .expect("unreachable nodes");
    state
        .apply_cleanup_unreachable_deletion(&deletion)
        .expect("apply deletion");

    assert_eq!(state.cleanup_audit_tasks.len(), 1);
    assert_eq!(
        state.cleanup_audit_tasks[0].target_node,
        NodeId::from("Root")
    );
    assert!(state.cleanup_active_batch.is_empty());
    assert_eq!(state.cleanup_active_task, None);
    assert_eq!(state.pending_task, None);
    assert_eq!(state.active_node, None);
}

#[test]
fn cleanup_start_cycle_deletes_pairs_and_persists_coherent_audit_record() {
    let (temp, repo, state) = normalized_cleanup_fixture();
    let runtime_root = temp.path().join("runtime");
    let config_path = repo.join("trellis.config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "repo_path": repo,
            "worker": {"provider": "codex", "model": "worker", "label": "worker"},
            "reviewer": {"provider": "codex", "model": "reviewer", "label": "reviewer"},
            "workflow": {}
        })
        .to_string(),
    )
    .expect("write runtime config");
    let paths = RuntimePaths::new(&runtime_root);
    let mut runtime = SupervisorRuntime::initialize_with_metadata(
        paths.clone(),
        state,
        RuntimeMetadata {
            repo_path: Some(repo.clone()),
            config_path: Some(config_path),
            initial_planning_seeded: false,
            ..RuntimeMetadata::default()
        },
    )
    .expect("initialize runtime");
    let outcome = runtime
        .step_injected_event_with_checkpoint_sink(
            ProtocolEvent::StartCycle,
            &mut NoopCheckpointSink,
        )
        .expect("start cleanup cycle");

    let deletion = outcome
        .commands
        .iter()
        .find_map(|command| match command {
            ProtocolCommand::DeleteCleanupUnreachableNodePairs { deletion } => Some(deletion),
            _ => None,
        })
        .expect("kernel deletion command");
    assert_eq!(deletion.rounds.len(), 2);
    assert!(outcome
        .commands
        .iter()
        .any(|command| matches!(command, ProtocolCommand::CommitCheckpoint)));
    assert!(outcome.commands.iter().any(
        |command| matches!(command, ProtocolCommand::DeleteLocalClosureRecord { node } if node == &NodeId::from("Loose"))
    ));
    assert!(outcome.commands.iter().any(
        |command| matches!(command, ProtocolCommand::IssueRequest { request } if request.kind == trellis_kernel::RequestKind::Audit)
    ));

    for node in ["MJoinIso", "AnticompletePairIso", "Loose"] {
        assert!(!repo.join("Tablet").join(format!("{node}.lean")).exists());
        assert!(!repo.join("Tablet").join(format!("{node}.tex")).exists());
    }
    for node in ["Root", "Preamble"] {
        assert!(repo.join("Tablet").join(format!("{node}.lean")).is_file());
        assert!(repo.join("Tablet").join(format!("{node}.tex")).is_file());
    }

    let state = runtime.state();
    let present = &state.live.present_nodes;
    assert_eq!(
        present,
        &BTreeSet::from([NodeId::from("Preamble"), NodeId::from("Root")])
    );
    assert!(state
        .deps
        .iter()
        .all(|(node, deps)| present.contains(node) && deps.is_subset(present)));
    assert!(state.node_kinds.keys().all(|node| present.contains(node)));
    assert!(state
        .target_claims
        .keys()
        .all(|node| present.contains(node)));
    assert!(state.corr_status.keys().all(|node| present.contains(node)));
    assert!(state
        .corr_approved_fingerprints
        .keys()
        .all(|node| present.contains(node)));
    assert!(state
        .elaboration_cost_records
        .keys()
        .all(|node| present.contains(node)));
    assert!(state
        .node_difficulty
        .keys()
        .all(|node| present.contains(node)));
    assert!(state
        .easy_attempts
        .keys()
        .all(|node| present.contains(node)));
    assert!(state
        .latest_corr_reviewer_evidence
        .keys()
        .all(|node| present.contains(node)));
    assert!(state.node_target.keys().all(|node| present.contains(node)));
    assert!(state
        .local_closure_records
        .keys()
        .all(|node| present.contains(node)));
    assert_eq!(
        state.cleanup_unreachable_deletion_log.last(),
        Some(deletion)
    );

    let checkpoint: RuntimeCheckpoint = serde_json::from_slice(
        &fs::read(&paths.checkpoint_path).expect("checkpoint with deletion record"),
    )
    .expect("parse checkpoint");
    assert_eq!(
        checkpoint.cleanup_unreachable_deletion.as_ref(),
        Some(deletion)
    );
    let event_log = fs::read_to_string(trellis_kernel::event_log_cycle_file(
        &runtime.event_log_dir(),
        state.cycle,
    ))
    .expect("cleanup event log");
    assert!(event_log.contains("delete_cleanup_unreachable_node_pairs"));
    assert!(event_log.contains("AnticompletePairIso"));

    // The engine path itself is pure/replayable and agrees with the runtime
    // result about the remaining graph.
    let mut replay_base = normalized_cleanup_fixture().2;
    let replay = apply_event(replay_base.clone(), ProtocolEvent::StartCycle)
        .expect("pure replay transition");
    replay_base = replay.state;
    assert_eq!(replay_base.live.present_nodes, state.live.present_nodes);
}

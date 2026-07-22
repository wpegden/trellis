//! Reference-papers feature end-to-end fixture (see `REFERENCE_PAPERS.md`).
//!
//! Covers, through the kernel's public APIs plus the runtime-CLI binary:
//!
//!   * golden byte-identity: an empty-registry `ProtocolState` serializes
//!     without any of the new keys and round-trips byte-identically (the
//!     legacy / MULTI_PAPER carry-forward shape);
//!   * the claim lifecycle at the model level: apply, full-set replace,
//!     reopen-by-fingerprint, prune-on-node-deletion (both normalizers),
//!     unknown-id prune, claim survival across an unrelated node edit;
//!   * rewind consistency: `commit_live` captures the committed +
//!     LastClean claim mirrors, `apply_last_clean_reset` rolls a claim
//!     back with the approved fingerprint, `restore_committed` rolls a
//!     rejected burst's claim back, and the (unmirrored) registry
//!     survives all of it;
//!   * the `add_reference_paper` runtime-CLI action against a real
//!     runtime root: quiescent-state precondition matrix (including
//!     compose-with-add-targets: a RevisionStating state accepts it),
//!     duplicate-id/spec-drift rejection, missing/empty file rejection,
//!     idempotent no-op re-add, the state ⊆ config assertion, and the
//!     TRELLIS_AB_TEMPLATES_DIR divergence warning;
//!   * `remove_reference_paper` is reject-only (names claimants while
//!     claimed; unsupported otherwise);
//!   * the substantiveness contract payload of a request built from the
//!     revived state carries the state-registry (bridge's sole source).
//!
//! Constraints: small synthetic fixture, no host `lake`, temp dirs rooted
//! under the build area via `common::project_tempdir` (never `/tmp`).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;
use trellis_kernel::{
    CorrStatus, GateKind, NodeId, NodeKind, PendingTask, Phase, ProtocolState, RefPaperId,
    ReferencePaperSpec, RequestKind, RuntimeMetadata, RuntimePaths, Stage, SupervisorRuntime,
    TargetId, WorkerResponse,
};

fn nid(s: &str) -> NodeId {
    NodeId::from(s)
}

fn tid(s: &str) -> TargetId {
    TargetId::from(s)
}

fn rid(s: &str) -> RefPaperId {
    RefPaperId::from(s)
}

fn ref_spec(path: &str, source: &str) -> ReferencePaperSpec {
    ReferencePaperSpec {
        tex_path: path.to_string(),
        source_id: source.to_string(),
    }
}

/// A clean two-target fixture with every lane Pass and the LastClean
/// mirrors captured (the `add_targets_mode.rs` fixture, trimmed).
fn clean_fixture() -> ProtocolState {
    let mut state = ProtocolState::default();
    state.max_theorem_invalid_attempt = 2;
    state.proof_invalid_review_threshold = 2;
    state.easy_max_retries = 2;

    let present: BTreeSet<NodeId> = ["Preamble", "MainTheorem", "Aux"]
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
    let proof_nodes: BTreeSet<NodeId> = ["MainTheorem", "Aux"].iter().map(|n| nid(n)).collect();
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
        state
            .sound_status
            .insert(n.clone(), trellis_kernel::SoundStatus::Pass);
        state
            .live
            .substantiveness_current_fingerprints
            .insert(n.clone(), subst.clone());
        state
            .substantiveness_approved_fingerprints
            .insert(n.clone(), subst);
        state
            .substantiveness_status
            .insert(n.clone(), CorrStatus::Pass);
    }
    state.normalize_all_structural_state();
    state.commit_live();
    state.ensure_node_metadata();
    state.validate().expect("fixture must be valid");
    assert!(state.global_blockers().is_empty(), "fixture must be clean");
    assert!(state.last_clean_mirrors_populated());
    state
}

// ===== Golden byte-identity ================================================

#[test]
fn empty_registry_state_serializes_without_new_keys_and_roundtrips() {
    let state = clean_fixture();
    let json = serde_json::to_string_pretty(&state).expect("serialize");
    for key in [
        "configured_reference_papers",
        "node_reference_grounds",
        "committed_node_reference_grounds",
        "last_clean_node_reference_grounds",
    ] {
        assert!(
            !json.contains(key),
            "empty-registry state must not emit `{key}` (legacy shape)"
        );
    }
    // MULTI_PAPER carry-forward: the legacy-shaped file loads and
    // re-saves byte-identically.
    let reloaded: ProtocolState = serde_json::from_str(&json).expect("legacy state loads");
    let resaved = serde_json::to_string_pretty(&reloaded).expect("re-serialize");
    assert_eq!(json, resaved, "legacy-shape state must round-trip byte-identically");
}

// ===== Claim lifecycle (model level) =======================================

fn seeded_registry_state() -> ProtocolState {
    let mut state = clean_fixture();
    state
        .configured_reference_papers
        .insert(rid("smith2020"), ref_spec("paper/refs/smith2020.tex", "Smith 2020"));
    state
}

#[test]
fn claim_apply_reopen_and_reapprove() {
    let mut state = seeded_registry_state();
    let node = nid("MainTheorem");

    // Worker claims the reference (full-replacement set).
    let response = WorkerResponse {
        snapshot: state.live.clone(),
        node_reference_grounds: BTreeMap::from([(
            node.clone(),
            BTreeSet::from([rid("smith2020")]),
        )]),
        ..WorkerResponse::default()
    };
    state.apply_worker_structure_updates(&response);
    assert_eq!(
        state.node_reference_grounds.get(&node),
        Some(&BTreeSet::from([rid("smith2020")]))
    );

    // The claim changes the observed substantiveness fingerprint (the
    // hydrator embeds claimed_reference_shas); current != approved
    // reopens the lane.
    state
        .live
        .substantiveness_current_fingerprints
        .insert(node.clone(), "subst-MainTheorem+ref".to_string());
    assert!(
        state.current_substantiveness_unknown(&node),
        "claimed node must reopen substantiveness"
    );

    // Verifier re-approves against the claim-bearing fingerprint.
    state
        .substantiveness_approved_fingerprints
        .insert(node.clone(), "subst-MainTheorem+ref".to_string());
    state
        .substantiveness_status
        .insert(node.clone(), CorrStatus::Pass);
    assert!(state.current_substantiveness_pass(&node));

    // An unrelated node edit leaves the claim in place.
    state
        .live
        .corr_current_fingerprints
        .insert(nid("Aux"), "corr-Aux-edited".to_string());
    state.normalize_all_structural_state();
    assert_eq!(
        state.node_reference_grounds.get(&node),
        Some(&BTreeSet::from([rid("smith2020")])),
        "claim must survive an unrelated node edit"
    );

    // Empty replacement set clears the claim.
    let clear = WorkerResponse {
        snapshot: state.live.clone(),
        node_reference_grounds: BTreeMap::from([(node.clone(), BTreeSet::new())]),
        ..WorkerResponse::default()
    };
    state.apply_worker_structure_updates(&clear);
    assert!(!state.node_reference_grounds.contains_key(&node));
}

#[test]
fn normalizers_prune_deleted_nodes_and_unknown_ids() {
    let mut state = seeded_registry_state();
    state
        .node_reference_grounds
        .insert(nid("MainTheorem"), BTreeSet::from([rid("smith2020")]));
    state
        .node_reference_grounds
        .insert(nid("Aux"), BTreeSet::from([rid("ghost99")]));
    // Unknown id pruned by the live normalizer (defensive; acceptance
    // rejects it upstream)…
    state.normalize_all_structural_state();
    assert!(!state.node_reference_grounds.contains_key(&nid("Aux")));
    // …and never reaches the committed mirror at commit time.
    state.commit_live();
    assert_eq!(
        state.committed_node_reference_grounds.get(&nid("MainTheorem")),
        Some(&BTreeSet::from([rid("smith2020")]))
    );
    assert!(!state.committed_node_reference_grounds.contains_key(&nid("Aux")));

    // Node deletion prunes the entry in both tiers.
    state.live.present_nodes.remove(&nid("MainTheorem"));
    state.committed.present_nodes.remove(&nid("MainTheorem"));
    state.normalize_all_structural_state();
    assert!(state.node_reference_grounds.is_empty());
    assert!(state.committed_node_reference_grounds.is_empty());
}

// ===== Rewind consistency ==================================================

#[test]
fn last_clean_rewind_rolls_claim_back_and_registry_survives() {
    // Claim present at the clean checkpoint: commit_live captures the
    // committed + LastClean mirrors.
    let mut state = seeded_registry_state();
    state
        .node_reference_grounds
        .insert(nid("MainTheorem"), BTreeSet::from([rid("smith2020")]));
    state
        .live
        .substantiveness_current_fingerprints
        .insert(nid("MainTheorem"), "subst+ref".to_string());
    state
        .substantiveness_approved_fingerprints
        .insert(nid("MainTheorem"), "subst+ref".to_string());
    state.commit_live();
    assert_eq!(
        state.last_clean_node_reference_grounds.get(&nid("MainTheorem")),
        Some(&BTreeSet::from([rid("smith2020")])),
        "clean checkpoint must capture the claim mirror"
    );

    // Later drift: the claim is dropped and the fingerprint moves.
    let drop_claim = WorkerResponse {
        snapshot: state.live.clone(),
        node_reference_grounds: BTreeMap::from([(nid("MainTheorem"), BTreeSet::new())]),
        ..WorkerResponse::default()
    };
    state.apply_worker_structure_updates(&drop_claim);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(nid("MainTheorem"), "subst-no-ref".to_string());
    assert!(!state.node_reference_grounds.contains_key(&nid("MainTheorem")));

    // LastClean rewind: claim restored consistently with the approved
    // fingerprint captured at the clean checkpoint; registry untouched.
    assert!(state.apply_last_clean_reset().expect("reset applies"));
    assert_eq!(
        state.node_reference_grounds.get(&nid("MainTheorem")),
        Some(&BTreeSet::from([rid("smith2020")])),
        "LastClean rewind must restore the claim"
    );
    assert_eq!(
        state.committed_node_reference_grounds.get(&nid("MainTheorem")),
        Some(&BTreeSet::from([rid("smith2020")]))
    );
    assert_eq!(
        state
            .substantiveness_approved_fingerprints
            .get(&nid("MainTheorem")),
        Some(&"subst+ref".to_string()),
        "restored claim must be consistent with the restored approved fingerprint"
    );
    assert!(state.current_substantiveness_pass(&nid("MainTheorem")));
    assert!(
        state.configured_reference_papers.contains_key(&rid("smith2020")),
        "the unmirrored registry survives the rewind"
    );
}

#[test]
fn restore_committed_rolls_back_a_rejected_bursts_claim() {
    let mut state = seeded_registry_state();
    state.commit_live();
    // Uncommitted (rejected-burst) claim mutation…
    let response = WorkerResponse {
        snapshot: state.live.clone(),
        node_reference_grounds: BTreeMap::from([(
            nid("MainTheorem"),
            BTreeSet::from([rid("smith2020")]),
        )]),
        ..WorkerResponse::default()
    };
    state.apply_worker_structure_updates(&response);
    assert!(state.node_reference_grounds.contains_key(&nid("MainTheorem")));
    // …is rolled back by restore_committed.
    state.restore_committed();
    assert!(!state.node_reference_grounds.contains_key(&nid("MainTheorem")));
    assert!(state.configured_reference_papers.contains_key(&rid("smith2020")));
}

// ===== Runtime-CLI action ==================================================

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, text).expect("write file");
}

fn run_cli_with_env(request: &Value, envs: &[(&str, &str)]) -> (Value, bool) {
    let exe = env!("CARGO_BIN_EXE_trellis_runtime_cli");
    let mut command = Command::new(exe);
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command
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

fn run_cli(request: &Value) -> (Value, bool) {
    run_cli_with_env(request, &[])
}

struct CliFixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    repo: std::path::PathBuf,
    config_path: std::path::PathBuf,
}

/// Real runtime root + repo. `state_prep` mutates the clean fixture
/// before it is persisted (phase changes, pending task, claims, …).
fn cli_fixture(reference_entries: Value, state_prep: impl FnOnce(&mut ProtocolState)) -> CliFixture {
    let tmp = common::project_tempdir();
    let root = tmp.path().join("runtime");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    write_file(&repo.join("paper/main.tex"), "\\begin{document}x\\end{document}\n");
    write_file(
        &repo.join("paper/refs/smith2020.tex"),
        "\\section*{Smith 2020}\nCited result text.\n",
    );
    write_file(
        &repo.join("paper/refs/doe2021.tex"),
        "\\section*{Doe 2021}\nSecond cited result.\n",
    );
    let config_path = repo.join("trellis.config.json");
    write_file(
        &config_path,
        &serde_json::to_string_pretty(&serde_json::json!({
            "repo_path": ".",
            "worker": {"provider": "codex", "model": "m"},
            "reviewer": {"provider": "codex", "model": "m"},
            "verification": {"provider": "codex", "model": "m"},
            "workflow": {
                "paper_tex_path": "paper/main.tex",
                "reference_papers": reference_entries,
            }
        }))
        .unwrap(),
    );
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("git")
                .status
                .success(),
            "git {args:?}"
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);
    git(&["tag", "supervisor2/clean-000001"]);

    let mut state = clean_fixture();
    state_prep(&mut state);
    let metadata = RuntimeMetadata {
        repo_path: Some(repo.clone()),
        config_path: Some(config_path.clone()),
        native_history_kinds: Default::default(),
        initial_planning_seeded: false,
        coverage_replanning_seeded: false,
    };
    SupervisorRuntime::initialize_with_metadata(RuntimePaths::new(root.clone()), state, metadata)
        .expect("initialize runtime");
    CliFixture {
        _tmp: tmp,
        root,
        repo,
        config_path,
    }
}

fn two_entry_config() -> Value {
    serde_json::json!([
        {"id": "smith2020", "tex_path": "paper/refs/smith2020.tex", "source_id": "Smith 2020"},
        {"id": "doe2021", "tex_path": "paper/refs/doe2021.tex", "source_id": "Doe 2021"}
    ])
}

#[test]
fn cli_add_reference_paper_end_to_end() {
    // Fixture state is RevisionStating: proves the action composes with
    // add-targets (quiescent, NOT Complete-only).
    let fx = cli_fixture(two_entry_config(), |state| {
        state.phase = Phase::RevisionStating;
        state.stage = Stage::Start;
    });
    let request = serde_json::json!({
        "action": "add_reference_paper",
        "root": fx.root,
        "config_path": fx.config_path,
    });

    let (response, ok) = run_cli(&request);
    assert!(ok, "action must succeed: {response:#}");
    assert_eq!(response["status"], "add_reference_paper_ok");
    assert_eq!(response["added"], serde_json::json!(["doe2021", "smith2020"]));
    assert_eq!(response["state"]["phase"], "RevisionStating");

    // Persisted state carries the registry (and nothing else moved: the
    // action mutates only configured_reference_papers).
    let persisted: ProtocolState = serde_json::from_str(
        &fs::read_to_string(fx.root.join("protocol_state.json")).expect("read persisted"),
    )
    .expect("parse persisted");
    assert_eq!(
        persisted.configured_reference_papers.get(&rid("smith2020")),
        Some(&ref_spec("paper/refs/smith2020.tex", "Smith 2020"))
    );
    assert_eq!(persisted.configured_reference_papers.len(), 2);
    assert!(persisted.node_reference_grounds.is_empty());

    // A request built from the revived state carries the registry to
    // the substantiveness lane (state-carried, the bridge's sole path
    // source).
    let mut paper_request = persisted.expected_request(7, RequestKind::Paper);
    paper_request.substantiveness_verify_nodes = BTreeSet::from([nid("MainTheorem")]);
    paper_request.node_reference_grounds =
        BTreeMap::from([(nid("MainTheorem"), BTreeSet::from([rid("smith2020")]))]);
    trellis_kernel::request_contracts::populate_request_prompt_contracts(&mut paper_request, None);
    assert_eq!(
        paper_request.paper_contract["reference_papers"]["smith2020"]["tex_path"],
        serde_json::json!("paper/refs/smith2020.tex"),
        "substantiveness contract must carry the state registry"
    );

    // Idempotent no-op on identical re-add.
    let (response, ok) = run_cli(&request);
    assert!(ok, "re-add must be a no-op: {response:#}");
    assert_eq!(response["added"], serde_json::json!([]));
    assert!(response["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap_or_default().contains("idempotent no-op")));

    // Duplicate id (config spec drift vs state): rejected.
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(&fx.config_path).unwrap()).unwrap();
    config["workflow"]["reference_papers"][0]["source_id"] = serde_json::json!("Smith 2021 v2");
    write_file(&fx.config_path, &serde_json::to_string_pretty(&config).unwrap());
    let (response, ok) = run_cli(&request);
    assert!(!ok, "spec drift must be rejected: {response:#}");
    assert!(response["message"]
        .as_str()
        .unwrap_or_default()
        .contains("duplicate id `smith2020`"));

    // State ⊆ config assertion: dropping a state-held entry from config
    // is refused.
    config["workflow"]["reference_papers"] = serde_json::json!([
        {"id": "doe2021", "tex_path": "paper/refs/doe2021.tex", "source_id": "Doe 2021"}
    ]);
    write_file(&fx.config_path, &serde_json::to_string_pretty(&config).unwrap());
    let (response, ok) = run_cli(&request);
    assert!(!ok);
    assert!(response["message"]
        .as_str()
        .unwrap_or_default()
        .contains("must be a subset of the config registry"));

    // Missing file: a new config entry whose tex file does not exist.
    config["workflow"]["reference_papers"] = serde_json::json!([
        {"id": "smith2020", "tex_path": "paper/refs/smith2020.tex", "source_id": "Smith 2020"},
        {"id": "doe2021", "tex_path": "paper/refs/doe2021.tex", "source_id": "Doe 2021"},
        {"id": "ghost", "tex_path": "paper/refs/ghost.tex", "source_id": "Ghost"}
    ]);
    write_file(&fx.config_path, &serde_json::to_string_pretty(&config).unwrap());
    let (response, ok) = run_cli(&request);
    assert!(!ok);
    assert!(response["message"]
        .as_str()
        .unwrap_or_default()
        .contains("missing/unreadable"));

    // Empty file: rejected too.
    write_file(&fx.repo.join("paper/refs/ghost.tex"), "  \n");
    let (response, ok) = run_cli(&request);
    assert!(!ok);
    assert!(response["message"].as_str().unwrap_or_default().contains("is empty"));

    // A/B templates warning rides the notes when the env var is set.
    write_file(&fx.repo.join("paper/refs/ghost.tex"), "real content\n");
    let (response, ok) = run_cli_with_env(&request, &[("TRELLIS_AB_TEMPLATES_DIR", "/nonexistent/ab")]);
    assert!(ok, "add with env must succeed: {response:#}");
    assert_eq!(response["added"], serde_json::json!(["ghost"]));
    assert!(response["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap_or_default().contains("TRELLIS_AB_TEMPLATES_DIR")));
}

#[test]
fn cli_add_reference_paper_precondition_matrix() {
    struct Case {
        name: &'static str,
        prep: fn(&mut ProtocolState),
        expect: &'static str,
    }
    let cases = [
        Case {
            name: "in_flight_request",
            prep: |state| {
                let request = state.issue_request(RequestKind::Review);
                state.in_flight_request = Some(request);
            },
            expect: "no in-flight request",
        },
        Case {
            name: "pending_task",
            prep: |state| {
                state.pending_task = Some(PendingTask::default());
            },
            expect: "no pending worker task",
        },
        Case {
            name: "active_human_gate",
            prep: |state| {
                state.gate_kind = GateKind::Advance;
            },
            expect: "no active human gate",
        },
    ];
    for case in cases {
        let fx = cli_fixture(two_entry_config(), case.prep);
        let (response, ok) = run_cli(&serde_json::json!({
            "action": "add_reference_paper",
            "root": fx.root,
            "config_path": fx.config_path,
        }));
        assert!(!ok, "{}: must be rejected; got {response:#}", case.name);
        assert!(
            response["message"]
                .as_str()
                .unwrap_or_default()
                .contains(case.expect),
            "{}: expected `{}` in {response:#}",
            case.name,
            case.expect
        );
    }
    // Quiescent Complete state accepts too (any quiescent phase does).
    let fx = cli_fixture(two_entry_config(), |state| {
        state.phase = Phase::Complete;
        state.stage = Stage::Complete;
    });
    let (response, ok) = run_cli(&serde_json::json!({
        "action": "add_reference_paper",
        "root": fx.root,
        "config_path": fx.config_path,
    }));
    assert!(ok, "quiescent Complete state must accept: {response:#}");
}

#[test]
fn cli_remove_reference_paper_is_reject_only() {
    // Claimed id: rejected naming the claimants.
    let fx = cli_fixture(two_entry_config(), |state| {
        state
            .configured_reference_papers
            .insert(rid("smith2020"), ref_spec("paper/refs/smith2020.tex", "Smith 2020"));
        state
            .node_reference_grounds
            .insert(nid("MainTheorem"), BTreeSet::from([rid("smith2020")]));
        state.normalize_all_structural_state();
    });
    let (response, ok) = run_cli(&serde_json::json!({
        "action": "remove_reference_paper",
        "root": fx.root,
        "id": "smith2020",
    }));
    assert!(!ok);
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("claimed by node(s) MainTheorem"),
        "got: {message}"
    );

    // Unclaimed id: still rejected — no removal path in v1.
    let (response, ok) = run_cli(&serde_json::json!({
        "action": "remove_reference_paper",
        "root": fx.root,
        "id": "doe2021",
    }));
    assert!(!ok);
    assert!(response["message"]
        .as_str()
        .unwrap_or_default()
        .contains("no supported path in v1"));
}

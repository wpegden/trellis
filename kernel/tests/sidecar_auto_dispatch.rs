//! Kernel-queue refill — the kernel's standing ranked list of every
//! eligible node, which the grunt pool draws on whenever the reviewer
//! lane has nothing assignable.
//!
//! Three tiers:
//!   * the pure decision (`plan_kernel_queue_refill` + the candidate
//!     order): headroom, short-supply, re-entrancy, and the full
//!     priority order (fewest attempts → sketches last → shortest NL
//!     proof → node id);
//!   * the disk-reading half (`read_attempt_counts` / `nl_proof_length`):
//!     every "do not know" must read as "do not fire";
//!   * the engine apply (`SidecarQueueAutoAdd`): the entries it mints
//!     land in the KERNEL lane and flow through the identical prune
//!     path as reviewer-added ones.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use trellis_kernel::engine::{apply_event, ProtocolEvent};
use trellis_kernel::sidecar::{
    collect_auto_dispatch_candidates, nl_proof_length, order_auto_dispatch_candidates,
    plan_kernel_queue_refill, read_attempt_counts, read_attempt_history, sidecar_attempted_path,
    AutoDispatchCandidate,
};
use trellis_kernel::{
    AssessmentOrigin, CorrStatus, NodeId, NodeKind, Phase, ProtocolState, SidecarQueueAutoAddPayload,
    SidecarQueueOrigin, SoundAssessment, SoundAssessmentStatus, SoundFingerprintParts, Stage,
    WorkingSnapshot,
};

fn tempdir() -> tempfile::TempDir {
    let tmp_root = std::env::current_dir()
        .expect("current dir")
        .join(".tmp-tests");
    std::fs::create_dir_all(&tmp_root).expect("tmp root");
    tempfile::tempdir_in(&tmp_root).expect("tempdir")
}

fn nid(s: &str) -> NodeId {
    NodeId::from(s)
}

fn candidate(node: &str, attempts: u32, sketch: bool, proof: Option<usize>) -> AutoDispatchCandidate {
    AutoDispatchCandidate {
        node: nid(node),
        attempts,
        sketch,
        proof_length: proof,
    }
}

fn names(nodes: &[NodeId]) -> Vec<&str> {
    nodes.iter().map(|node| node.as_str()).collect()
}

/// `n` attempt-history rows with no recorded content hash (rows written
/// before the daemon stamped `node_file_sha256`) — these always count
/// toward the attempt cap.
fn legacy_rows(n: usize) -> Vec<String> {
    vec![String::new(); n]
}

/// The cap is a safety bound, not a scheduling parameter, so almost
/// every test wants "no bound in the way".
const UNCAPPED: usize = 512;

// ====================================================================
// Tier 1 — the pure decision
// ====================================================================

#[test]
fn refills_the_whole_ranked_list_not_the_pool_width() {
    // The bug this feature exists to fix: the old rule added `idle - 1`
    // nodes, so a 4-slot pool got 3 and idled again one attempt later.
    // The lane takes EVERY candidate it has headroom for.
    let pool: Vec<AutoDispatchCandidate> = (0..40)
        .map(|i| candidate(&format!("N{i:02}"), 0, false, Some(i)))
        .collect();
    assert_eq!(plan_kernel_queue_refill(0, UNCAPPED, pool).len(), 40);
}

#[test]
fn headroom_is_the_cap_minus_what_the_lane_already_holds() {
    let pool: Vec<AutoDispatchCandidate> = (0..10)
        .map(|i| candidate(&format!("N{i:02}"), 0, false, Some(i)))
        .collect();
    assert_eq!(names(&plan_kernel_queue_refill(7, 10, pool.clone())), vec!["N00", "N01", "N02"]);
    // Lane at (or over) the cap: nothing more, ever.
    assert!(plan_kernel_queue_refill(10, 10, pool.clone()).is_empty());
    assert!(plan_kernel_queue_refill(99, 10, pool).is_empty());
}

#[test]
fn re_entering_the_same_boundary_adds_nothing_the_first_pass_did_not() {
    // The human-gate poll loop re-enters the boundary hook. The second
    // pass sees the first pass's adds BOTH as `resident` and as queue
    // members (so they are filtered out of `candidates` upstream) —
    // the headroom subtraction is the churn guard the old queue-empty
    // trigger used to be.
    let pool = vec![
        candidate("Alpha", 0, false, Some(10)),
        candidate("Beta", 0, false, Some(20)),
    ];
    let first = plan_kernel_queue_refill(0, UNCAPPED, pool);
    assert_eq!(first.len(), 2);
    assert!(plan_kernel_queue_refill(first.len(), UNCAPPED, Vec::new()).is_empty());
}

#[test]
fn stops_short_when_eligible_nodes_run_out() {
    // A wide lane wants everything; only two nodes are eligible (the
    // rest of the tablet is Lean-closed). Take what exists, no padding.
    let pool = vec![
        candidate("Alpha", 0, false, Some(10)),
        candidate("Beta", 0, false, Some(20)),
    ];
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, pool)),
        vec!["Alpha", "Beta"]
    );
    // Nothing eligible at all: nothing to say.
    assert!(plan_kernel_queue_refill(0, UNCAPPED, Vec::new()).is_empty());
}

#[test]
fn order_is_fewest_attempts_then_shortest_proof() {
    // Attempts dominate length: the 3-char proof with two attempts
    // sorts BELOW the 900-char proof with none. This is also the
    // anti-starvation property — a node that fails is re-minted a cycle
    // later carrying attempts+1 and sinks below everything tried less
    // often.
    let pool = vec![
        candidate("Short2Attempts", 2, false, Some(3)),
        candidate("Long0Attempts", 0, false, Some(900)),
        candidate("Mid1Attempt", 1, false, Some(50)),
        candidate("Short0Attempts", 0, false, Some(30)),
    ];
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, pool)),
        vec![
            "Short0Attempts",
            "Long0Attempts",
            "Mid1Attempt",
            "Short2Attempts"
        ]
    );
}

#[test]
fn sketch_counts_as_longer_than_every_non_sketch_proof() {
    // The sketch has the SHORTEST body and would win on length alone;
    // it must still sort after every non-sketch candidate of the same
    // attempt count — and ahead of a non-sketch with MORE attempts,
    // because attempts remain the primary key.
    let pool = vec![
        candidate("SketchTiny", 0, true, Some(1)),
        candidate("PlainHuge", 0, false, Some(100_000)),
        candidate("PlainMid", 0, false, Some(500)),
        candidate("PlainAttempted", 1, false, Some(2)),
    ];
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, pool)),
        vec!["PlainMid", "PlainHuge", "SketchTiny", "PlainAttempted"]
    );
}

#[test]
fn unmeasurable_proof_sorts_after_every_measured_one() {
    // No `\begin{proof}` block is not evidence of a short proof.
    let pool = vec![
        candidate("NoBlock", 0, false, None),
        candidate("Huge", 0, false, Some(999_999)),
    ];
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, pool)),
        vec!["Huge", "NoBlock"]
    );
}

#[test]
fn tie_break_is_node_id_and_independent_of_input_order() {
    let forward = vec![
        candidate("Alpha", 1, false, Some(40)),
        candidate("Beta", 1, false, Some(40)),
        candidate("Gamma", 1, false, Some(40)),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();
    let expected = vec!["Alpha", "Beta", "Gamma"];
    assert_eq!(names(&plan_kernel_queue_refill(0, UNCAPPED, forward)), expected);
    assert_eq!(names(&plan_kernel_queue_refill(0, UNCAPPED, reversed)), expected);
}

#[test]
fn ordering_is_a_total_order_on_every_key() {
    // One candidate per key position, shuffled in: the full ranking
    // pins that no key silently dominates the one above it.
    let pool = vec![
        candidate("ZeroSketch", 0, true, Some(1)),
        candidate("OneShortB", 1, false, Some(5)),
        candidate("OneShortA", 1, false, Some(5)),
        candidate("ZeroNoBlock", 0, false, None),
        candidate("ZeroLong", 0, false, Some(80)),
        candidate("ZeroShort", 0, false, Some(7)),
        candidate("TwoTiny", 2, false, Some(1)),
    ];
    let ranked = order_auto_dispatch_candidates(pool);
    let ordered: Vec<&str> = ranked.iter().map(|c| c.node.as_str()).collect();
    assert_eq!(
        ordered,
        vec![
            "ZeroShort",   // 0 attempts, shortest measured
            "ZeroLong",    // 0 attempts, longer
            "ZeroNoBlock", // 0 attempts, unmeasurable → after measured
            "ZeroSketch",  // 0 attempts, sketch → after every non-sketch
            "OneShortA",   // 1 attempt, node-id tie-break
            "OneShortB",
            "TwoTiny", // 2 attempts, last despite the tiniest proof
        ]
    );
}

// ====================================================================
// Tier 2 — reading disk (never act on a reading you do not trust)
// ====================================================================

#[test]
fn attempt_counts_count_every_generation_and_fail_closed_on_garbage() {
    let dir = tempdir();
    let runtime_root = dir.path().join("runtime");
    std::fs::create_dir_all(runtime_root.join("sidecar")).unwrap();
    // Absent file = "no attempts yet" (fresh run, or the daemon's
    // post-rewind wipe), which is a legitimate zero.
    assert!(read_attempt_counts(&runtime_root).expect("absent is ok").is_empty());
    std::fs::write(
        sidecar_attempted_path(&runtime_root),
        r#"{"Alpha":[{"entry_seq":1,"attempt_id":"a1","status":"failed","node_file_sha256":"abc123"},{"entry_seq":9,"attempt_id":"a2","status":"budget_exhausted"}],
            "Beta":[{"entry_seq":4,"attempt_id":"b1","status":"failed"}]}"#,
    )
    .unwrap();
    let counts = read_attempt_counts(&runtime_root).expect("parses");
    assert_eq!(counts.get(&nid("Alpha")), Some(&2));
    assert_eq!(counts.get(&nid("Beta")), Some(&1));
    assert_eq!(counts.get(&nid("Gamma")), None);
    // The per-attempt view keeps the stamped content hash of each row;
    // rows that predate the stamping read as "" (they always count
    // toward the attempt cap).
    let history = read_attempt_history(&runtime_root).expect("parses");
    assert_eq!(
        history.get(&nid("Alpha")),
        Some(&vec!["abc123".to_string(), String::new()])
    );
    assert_eq!(history.get(&nid("Beta")), Some(&vec![String::new()]));
    // A kernel `not_queued` disposition means authority/infrastructure
    // discarded the proof before it was judged. Keep the raw attempted row
    // for daemon generation dedupe, but exempt it from BOTH ranking and the
    // per-content attempt ceiling.
    let mut attempted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(sidecar_attempted_path(&runtime_root)).unwrap(),
    )
    .unwrap();
    attempted["Alpha"][0]["chargeable"] = serde_json::json!(false);
    std::fs::write(
        sidecar_attempted_path(&runtime_root),
        serde_json::to_vec_pretty(&attempted).unwrap(),
    )
    .unwrap();
    let counts = read_attempt_counts(&runtime_root).expect("parses with feedback");
    assert_eq!(counts.get(&nid("Alpha")), Some(&1));
    let history = read_attempt_history(&runtime_root).expect("parses with feedback");
    assert_eq!(history.get(&nid("Alpha")), Some(&vec![String::new()]));
    let repo = dir.path().join("repo");
    write_tex(&repo, "Alpha", "short");
    let supply = collect_auto_dispatch_candidates(
        &repo,
        &boundary_state(&["Alpha"]),
        &history,
        2,
        0,
    );
    assert!(supply.capped.is_empty());
    assert_eq!(supply.candidates.len(), 1);
    assert_eq!(supply.candidates[0].node, nid("Alpha"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(sidecar_attempted_path(&runtime_root)).unwrap()
        )
        .unwrap()["Alpha"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "accounting exemption must not erase daemon dedupe/forensic history"
    );
    // Present but corrupt: the PRIMARY sort key is unavailable, so the
    // caller must skip rather than rank everything as zero-attempts.
    std::fs::write(sidecar_attempted_path(&runtime_root), "{ truncated").unwrap();
    assert!(read_attempt_counts(&runtime_root).is_err());
    assert!(read_attempt_history(&runtime_root).is_err());
}

fn write_tex(repo: &Path, node: &str, body: &str) {
    let tablet = repo.join("Tablet");
    std::fs::create_dir_all(&tablet).unwrap();
    std::fs::write(
        tablet.join(format!("{node}.tex")),
        format!("\\begin{{theorem}}[{node}]\nStatement.\n\\end{{theorem}}\n\\begin{{proof}}\n{body}\n\\end{{proof}}\n"),
    )
    .unwrap();
}

#[test]
fn proof_length_is_the_proof_block_and_survives_a_reflow() {
    let dir = tempdir();
    let repo = dir.path().join("repo");
    write_tex(&repo, "Wrapped", "Let $x$ be\nsmall.");
    write_tex(&repo, "OneLine", "Let $x$ be small.");
    let wrapped = nl_proof_length(&repo, &nid("Wrapped"));
    let one_line = nl_proof_length(&repo, &nid("OneLine"));
    assert_eq!(
        wrapped, one_line,
        "reflowing an untouched proof must not move it in the queue order"
    );
    assert_eq!(wrapped, Some("Let$x$besmall.".chars().count()));
    // The statement text outside the proof block is not counted.
    write_tex(&repo, "LongStatement", "QED.");
    assert!(nl_proof_length(&repo, &nid("LongStatement")) < wrapped);
    // Absent file / no proof block: unmeasurable, never zero.
    assert_eq!(nl_proof_length(&repo, &nid("Ghost")), None);
    std::fs::write(repo.join("Tablet").join("NoProof.tex"), "\\begin{theorem}\nX\n\\end{theorem}\n").unwrap();
    assert_eq!(nl_proof_length(&repo, &nid("NoProof")), None);
}

// ====================================================================
// Tier 3 — candidate collection + the engine apply
// ====================================================================

fn add_eligible_node(state: &mut ProtocolState, name: &str) {
    let n = nid(name);
    state.live.present_nodes.insert(n.clone());
    state.live.open_nodes.insert(n.clone());
    state.node_kinds.insert(n.clone(), NodeKind::Proof);
    state.proof_nodes.insert(n.clone());
    state.corr_status.insert(n.clone(), CorrStatus::Pass);
    state
        .live
        .corr_current_fingerprints
        .insert(n.clone(), format!("c-{name}"));
    state
        .corr_approved_fingerprints
        .insert(n.clone(), format!("c-{name}"));
    state
        .substantiveness_status
        .insert(n.clone(), trellis_kernel::SubstantivenessStatus::Pass);
    state
        .live
        .substantiveness_current_fingerprints
        .insert(n.clone(), format!("s-{name}"));
    state
        .substantiveness_approved_fingerprints
        .insert(n.clone(), format!("s-{name}"));
    state.deps.insert(n.clone(), BTreeSet::new());
    state.sound_assessments.insert(
        n.clone(),
        SoundAssessment {
            status: SoundAssessmentStatus::VerifierPass,
            origin: AssessmentOrigin::VerifierPanel,
            fingerprints: SoundFingerprintParts::default(),
            lane_votes: BTreeMap::new(),
            reviewer_action_id: None,
        },
    );
}

/// A quiescent ProofFormalization boundary (stage `Start`, nothing in
/// flight) with three open, sidecar-eligible proof nodes.
fn boundary_state(nodes: &[&str]) -> ProtocolState {
    let mut state = ProtocolState {
        phase: Phase::ProofFormalization,
        stage: Stage::Start,
        cycle: 12,
        active_node: None,
        live: WorkingSnapshot::default(),
        ..ProtocolState::default()
    };
    for name in nodes {
        add_eligible_node(&mut state, name);
    }
    state.committed = state.live.clone();
    state.committed_proof_nodes = state.proof_nodes.clone();
    state.committed_deps = state.deps.clone();
    state
}

fn auto_add(state: ProtocolState, nodes: &[&str]) -> Result<ProtocolState, String> {
    let payload = SidecarQueueAutoAddPayload {
        nodes: nodes.iter().map(|n| nid(n)).collect(),
    };
    apply_event(state, ProtocolEvent::SidecarQueueAutoAdd { payload })
        .map(|outcome| outcome.state)
        .map_err(|err| format!("{err:?}"))
}

#[test]
fn candidate_collection_reuses_the_single_eligibility_predicate() {
    let dir = tempdir();
    let repo = dir.path().join("repo");
    for (node, body) in [("Alpha", "aaa"), ("Beta", "bb"), ("Gamma", "g")] {
        write_tex(&repo, node, body);
    }
    let mut state = boundary_state(&["Alpha", "Beta", "Gamma", "Delta"]);
    // Delta is the routed active node — ineligible, and therefore not a
    // candidate: the auto-dispatcher owns no second definition of
    // "eligible".
    state.active_node = Some(nid("Delta"));
    // Gamma is already queued — the reviewer got there first.
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Gamma"),
        entry_seq: 4,
        queued_at_cycle: 11,
        origin: Default::default(),
    });
    state.sidecar_queue_seq = 4;
    let history = BTreeMap::from([(nid("Alpha"), legacy_rows(3))]);
    let candidates = collect_auto_dispatch_candidates(&repo, &state, &history, 0, 0).candidates;
    let got: Vec<(&str, u32, Option<usize>)> = candidates
        .iter()
        .map(|c| (c.node.as_str(), c.attempts, c.proof_length))
        .collect();
    assert_eq!(got, vec![("Alpha", 3, Some(3)), ("Beta", 0, Some(2))]);
    // And the ranking puts the never-attempted node first.
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, candidates)),
        vec!["Beta", "Alpha"]
    );
}

#[test]
fn a_generation_spent_this_cycle_is_not_re_queued_until_the_next_one() {
    let dir = tempdir();
    let repo = dir.path().join("repo");
    for node in ["Alpha", "Beta"] {
        write_tex(&repo, node, "short");
    }
    let mut state = boundary_state(&["Alpha", "Beta"]);
    // The boundary that just retired Alpha's generation (the outcome
    // ingest runs immediately before auto-dispatch, in this same
    // boundary) must not hand Alpha straight back to the pool.
    state
        .sidecar_queue_prune_log
        .push(trellis_kernel::SidecarQueuePrune {
            node: nid("Alpha"),
            entry_seq: 3,
            cycle: state.cycle,
            reason: "attempt_spent:failed".to_string(),
            origin: Default::default(),
        });
    let history = BTreeMap::new();
    let candidates = collect_auto_dispatch_candidates(&repo, &state, &history, 0, 0).candidates;
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, candidates)),
        vec!["Beta"],
        "the just-spent node waits a cycle; everything else still flows"
    );
    // Next cycle, Alpha is a candidate again.
    state.cycle += 1;
    let candidates = collect_auto_dispatch_candidates(&repo, &state, &history, 0, 0).candidates;
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, candidates)),
        vec!["Alpha", "Beta"]
    );
    // A prune for any OTHER reason is not a cool-off (those nodes are
    // ineligible anyway, and a `lane_drift` entry must not outlive the
    // drift).
    let mut drifted = boundary_state(&["Alpha", "Beta"]);
    drifted
        .sidecar_queue_prune_log
        .push(trellis_kernel::SidecarQueuePrune {
            node: nid("Alpha"),
            entry_seq: 3,
            cycle: drifted.cycle,
            reason: "lane_drift".to_string(),
            origin: Default::default(),
        });
    let candidates = collect_auto_dispatch_candidates(&repo, &drifted, &history, 0, 0).candidates;
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, candidates)),
        vec!["Alpha", "Beta"]
    );
}

#[test]
fn auto_added_entries_are_reviewer_entries_bar_the_lane_tag() {
    let mut state = boundary_state(&["Alpha", "Beta"]);
    // A reviewer-added entry already holds generation 4.
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Alpha"),
        entry_seq: 4,
        queued_at_cycle: 11,
        origin: Default::default(),
    });
    state.sidecar_queue_seq = 4;
    let state = auto_add(state, &["Beta"]).expect("auto-add applies");
    assert_eq!(state.sidecar_queue.len(), 2);
    let auto = &state.sidecar_queue[1];
    assert_eq!(auto.node.as_str(), "Beta");
    assert_eq!(
        auto.entry_seq, 5,
        "the auto-added entry mints the NEXT generation from the same monotone counter"
    );
    assert_eq!(auto.queued_at_cycle, 12);
    assert_eq!(state.sidecar_queue_seq, 5);
    // Identical to a reviewer entry of the same generation in every
    // field EXCEPT the lane tag — which is the whole difference, and
    // the only thing any consumer may branch on.
    assert_eq!(
        *auto,
        trellis_kernel::SidecarQueueEntry {
            node: nid("Beta"),
            entry_seq: 5,
            queued_at_cycle: 12,
            origin: SidecarQueueOrigin::Kernel,
        }
    );
    assert_eq!(state.sidecar_queue[0].origin, SidecarQueueOrigin::Reviewer);
    assert_eq!(state.sidecar_kernel_queue_len(), 1);
}

#[test]
fn auto_added_entries_prune_on_lane_drift_like_any_other_entry() {
    let state = boundary_state(&["Alpha", "Beta"]);
    let mut state = auto_add(state, &["Alpha", "Beta"]).expect("auto-add applies");
    assert_eq!(state.sidecar_queue.len(), 2);
    // Substantiveness stops passing for Alpha (a statement edit moves
    // the current fingerprint off the approved one).
    state
        .live
        .substantiveness_current_fingerprints
        .insert(nid("Alpha"), "s-drifted".to_string());
    // The deterministic tail prune runs on the NEXT event, whatever it
    // is. Re-adding an unrelated node is the cheapest boundary event.
    state.live.open_nodes.insert(nid("Gamma"));
    state.live.present_nodes.insert(nid("Gamma"));
    add_eligible_node(&mut state, "Gamma");
    let state = auto_add(state, &["Gamma"]).expect("auto-add applies");
    let queued: Vec<&str> = state
        .sidecar_queue
        .iter()
        .map(|entry| entry.node.as_str())
        .collect();
    assert_eq!(queued, vec!["Beta", "Gamma"], "the drifted entry left");
    let prune = state
        .sidecar_queue_prune_log
        .iter()
        .find(|row| row.node.as_str() == "Alpha")
        .expect("prune log names the auto-added entry");
    assert_eq!(prune.reason, "lane_drift");
    assert_eq!(prune.entry_seq, 1);
}

#[test]
fn auto_add_skips_a_node_that_stopped_qualifying_instead_of_failing_the_batch() {
    let mut state = boundary_state(&["Alpha", "Beta", "Gamma"]);
    // Gamma got routed active between the runtime's ranking and the
    // apply; Beta is somehow already queued.
    state.active_node = Some(nid("Gamma"));
    state.sidecar_queue.push(trellis_kernel::SidecarQueueEntry {
        node: nid("Beta"),
        entry_seq: 2,
        queued_at_cycle: 11,
        origin: Default::default(),
    });
    state.sidecar_queue_seq = 2;
    let state = auto_add(state, &["Alpha", "Beta", "Gamma"]).expect("the batch still applies");
    let queued: Vec<&str> = state
        .sidecar_queue
        .iter()
        .map(|entry| entry.node.as_str())
        .collect();
    assert_eq!(queued, vec!["Beta", "Alpha"]);
    assert_eq!(state.sidecar_queue_seq, 3, "only one generation was minted");
}

#[test]
fn auto_add_is_boundary_only_and_fails_loud_on_a_malformed_batch() {
    // Not at a boundary.
    let mut mid_cycle = boundary_state(&["Alpha"]);
    mid_cycle.stage = Stage::Reviewer;
    assert!(auto_add(mid_cycle, &["Alpha"]).is_err());
    // Empty batch / empty node / repeated node: a broken producer, not
    // a race — fail loud.
    assert!(auto_add(boundary_state(&["Alpha"]), &[]).is_err());
    assert!(auto_add(boundary_state(&["Alpha"]), &[""]).is_err());
    assert!(auto_add(boundary_state(&["Alpha"]), &["Alpha", "Alpha"]).is_err());
}

#[test]
fn auto_add_emits_no_commands_and_no_checkpoint() {
    let payload = SidecarQueueAutoAddPayload {
        nodes: vec![nid("Alpha")],
    };
    let outcome = apply_event(
        boundary_state(&["Alpha"]),
        ProtocolEvent::SidecarQueueAutoAdd { payload },
    )
    .expect("applies");
    assert!(
        outcome.commands.is_empty(),
        "nothing on disk changed; the queue rides the next ordinary checkpoint"
    );
}

#[test]
fn the_attempt_ceiling_retires_a_node_the_pool_keeps_failing() {
    let dir = tempdir();
    let repo = dir.path().join("repo");
    for node in ["Alpha", "Beta"] {
        write_tex(&repo, node, "short");
    }
    let state = boundary_state(&["Alpha", "Beta"]);
    // Hash-less rows (pre-stamping history): they always count toward
    // the cap, whatever the node file holds now.
    let history = BTreeMap::from([(nid("Alpha"), legacy_rows(3)), (nid("Beta"), legacy_rows(2))]);
    // Ceiling 0 = unlimited: rank alone decides, and Beta (fewer
    // attempts) simply sorts first.
    let supply = collect_auto_dispatch_candidates(&repo, &state, &history, 0, 0);
    assert!(supply.capped.is_empty());
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, supply.candidates)),
        vec!["Beta", "Alpha"]
    );
    // Ceiling 3: Alpha has spent its allowance and leaves the lane;
    // Beta, at 2, gets one more. The withheld node is named for the
    // supply-gate log line.
    let supply = collect_auto_dispatch_candidates(&repo, &state, &history, 3, 0);
    assert_eq!(names(&supply.capped), vec!["Alpha"]);
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, supply.candidates)),
        vec!["Beta"]
    );
    // Ceiling 2 retires both.
    let supply = collect_auto_dispatch_candidates(&repo, &state, &history, 2, 0);
    assert_eq!(names(&supply.capped), vec!["Alpha", "Beta"]);
    assert!(plan_kernel_queue_refill(0, UNCAPPED, supply.candidates).is_empty());
}

#[test]
fn a_content_change_resets_the_attempt_cap() {
    let dir = tempdir();
    let repo = dir.path().join("repo");
    write_tex(&repo, "Alpha", "short");
    let tablet = repo.join("Tablet");
    std::fs::write(
        tablet.join("Alpha.lean"),
        "theorem alpha : True := by\n-- BODY\n  sorry\n",
    )
    .unwrap();
    let old_sha = "0".repeat(64); // some content the file no longer holds
    let state = boundary_state(&["Alpha"]);
    // Five attempts, all recorded against the OLD content: the node was
    // repaired since, so it is a new problem and flows again.
    let history = BTreeMap::from([(nid("Alpha"), vec![old_sha.clone(); 5])]);
    let supply = collect_auto_dispatch_candidates(&repo, &state, &history, 5, 0);
    assert!(supply.capped.is_empty());
    assert_eq!(
        names(&plan_kernel_queue_refill(0, UNCAPPED, supply.candidates)),
        vec!["Alpha"],
        "attempts against superseded content do not count toward the cap"
    );
    // Five attempts against the CURRENT content: capped.
    let (_, current_sha) =
        trellis_kernel::filespec_split::read_node_file(&repo, "Alpha").expect("node file reads");
    let history = BTreeMap::from([(nid("Alpha"), vec![current_sha.clone(); 5])]);
    let supply = collect_auto_dispatch_candidates(&repo, &state, &history, 5, 0);
    assert_eq!(names(&supply.capped), vec!["Alpha"]);
    assert!(supply.candidates.is_empty());
    // Mixed history: four spent on the old content, four on the current
    // — only the current-content spends count, so one attempt remains.
    let mut rows = vec![old_sha; 4];
    rows.extend(vec![current_sha; 4]);
    let history = BTreeMap::from([(nid("Alpha"), rows)]);
    let supply = collect_auto_dispatch_candidates(&repo, &state, &history, 5, 0);
    assert!(supply.capped.is_empty());
    assert_eq!(supply.candidates.len(), 1);
}

/// The lane is a STANDING list: a boundary mints only for nodes it does
/// not already hold. That is what keeps the daemon's
/// one-attempt-per-(node, entry_seq) dedupe meaningful — and what stops
/// ~108 generations being minted every single boundary.
#[test]
fn a_second_boundary_mints_only_for_what_the_lane_does_not_hold() {
    let dir = tempdir();
    let repo = dir.path().join("repo");
    for node in ["Alpha", "Beta", "Gamma"] {
        write_tex(&repo, node, "short");
    }
    let state = boundary_state(&["Alpha", "Beta", "Gamma"]);
    let history = BTreeMap::new();
    let first = plan_kernel_queue_refill(
        0,
        UNCAPPED,
        collect_auto_dispatch_candidates(&repo, &state, &history, 0, 0).candidates,
    );
    assert_eq!(first.len(), 3);
    let state = auto_add(state, &["Alpha", "Beta", "Gamma"]).expect("refill applies");
    assert_eq!(state.sidecar_queue_seq, 3);
    assert_eq!(state.sidecar_kernel_queue_len(), 3);
    // Next boundary: every eligible node is resident, so nothing is a
    // candidate and no generation is minted.
    let resident = state.sidecar_kernel_queue_len();
    let again = plan_kernel_queue_refill(
        resident,
        UNCAPPED,
        collect_auto_dispatch_candidates(&repo, &state, &history, 0, 0).candidates,
    );
    assert!(again.is_empty());
}

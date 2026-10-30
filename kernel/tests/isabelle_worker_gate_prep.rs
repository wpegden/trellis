//! The proof-phase worker gate prep must route the declaration-signature
//! baseline on the REPO's backend.
//!
//! Regression: `declaration_hash_for_gate` pinned `BackendId::Lean`, so the
//! FIRST proof-formalization worker dispatch on an Isabelle run failed with
//! `filespec_split: no `-- BODY` marker line found` during gate prep — before
//! any burst launched — five times in a row, and the run halted on the
//! transport-failure budget.
//!
//! This is an INTEGRATION test deliberately: the lib's `#[cfg(test)]` build
//! swaps `declaration_hash_for_gate` for a legacy fallback, so only a test
//! linking the production lib exercises the routed body.

use std::collections::BTreeSet;
use std::fs;

use trellis_kernel::{
    prepare_worker_gate_observations, NodeId, WorkerGateObservationInput,
};

fn write_isabelle_repo(repo: &std::path::Path) {
    fs::create_dir_all(repo.join("Tablet")).expect("Tablet dir");
    fs::write(
        repo.join("trellis.config.json"),
        r#"{"workflow": {"default_target": "isabelle_hol"}}"#,
    )
    .expect("config");
    // A realistic proof node: no `-- BODY` marker anywhere, statement/proof
    // boundary is the command boundary.
    fs::write(
        repo.join("Tablet/GnpLawEdgePattern.thy"),
        "theory Tablet_GnpLawEdgePattern\n  imports Tablet_Preamble\nbegin\n\n\
         theorem GnpLawEdgePattern:\n  shows \"True\"\n  sorry\n\nend\n",
    )
    .expect("thy");
    fs::write(repo.join("Tablet/GnpLawEdgePattern.tex"), "\\begin{theorem}T\\end{theorem}\n")
        .expect("tex");
}

#[test]
fn isabelle_proof_gate_prep_captures_hashes_without_a_body_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().to_path_buf();
    write_isabelle_repo(&repo);

    let node = NodeId::from("GnpLawEdgePattern");
    let mut input = WorkerGateObservationInput::default();
    input.repo_path = repo;
    input.collect_observations = true;
    input.active_node = Some(node.clone());
    input.current_present_nodes = BTreeSet::from([node.clone()]);
    input.observation_plan.capture_expected_active_hash = true;
    input.observation_plan.capture_baseline_declaration_hashes = true;

    let output = prepare_worker_gate_observations(&input)
        .expect("gate prep must succeed on an Isabelle node without `-- BODY`");

    assert!(
        !output.expected_active_hash.is_empty(),
        "the active node's signature hash must be captured"
    );
    assert!(
        output.baseline_declaration_hashes.contains_key(&node),
        "the baseline hash map must cover the node"
    );
}

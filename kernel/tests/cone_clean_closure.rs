//! Audit M-4 — direct regression tests for
//! `ProtocolState::prune_local_closure_after_runtime_tablet_reset`.
//!
//! Background: the cone-clean reset path
//! (`runtime::restore_theorem_stating_node_and_prune_orphans`) calls this
//! pure-state helper after the runtime reobserves live structural state
//! and fingerprints from disk. Before this audit pass, the helper had
//! zero direct unit tests; bugs were only caught when an end-to-end
//! cone-clean cycle reproduced them. The cases below pin the
//! published behavior:
//!
//!   * owner_changed — a record whose owner is in the `changed_nodes`
//!     set must be removed.
//!   * owner_missing — a record whose owner left `live.present_nodes`
//!     must be removed.
//!   * referenced_missing — a record with a dep that left
//!     `live.present_nodes` must be removed.
//!   * referenced_changed — a record with a dep in the `changed_nodes`
//!     set must be removed.
//!   * unrelated_node — changes to a node not referenced by any record
//!     must leave records intact.
//!
//! Post-state assertions cover the reverse-index invariant:
//! `boundary_statement_consumers` / `strict_dep_consumers` must equal
//! the result of `recompute_local_closure_reverse_indices(&mut state)`
//! after any prune mutation.

use std::collections::{BTreeMap, BTreeSet};

use trellis_kernel::model::{
    recompute_local_closure_reverse_indices, LocalClosureRecord, NodeId, ProtocolState,
    WorkingSnapshot,
};
use trellis_kernel::{NodeCertificateEvidence, ObservedDeclarationUse};

fn node(id: &str) -> NodeId {
    NodeId::from(id)
}

/// Build a Lean `LocalClosureRecord` whose authoritative exact-owner
/// evidence covers dependencies historically serialized in all three
/// legacy map shapes. The maps remain populated to exercise compatibility,
/// but lifecycle invalidation is driven only by the exact evidence.
fn record_with_deps(
    owner: &str,
    boundary_deps: &[(&str, &str)],
    strict_theorem_deps: &[(&str, &str)],
    strict_def_deps: &[(&str, &str)],
    kernel_semantic_hashes: &[(&str, &str)],
) -> LocalClosureRecord {
    let mut record = LocalClosureRecord::default();
    record.node = node(owner);
    record.closure_version = "v1".to_string();
    record.toolchain_hash = "tc".to_string();
    record.lake_manifest_hash = "lk".to_string();
    record.preamble_hash = "pre".to_string();
    record.approved_axioms_hash = "ax".to_string();
    record.active_decl_hash = "decl".to_string();
    record.active_statement_hash = "stmt".to_string();
    record.kernel_axioms = BTreeSet::from(["propext".to_string()]);
    record.boundary_theorems = boundary_deps
        .iter()
        .map(|(k, v)| (node(k), (*v).to_string()))
        .collect();
    record.strict_theorem_deps = strict_theorem_deps
        .iter()
        .map(|(k, v)| (node(k), (*v).to_string()))
        .collect();
    record.strict_definition_deps = strict_def_deps
        .iter()
        .map(|(k, v)| (node(k), (*v).to_string()))
        .collect();
    let exact_owners: BTreeSet<NodeId> = boundary_deps
        .iter()
        .chain(strict_theorem_deps)
        .chain(strict_def_deps)
        .map(|(owner, _)| node(owner))
        .collect();
    record.node_certificate_evidence = Some(NodeCertificateEvidence {
        exact_declaration_uses: exact_owners
            .into_iter()
            .map(|owner| ObservedDeclarationUse {
                reached_declaration: owner.as_str().to_owned(),
                owner,
                declaration_kind: "theorem".to_owned(),
                visibility: "exported".to_owned(),
            })
            .collect(),
        ..NodeCertificateEvidence::default()
    });
    record.kernel_semantic_hashes = kernel_semantic_hashes
        .iter()
        .map(|(k, v)| (node(k), (*v).to_string()))
        .collect();
    record.accepted_at_snapshot_id = "snap-1".to_string();
    record
}

/// Build a ProtocolState with `owner_nodes` as proof nodes (sorry-free,
/// present) and `dep_nodes` as present non-proof helpers. Live and
/// committed snapshots agree. `corr_current_fingerprints` populated for
/// every present node with the supplied value so the C-2 canonical
/// predicate has consistent fingerprints to compare against the
/// records' `kernel_semantic_hashes`.
fn build_state(
    owner_nodes: &[&str],
    dep_nodes: &[&str],
    fingerprint_overrides: &[(&str, &str)],
) -> ProtocolState {
    let mut present: BTreeSet<NodeId> = BTreeSet::new();
    let mut proof_nodes: BTreeSet<NodeId> = BTreeSet::new();
    let mut fingerprints: BTreeMap<NodeId, String> = BTreeMap::new();
    for n in owner_nodes {
        present.insert(node(n));
        proof_nodes.insert(node(n));
        fingerprints.insert(node(n), "fp-owner".to_string());
    }
    for n in dep_nodes {
        present.insert(node(n));
        fingerprints.insert(node(n), "fp-dep".to_string());
    }
    for (k, v) in fingerprint_overrides {
        fingerprints.insert(node(k), (*v).to_string());
    }
    let mut state = ProtocolState::default();
    state.proof_nodes = proof_nodes;
    state.live = WorkingSnapshot {
        present_nodes: present,
        open_nodes: BTreeSet::new(),
        corr_current_fingerprints: fingerprints,
        ..WorkingSnapshot::default()
    };
    state
}

fn install_record(state: &mut ProtocolState, record: LocalClosureRecord) {
    let owner = record.node.clone();
    state.local_closure_records.insert(owner, record);
    recompute_local_closure_reverse_indices(state);
}

fn assert_reverse_indices_consistent(state: &ProtocolState) {
    let expected_boundary: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
    let mut expected_strict: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
    for (owner, record) in &state.local_closure_records {
        for dep in record.authoritative_dependency_owners(state) {
            expected_strict
                .entry(dep)
                .or_default()
                .insert(owner.clone());
        }
    }
    assert_eq!(
        state.boundary_statement_consumers, expected_boundary,
        "boundary_statement_consumers must equal the recomputed reverse index"
    );
    assert_eq!(
        state.strict_dep_consumers, expected_strict,
        "strict_dep_consumers must equal the recomputed reverse index"
    );
}

#[test]
fn prune_removes_record_when_owner_is_in_changed_nodes() {
    // owner_changed: cone-clean targets the consumer C directly.
    // C's record must be dropped; C becomes unverified (sorry-free
    // proof_node, still present).
    let mut state = build_state(&["C"], &["HelperB"], &[]);
    install_record(
        &mut state,
        record_with_deps(
            "C",
            &[("HelperB", "stmt-b")],
            &[],
            &[],
            &[("HelperB", "fp-dep")],
        ),
    );

    let changed: BTreeSet<NodeId> = BTreeSet::from([node("C")]);
    let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);

    assert!(
        removed.contains(&node("C")),
        "C's record must be removed (owner_changed); got removed={:?}",
        removed
    );
    assert!(
        !state.local_closure_records.contains_key(&node("C")),
        "C's record must no longer be present after prune"
    );
    assert!(
        state.local_closure_unverified_nodes.contains(&node("C")),
        "C must be inserted into unverified (sorry-free proof_node)"
    );
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_removes_record_when_owner_left_present_nodes() {
    // owner_missing: cone-clean deleted C entirely (orphaned by
    // removal of a coarse parent). Record must drop; C must NOT
    // appear in unverified because it's no longer present.
    let mut state = build_state(&["HelperB"], &[], &[]);
    // Inject a record for an owner that's NOT in present_nodes —
    // simulates the state right after the cone-clean reobservation
    // removed C from `live.present_nodes`.
    let stale_record = record_with_deps(
        "C",
        &[("HelperB", "stmt-b")],
        &[],
        &[],
        &[("HelperB", "fp-owner")],
    );
    state.local_closure_records.insert(node("C"), stale_record);
    recompute_local_closure_reverse_indices(&mut state);

    let changed: BTreeSet<NodeId> = BTreeSet::new();
    let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);

    assert!(
        removed.contains(&node("C")),
        "C's record must be removed (owner_missing); got removed={:?}",
        removed
    );
    assert!(!state.local_closure_records.contains_key(&node("C")));
    assert!(
        !state.local_closure_unverified_nodes.contains(&node("C")),
        "C must NOT be in unverified since it's absent from present_nodes"
    );
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_removes_record_when_dep_left_present_nodes() {
    // referenced_missing: C's helper H was deleted (cone-clean
    // removed it as an orphan). C's record references a now-missing
    // dep, so it must drop. C stays in unverified (it's still
    // present and sorry-free).
    let mut state = build_state(&["C"], &[], &[]);
    // Note: HelperB is NOT in present_nodes (deliberately missing).
    install_record(
        &mut state,
        record_with_deps(
            "C",
            &[("HelperB", "stmt-b")],
            &[],
            &[],
            &[("HelperB", "fp-dep")],
        ),
    );

    let changed: BTreeSet<NodeId> = BTreeSet::new();
    let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);

    assert!(
        removed.contains(&node("C")),
        "C's record must be removed (referenced_missing); got removed={:?}",
        removed
    );
    assert!(state.local_closure_unverified_nodes.contains(&node("C")));
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_removes_record_when_dep_is_in_changed_nodes() {
    // referenced_changed: H was directly mutated by cone-clean; C
    // references H, so C's record drops.
    let mut state = build_state(&["C"], &["HelperB"], &[]);
    install_record(
        &mut state,
        record_with_deps(
            "C",
            &[("HelperB", "stmt-b")],
            &[],
            &[],
            &[("HelperB", "fp-dep")],
        ),
    );

    let changed: BTreeSet<NodeId> = BTreeSet::from([node("HelperB")]);
    let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);

    assert!(
        removed.contains(&node("C")),
        "C's record must be removed (referenced_changed); got removed={:?}",
        removed
    );
    assert!(state.local_closure_unverified_nodes.contains(&node("C")));
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_keeps_unrelated_record_when_changed_node_is_not_referenced() {
    // unrelated_node: cone-clean target X is not referenced by C's
    // record. C's record must survive. Reverse indices must remain
    // consistent.
    let mut state = build_state(&["C"], &["HelperB", "X"], &[]);
    install_record(
        &mut state,
        record_with_deps(
            "C",
            &[("HelperB", "stmt-b")],
            &[],
            &[],
            &[("HelperB", "fp-dep")],
        ),
    );

    let changed: BTreeSet<NodeId> = BTreeSet::from([node("X")]);
    let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);

    assert!(
        !removed.contains(&node("C")),
        "C's record must survive (unrelated change); got removed={:?}",
        removed
    );
    assert!(
        state.local_closure_records.contains_key(&node("C")),
        "C's record must still be present after prune"
    );
    // X is a non-proof helper (we put it in `dep_nodes`), so the
    // prune's "present ∩ changed ∩ proof_nodes ∩ ¬open" insert for
    // unverified must NOT fire for X.
    assert!(
        !state.local_closure_unverified_nodes.contains(&node("X")),
        "X is not a proof_node; must not enter unverified"
    );
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_retains_unverified_entries_for_open_owners() {
    // Open owners remain certificate-eligible: their truthful closure may
    // include `sorryAx`, while `open_nodes` separately blocks completion.
    // Pruning must therefore retain an explicitly invalidated open owner.
    let mut state = build_state(&["C"], &[], &[]);
    state.live.open_nodes.insert(node("C"));
    state.local_closure_unverified_nodes.insert(node("C"));

    let removed = state.prune_local_closure_after_runtime_tablet_reset(&BTreeSet::new());
    assert!(removed.is_empty(), "no records to remove");
    assert!(
        state.local_closure_unverified_nodes.contains(&node("C")),
        "an open registered owner remains scheduled for truthful issuance"
    );
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_drops_failures_for_nodes_no_longer_unverified() {
    // Plan §7.0 contract: failure summaries are only meaningful while
    // the owner is in `local_closure_unverified_nodes`. After a prune
    // that removes an ineligible owner from the unverified set (e.g. the owner
    // vanished), the failure summary must drop too.
    use trellis_kernel::model::ErrorSummary;
    let mut state = build_state(&["C"], &[], &[]);
    state.live.present_nodes.remove(&node("C"));
    state.local_closure_unverified_nodes.insert(node("C"));
    state.local_closure_failures.insert(
        node("C"),
        ErrorSummary {
            status: "axiom_violation".to_string(),
            returncode: 0,
            timed_out: false,
            stderr_excerpt: "uses sorryAx".to_string(),
            axiom_violations: vec!["sorryAx".to_string()],
            strict_errors: vec![],
            captured_at_cycle: 1,
            retry_count: 0,
            last_attempt_cycle: 0,
            next_retry_cycle: 0,
            retry_exhausted: false,
        },
    );

    let _ = state.prune_local_closure_after_runtime_tablet_reset(&BTreeSet::new());
    assert!(
        !state.local_closure_failures.contains_key(&node("C")),
        "failure summary must drop when its owner is no longer unverified"
    );
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_inserts_changed_present_proof_nodes_into_unverified() {
    // The prune helper has a "present ∩ changed ∩ proof_nodes ∩
    // ¬open" insert step. Cover it directly: a sorry-free proof_node
    // P that's in `changed_nodes` AND has no record yet must land in
    // unverified.
    let mut state = build_state(&["P"], &[], &[]);
    // Pre-condition: P has no record, no unverified entry.
    assert!(!state.local_closure_records.contains_key(&node("P")));
    assert!(!state.local_closure_unverified_nodes.contains(&node("P")));

    let changed: BTreeSet<NodeId> = BTreeSet::from([node("P")]);
    let _ = state.prune_local_closure_after_runtime_tablet_reset(&changed);

    assert!(
        state.local_closure_unverified_nodes.contains(&node("P")),
        "P (sorry-free, present, proof_node, changed) must be in unverified"
    );
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_exact_theorem_owner_change_triggers_record_drop() {
    // Exact owner evidence is authoritative even when the compatibility
    // serialization also calls the reached declaration a strict theorem.
    let mut state = build_state(&["C"], &["ThmT"], &[]);
    install_record(
        &mut state,
        record_with_deps(
            "C",
            &[],
            &[("ThmT", "stmt-t")],
            &[],
            &[("ThmT", "fp-dep")],
        ),
    );

    let changed: BTreeSet<NodeId> = BTreeSet::from([node("ThmT")]);
    let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);

    assert!(
        removed.contains(&node("C")),
        "C's record must drop when a strict_theorem_dep is in changed_nodes"
    );
    assert_reverse_indices_consistent(&state);
}

#[test]
fn prune_exact_definition_owner_change_triggers_record_drop() {
    // Declaration kind does not change exact-owner invalidation.
    let mut state = build_state(&["C"], &["DefD"], &[]);
    install_record(
        &mut state,
        record_with_deps(
            "C",
            &[],
            &[],
            &[("DefD", "stmt-d")],
            &[("DefD", "fp-dep")],
        ),
    );

    let changed: BTreeSet<NodeId> = BTreeSet::from([node("DefD")]);
    let removed = state.prune_local_closure_after_runtime_tablet_reset(&changed);

    assert!(
        removed.contains(&node("C")),
        "C's record must drop when a strict_definition_dep is in changed_nodes"
    );
    assert_reverse_indices_consistent(&state);
}

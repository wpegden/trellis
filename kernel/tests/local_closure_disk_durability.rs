//! Audit L-1 — regression test for the `DeleteLocalClosureRecord` disk
//! durability ordering invariants.
//!
//! Before the L-1 fix, the runtime processed the engine's
//! `ProtocolCommand::DeleteLocalClosureRecord` inline during the command
//! loop — which runs BEFORE the checkpoint sink commit. If the sink
//! failed, in-memory state rolled back to `pre_step_state` (which still
//! contained the record), but the disk file was already gone.
//! Restart-time migration then saw state.json claim a record while the
//! per-node JSON was missing.
//!
//! The fix buffers the disk deletes and flushes them only AFTER the
//! checkpoint sink succeeds AND `persist_state` makes the in-memory
//! tombstone durable.
//!
//! Test strategy: this file pins the load-bearing primitive the L-1
//! flush loop iterates and the durability-contract sequence the runtime
//! must follow. End-to-end driving through `step_with_checkpoint_sink`
//! is heavy (requires a full sample tablet repo + sound/corr/paper
//! state) and is exercised by `runtime.rs`'s own
//! `delete_persisted_local_closure_record_*` tests. This file's
//! contribution is the per-node primitive invariants the buffer-flush
//! relies on.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use trellis_kernel::model::NodeId;
use trellis_kernel::runtime::{
    delete_persisted_local_closure_record, persisted_record_file_name,
};

fn local_tempdir() -> TempDir {
    let tmp_root = std::env::current_dir()
        .expect("current dir")
        .join(".tmp-tests");
    fs::create_dir_all(&tmp_root).expect("tmp root");
    tempfile::tempdir_in(&tmp_root).expect("tempdir")
}

fn persisted_record_dir(runtime_root: &Path) -> PathBuf {
    runtime_root
        .join("checker-state")
        .join("local-closure-records")
}

fn write_persisted_record(runtime_root: &Path, node: &NodeId) -> PathBuf {
    let dir = persisted_record_dir(runtime_root);
    fs::create_dir_all(&dir).expect("records dir");
    let path = dir.join(persisted_record_file_name(node));
    fs::write(&path, r#"{"node":"a"}"#).expect("write record");
    path
}

#[test]
fn flush_loop_deletes_every_queued_record_file() {
    // L-1 load-bearing primitive: the runtime's deferred flush loop
    // iterates `pending_local_closure_disk_deletes: Vec<NodeId>` and
    // calls `delete_persisted_local_closure_record(&self.paths.root,
    // node)` for each entry. The per-node helper must:
    //   (a) succeed on each entry without aborting on missing files;
    //   (b) delete every file whose name matches
    //       `persisted_record_file_name(node)`.
    //
    // Pin the helper's per-call behaviour so a future refactor (e.g.
    // parallel deletes, batched fsync) preserves the per-node
    // guarantee.
    let dir = local_tempdir();
    let nodes = [NodeId::from("a"), NodeId::from("b"), NodeId::from("c")];
    let paths: Vec<_> = nodes
        .iter()
        .map(|n| write_persisted_record(dir.path(), n))
        .collect();
    for p in &paths {
        assert!(p.exists(), "precondition: {} must exist", p.display());
    }
    for node in &nodes {
        delete_persisted_local_closure_record(dir.path(), node);
    }
    for p in &paths {
        assert!(
            !p.exists(),
            "flush must delete every queued node's file; {} survived",
            p.display()
        );
    }
}

#[test]
fn delete_record_helper_is_idempotent_for_missing_files() {
    // The L-1 flush loop runs unconditionally. A queued entry whose
    // disk file was already removed (e.g. by an aborted previous
    // step) must NOT cause the flush to abort — otherwise a single
    // already-deleted entry would prevent later entries from
    // flushing, regressing the cleanup the engine intended.
    //
    // The helper is idempotent on missing files (no-op); pin that
    // here so it stays load-bearing.
    let dir = local_tempdir();
    let node = NodeId::from("absent");
    // Pre-condition: no records dir exists yet.
    assert!(!persisted_record_dir(dir.path()).exists());
    delete_persisted_local_closure_record(dir.path(), &node);
    // Post-condition: still no panic / error, no records dir created.
    assert!(!persisted_record_dir(dir.path()).exists());
}

#[test]
fn flush_loop_continues_through_missing_entries() {
    // Defensive: a flush loop iterating `[a, MISSING, c]` must
    // delete BOTH `a` and `c`. The L-1 fix's loop does this because
    // the per-node helper is idempotent on missing.
    let dir = local_tempdir();
    let present_a = write_persisted_record(dir.path(), &NodeId::from("a"));
    let present_c = write_persisted_record(dir.path(), &NodeId::from("c"));
    // Don't write `b`'s file.
    let queue = [
        NodeId::from("a"),
        NodeId::from("b"), // not on disk
        NodeId::from("c"),
    ];
    for node in &queue {
        delete_persisted_local_closure_record(dir.path(), node);
    }
    assert!(!present_a.exists(), "a should be deleted");
    assert!(!present_c.exists(), "c should be deleted");
}

#[test]
fn delete_helper_does_not_touch_unqueued_files() {
    // The flush loop only deletes files for queued NodeIds. Make
    // sure a single delete call does NOT touch sibling files.
    let dir = local_tempdir();
    let other_path = write_persisted_record(dir.path(), &NodeId::from("other"));
    let target = NodeId::from("target_node");
    let target_path = write_persisted_record(dir.path(), &target);
    delete_persisted_local_closure_record(dir.path(), &target);
    assert!(!target_path.exists(), "target deleted");
    assert!(
        other_path.exists(),
        "sibling file must survive — flush loop must scope to the queued node"
    );
}

#[test]
fn delete_helper_handles_empty_runtime_root_gracefully() {
    // If the runtime root path is the empty path or non-existent,
    // the helper must not panic — it's load-bearing during early
    // initialize, where the records dir may not exist yet.
    let dir = local_tempdir();
    let missing_root = dir.path().join("does-not-exist");
    delete_persisted_local_closure_record(&missing_root, &NodeId::from("any"));
    // No assertion needed; the test passes iff no panic.
}

#[test]
fn delete_helper_handles_escaped_node_names() {
    // Patch C-Q Q5 lockstep: persist and delete use the same
    // `persisted_record_file_name` for filename escaping. A NodeId
    // containing `/` must round-trip cleanly through the L-1 flush
    // loop. Even though current NodeIds don't carry `/`, future
    // schema evolution might; pin the contract.
    let dir = local_tempdir();
    let weird = NodeId::from("Group/Inner");
    let path = write_persisted_record(dir.path(), &weird);
    assert!(path.exists(), "precondition");
    // Filename must have `/` escaped to `_`.
    assert_eq!(
        path.file_name().and_then(|s| s.to_str()),
        Some("Group_Inner.json"),
        "Patch C-Q Q5 escape must produce 'Group_Inner.json'"
    );
    delete_persisted_local_closure_record(dir.path(), &weird);
    assert!(!path.exists(), "delete must agree with persistence escape");
}

/// Documentation-style test that pins the L-1 fix's ordering invariant
/// by reading the runtime.rs source. The actual end-to-end driving
/// requires a full sample tablet repo + verifier state and lives in
/// `runtime.rs`'s in-tree tests; this test ensures a future refactor
/// can't accidentally move the flush loop to a pre-durability point
/// without an obvious source-level marker.
#[test]
fn l1_flush_loop_runs_after_persist_state_in_source() {
    // Hard-code the relative path so the test fails loudly if
    // runtime.rs is renamed (the L-1 invariant lives in
    // step_with_checkpoint_sink there).
    let runtime_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("runtime.rs");
    let text = fs::read_to_string(&runtime_src)
        .unwrap_or_else(|e| panic!("read {}: {e}", runtime_src.display()));
    // The flush loop must appear AFTER the persist_state call in
    // step_with_checkpoint_sink. Find both and assert order.
    let persist_idx = text
        .find("self.persist_state()?;")
        .expect("self.persist_state()?; must appear in runtime.rs");
    let flush_idx = text
        .find("for node in &pending_local_closure_disk_deletes {")
        .expect(
            "L-1 flush loop must appear in runtime.rs — pending_local_closure_disk_deletes Vec \
             must be flushed after the durability barrier",
        );
    assert!(
        flush_idx > persist_idx,
        "L-1 flush loop must appear AFTER self.persist_state()? in runtime.rs::step_with_checkpoint_sink. \
         Found persist_state at offset {persist_idx}, flush at {flush_idx}. \
         A flush BEFORE persist_state would regress L-1 — disk delete would race the durability barrier."
    );
    // Also check: the buffer is declared BEFORE the command loop.
    let buffer_decl = text
        .find("let mut pending_local_closure_disk_deletes: Vec<NodeId>")
        .expect("L-1 pending-deletes buffer must be declared in runtime.rs");
    let command_loop = text
        .find("for command in &outcome.commands {")
        .expect("command loop must appear in runtime.rs");
    assert!(
        buffer_decl < command_loop,
        "L-1 pending-deletes buffer must be declared BEFORE the command loop \
         (so the loop can push into it). Found buffer at {buffer_decl}, loop at {command_loop}."
    );
    // And: the DeleteLocalClosureRecord arm must push to the buffer,
    // not call delete_persisted_local_closure_record directly.
    let delete_arm = text
        .find("ProtocolCommand::DeleteLocalClosureRecord { node } => {")
        .expect("DeleteLocalClosureRecord match arm must appear");
    // After the arm, the next ~600 chars should mention
    // `pending_local_closure_disk_deletes.push(`. (600 is a coarse
    // window that's well within the arm's body.)
    let arm_body = &text[delete_arm..text.len().min(delete_arm + 1000)];
    assert!(
        arm_body.contains("pending_local_closure_disk_deletes.push("),
        "L-1: the DeleteLocalClosureRecord arm must defer via push, not inline-delete. \
         Found arm but no push call within the next 1000 chars of source."
    );
    assert!(
        !arm_body.starts_with(
            "ProtocolCommand::DeleteLocalClosureRecord { node } => {\n                    delete_persisted_local_closure_record"
        ),
        "L-1: the DeleteLocalClosureRecord arm must NOT call delete_persisted_local_closure_record \
         directly inline — that would regress the durability ordering."
    );
}

// Silence unused import warning for BTreeSet — pulled in case a
// future test wants to type-spell a node-set parameter.
fn _silence_unused() {
    let _: BTreeSet<NodeId> = BTreeSet::new();
}

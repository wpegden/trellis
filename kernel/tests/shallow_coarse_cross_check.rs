//! Cross-check: run kernel's `shallowly_closed_from_coarse` predicate
//! over a real `protocol_state.json` snapshot and emit JSON to stdout.
//! Compared against a Python port of the viewer's JS predicate by
//! `scripts/verify_shallow_coarse_closure.py`.
//!
//! Auto-skips when the input path doesn't exist (so CI runs are
//! harmless on machines without a live runtime).
//!
//! Inputs (via env vars, both optional):
//!   TRELLIS_VERIFY_STATE_PATH  default: ${TRELLIS_ROOT:-/path/to/trellis}/math/example-run-runtime/protocol_state.json
//!   TRELLIS_VERIFY_OUT_PATH    default: /tmp/kernel_shallow_coarse_result.json
//!
//! Output JSON shape:
//!   {
//!     "input_path": "...",
//!     "coarse_nodes_total": <int>,
//!     "coarse_nodes_shallow_closed": ["NodeA", "NodeB", ...],
//!     "per_node": { "NodeA": true, "NodeB": false, ... }   // only coarse nodes
//!   }

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;
use trellis_kernel::{shallowly_closed_from_coarse, NodeId};

#[derive(Debug, Deserialize)]
struct LiveSnapshot {
    #[serde(default)]
    present_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    open_nodes: BTreeSet<NodeId>,
}

#[derive(Debug, Deserialize)]
struct ProtocolStateLite {
    #[serde(default)]
    live: Option<LiveSnapshot>,
    #[serde(default)]
    committed: Option<LiveSnapshot>,
    #[serde(default)]
    deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    #[serde(default)]
    coarse_dag_nodes: BTreeSet<NodeId>,
}

#[test]
fn shallow_coarse_cross_check_on_live_snapshot() {
    let state_path = std::env::var("TRELLIS_VERIFY_STATE_PATH")
        .unwrap_or_else(|_| "${TRELLIS_ROOT:-/path/to/trellis}/math/example-run-runtime/protocol_state.json".into());
    let out_path = std::env::var("TRELLIS_VERIFY_OUT_PATH")
        .unwrap_or_else(|_| "/tmp/kernel_shallow_coarse_result.json".into());

    let path = PathBuf::from(&state_path);
    if !path.exists() {
        eprintln!(
            "[shallow_coarse_cross_check] skipping: {} not found",
            state_path
        );
        return;
    }

    let raw = std::fs::read_to_string(&path).expect("read state file");
    let state: ProtocolStateLite = serde_json::from_str(&raw).expect("parse state JSON");

    // Mirror viewer: use committed snapshot for present/open (matches
    // viewer's `isKernelClosed` which keys on `committed.{present,open}_nodes`).
    let committed = state
        .committed
        .as_ref()
        .or(state.live.as_ref())
        .expect("snapshot has neither committed nor live");
    let present = &committed.present_nodes;
    let open = &committed.open_nodes;
    let deps = &state.deps;
    let coarse = &state.coarse_dag_nodes;

    let mut memo = BTreeMap::new();
    let mut per_node: BTreeMap<NodeId, bool> = BTreeMap::new();
    for n in coarse {
        let v = shallowly_closed_from_coarse(n, present, open, deps, coarse, &mut memo);
        per_node.insert(n.clone(), v);
    }
    let shallow_closed: BTreeSet<&NodeId> = per_node
        .iter()
        .filter_map(|(n, &v)| if v { Some(n) } else { None })
        .collect();

    let report = json!({
        "input_path": state_path,
        "coarse_nodes_total": coarse.len(),
        "coarse_nodes_shallow_closed": shallow_closed,
        "per_node": per_node,
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&report).unwrap())
        .expect("write output");

    // Also stash a count so a casual `cargo test -- --nocapture` shows it.
    eprintln!(
        "[shallow_coarse_cross_check] {} coarse nodes total, {} shallow-closed, wrote {}",
        coarse.len(),
        shallow_closed.len(),
        out_path
    );
}

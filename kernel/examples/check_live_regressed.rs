// Coarse-regression deadlock sanity check: load a READ-ONLY COPY of the
// live protocol_state.json under the fixed kernel and report the S8
// regression set + anchor-change gate. Precedent: load_live_state.rs
// (master-live migration check).
use trellis_kernel::model::{recompute_local_closure_reverse_indices, ProtocolState};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: check_live_regressed <path-to-copied-protocol_state.json>");
    let raw = std::fs::read_to_string(&path).expect("read state file");
    println!("read {} bytes", raw.len());

    let mut state: ProtocolState = serde_json::from_str(&raw).expect("deserialize");
    // Exact runtime.rs post-load normalize sequence.
    state.normalize_all_structural_state();
    recompute_local_closure_reverse_indices(&mut state);
    println!("=== LOAD + NORMALIZE OK ===");

    println!("phase                       = {:?}", state.phase);
    println!("cycle                       = {}", state.cycle);
    println!("active_coarse_node          = {:?}", state.active_coarse_node);
    println!("coarse_dag_nodes.len        = {}", state.coarse_dag_nodes.len());
    println!(
        "committed.present_nodes.len = {}",
        state.committed.present_nodes.len()
    );
    println!(
        "ever_shallow_coarse_closed.len = {}",
        state.ever_shallow_coarse_closed.len()
    );

    let phantoms: Vec<_> = state
        .ever_shallow_coarse_closed
        .iter()
        .filter(|n| !state.committed.present_nodes.contains(*n))
        .collect();
    println!("history entries absent from committed present (phantoms) = {phantoms:?}");

    let regressed = state.ever_shallow_coarse_closed_regressed();
    println!("ever_shallow_coarse_closed_regressed() = {regressed:?}");
    println!(
        "active_coarse_change_allowed() = {}",
        state.active_coarse_change_allowed()
    );
    println!(
        "kernel_hinted_next_active_coarse_nodes().len = {}",
        state.kernel_hinted_next_active_coarse_nodes().len()
    );

    if regressed.is_empty() {
        println!("=== RESULT: regressed() == EMPTY — deadlock unlocked ===");
    } else {
        println!("=== RESULT: regressed() NON-EMPTY — still locked ===");
        std::process::exit(1);
    }
}

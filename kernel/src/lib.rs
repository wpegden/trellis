extern crate self as trellis_kernel;

/// Single discipline for test-time env mutation; see `test_env.rs`.
#[cfg(test)]
mod test_env;

#[cfg(test)]
pub(crate) use test_env::{process_globals_test_guard, EnvScope};

pub mod abstract_model;
pub mod add_targets;
pub mod artifact_validation;
pub mod assumptions_registry;
pub mod audit_normalization;
/// Proof-assistant backend seam (Lean is the sole impl; kept in the LIB
/// only — `runtime_cli_observations.rs` is double-compiled via the bin
/// `#[path]`, so referencing `backend` through `trellis_kernel::backend`
/// avoids a duplicate compilation into the bin).
pub mod backend;
pub mod bridge_verifier_bindings;
pub mod burst_history;
pub mod cache_key;
pub mod check_ledger;
pub mod disk_cache;
pub mod dormant_store;
pub mod engine;
pub mod filespec;
pub mod filespec_split;
pub mod isabelle_filespec;
pub mod legacy_import;
pub mod model;
pub mod node_certificate;
pub mod paper_diff;
pub mod paper_fingerprints;
pub mod paper_targets;
pub mod phase0;
pub mod process_memory;
pub mod progress_history;
pub mod request_contracts;
pub mod review_normalization;
pub mod revision_import;
pub mod runtime;
pub(crate) mod runtime_cli_observations;
pub mod shared_state_codec;
pub mod sidecar;

/// Narrow re-export surface for the supervisor `Run` loop and external
/// consumers (Python tests, viewer adapter): just the halt-marker
/// constants + presence-check helpers, NOT the full observations module.
/// Keeps the legacy `pub(crate)` boundary intact for everything else.
pub mod runtime_cli_observations_halt {
    pub use crate::runtime_cli_observations::{
        acknowledge_checker_disagreement_halt_marker, acknowledge_system_feedback_fingerprint,
        any_halt_marker_present, checker_disagreement_halt_marker_path,
        checker_disagreement_halt_marker_present, halt_history_path,
        load_system_feedback_ack_store, system_feedback_ack_store_path,
        system_feedback_fingerprint, system_feedback_halt_enabled,
        system_feedback_halt_marker_path, system_feedback_halt_marker_present,
        system_feedback_log_path, write_acceptance_transition_disagreement_halt_marker_at,
        write_runtime_error_halt_marker_at, write_system_feedback_halt_marker,
        HaltMarkerAckOutcome, SystemFeedbackAck, SystemFeedbackAckResult, SystemFeedbackAckStore,
        CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME, RUNTIME_ERROR_HALT_MARKER_FILENAME,
        SYSTEM_FEEDBACK_ACK_STORE_FILENAME, SYSTEM_FEEDBACK_HALT_ENV,
        SYSTEM_FEEDBACK_HALT_MARKER_FILENAME, SYSTEM_FEEDBACK_LOG_FILENAME,
    };
}

/// Narrow re-export surface for the local-closure probe's end-to-end
/// integration tests (`kernel/tests/local_closure_smoke.rs`): the wire
/// parser plus the Patch C-K present-node validator, NOT the full
/// observations module. Those tests run the real Lean probe against
/// `kernel/tests/fixtures/local_closure_smoke/`, so they live in the
/// integration lane where the fixture gate is enforced — the `--lib`
/// lane stays free of any Lean dependency. Both functions are pure
/// transforms over already-public types and enforce nothing a caller
/// could bypass by invoking them directly.
pub mod runtime_cli_observations_probe {
    pub use crate::runtime_cli_observations::{
        parse_local_closure_response, validate_probe_present_nodes,
    };
}
pub mod tablet_root;
pub mod tablet_support;
pub mod trust_base;
pub mod verification_normalization;
pub mod worker_normalization;
pub mod worker_transition;

pub use abstract_model::*;
pub use add_targets::*;
pub use artifact_validation::*;
pub use audit_normalization::*;
pub use bridge_verifier_bindings::*;
pub use engine::{apply_event, ProtocolCommand, ProtocolEvent, TransitionError, TransitionOutcome};
pub use filespec::*;
pub use legacy_import::*;
pub use model::*;
pub use node_certificate::*;
pub use paper_diff::*;
pub use paper_fingerprints::*;
pub use paper_targets::*;
pub use phase0::*;
pub use process_memory::*;
pub use progress_history::*;
pub use request_contracts::*;
pub use review_normalization::*;
pub use revision_import::*;
pub use runtime::*;
pub use sidecar::*;
pub use tablet_root::*;
pub use tablet_support::*;
pub use verification_normalization::*;
pub use worker_normalization::*;
pub use worker_transition::*;

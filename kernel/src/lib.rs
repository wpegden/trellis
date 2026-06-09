extern crate self as trellis_kernel;

#[cfg(test)]
static KERNEL_CACHE_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn kernel_cache_env_test_guard() -> std::sync::MutexGuard<'static, ()> {
    KERNEL_CACHE_ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub mod abstract_model;
pub mod artifact_validation;
pub mod audit_normalization;
pub mod bridge_verifier_bindings;
pub mod burst_history;
pub mod cache_key;
pub mod check_ledger;
pub mod disk_cache;
pub mod engine;
pub mod filespec;
pub mod filespec_split;
pub mod legacy_import;
pub mod model;
pub mod paper_fingerprints;
pub mod paper_targets;
pub mod progress_history;
pub mod request_contracts;
pub mod review_normalization;
pub mod runtime;
pub(crate) mod runtime_cli_observations;

/// Narrow re-export surface for the supervisor `Run` loop and external
/// consumers (Python tests, viewer adapter): just the halt-marker
/// constants + presence-check helpers, NOT the full observations module.
/// Keeps the legacy `pub(crate)` boundary intact for everything else.
pub mod runtime_cli_observations_halt {
    pub use crate::runtime_cli_observations::{
        acknowledge_checker_disagreement_halt_marker, any_halt_marker_present,
        checker_disagreement_halt_marker_path, checker_disagreement_halt_marker_present,
        halt_history_path, system_feedback_halt_marker_path, system_feedback_halt_marker_present,
        write_system_feedback_halt_marker, HaltMarkerAckOutcome,
        CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME, SYSTEM_FEEDBACK_HALT_MARKER_FILENAME,
    };
}
pub mod tablet_root;
pub mod tablet_support;
pub mod verification_normalization;
pub mod worker_normalization;

pub use abstract_model::*;
pub use artifact_validation::*;
pub use audit_normalization::*;
pub use bridge_verifier_bindings::*;
pub use engine::{apply_event, ProtocolCommand, ProtocolEvent, TransitionError, TransitionOutcome};
pub use filespec::*;
pub use legacy_import::*;
pub use model::*;
pub use paper_fingerprints::*;
pub use paper_targets::*;
pub use progress_history::*;
pub use request_contracts::*;
pub use review_normalization::*;
pub use runtime::*;
pub use tablet_root::*;
pub use tablet_support::*;
pub use verification_normalization::*;
pub use worker_normalization::*;

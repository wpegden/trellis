//! Shared test helpers for kernel integration tests.
//!
//! Lives under `tests/common/` (subdirectory) so cargo treats it as a module
//! to be `mod`'d from each integration test, rather than as its own
//! integration test target.

#![allow(dead_code)]

use std::path::PathBuf;

/// Create a `tempfile::TempDir` rooted at `CARGO_TARGET_TMPDIR` if cargo
/// provides it (which it does for integration tests under `tests/`),
/// otherwise fall back to `std::env::temp_dir()`.
///
/// Why: `tempfile::tempdir()` defaults to `/tmp` on Linux. If a test process
/// is killed (Bash command timeout, Ctrl-C, OOM) before its `TempDir` Drop
/// impl runs, the dir is orphaned in `/tmp` and never cleaned. With kernel
/// integration tests that materialize the trellis source tree per-test
/// (~145 MiB each), a few hundred orphaned dirs fill the `/tmp` partition.
/// Rooting under `target/tmp/<test-target>/` keeps orphans scoped to the
/// build directory, where `cargo clean` (or a one-line `rm -rf target/tmp/`)
/// reclaims them.
pub fn project_tempdir() -> tempfile::TempDir {
    let root = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&root).expect("create tempdir scratch root");
    tempfile::Builder::new()
        .tempdir_in(&root)
        .expect("project_tempdir")
}

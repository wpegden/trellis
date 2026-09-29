//! The crate's single discipline for mutating process-global state that
//! request-payload derivation reads: environment variables, and the
//! test-only override slots production code consults (currently
//! `SOURCE_RECOURSE_AVAILABLE_OVERRIDE` in `request_contracts`).
//!
//! Why this exists: `setenv(3)` mutates the process-wide `environ` array
//! in place and may reallocate it. A concurrent `getenv(3)` — even one
//! reading a completely *different* variable — can therefore observe a
//! torn view and transiently miss a variable that is in fact set. So an
//! env mutation in one test does not merely leak its own value into a
//! sibling that reads the same variable; it can corrupt any sibling
//! reading any variable at all.
//!
//! That matters here because `ProtocolState::expected_request` reads
//! several env-backed tuning knobs (`csc_last_clean_threshold`,
//! `stuck_coarse_repair_threshold`, the audit-dispatch cooldown, the
//! sidecar window, the no-sound-progress window). The engine derives a
//! request payload once at issue time and re-derives it during the
//! `apply_event` invariant check; a knob that flips between those two
//! derivations surfaces as `InvariantViolation("in-flight request
//! payload does not match derived state")` in a test that never touched
//! the environment itself.
//!
//! The rule this module enforces: every mutation of such state in the
//! crate's tests happens under ONE process-wide lock, and set/restore is
//! owned by [`EnvScope`] rather than open-coded at the call site. Tests
//! whose derivation is sensitive to it take the same lock (via
//! [`process_globals_test_guard`]), which is what makes the mutation
//! non-concurrent with their reads.
//!
//! Adding a new process-global that derivation reads means routing its
//! test-time mutation through here too — a second lock elsewhere does
//! not serialize against this one, which is precisely how the flake this
//! module fixes came about.
//!
//! This file is shared by the lib and by the `trellis_runtime_cli` bin,
//! which `#[path]`-includes `runtime_cli_observations.rs` and so
//! compiles a second copy of those tests. Both compilations reach it as
//! `crate::EnvScope` / `crate::process_globals_test_guard`.

/// The one lock. Every env mutation in the crate's tests is taken under
/// this; so is every test whose assertions depend on an env-backed knob
/// staying put across a derivation.
static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the env lock without changing anything — for tests that only
/// need mutations to hold still while they read.
///
/// Re-acquires on poison: the lock orders env access, it does not
/// protect an invariant, and [`EnvScope`] restores prior values even
/// when the body panics.
pub(crate) fn process_globals_test_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

/// RAII holder for env-var overrides: acquires the lock, records each
/// variable's prior value the first time that variable is touched, and
/// restores every one of them on drop (including on panic).
///
/// Restoring is the scope's job precisely so a test cannot forget it —
/// a leaked override outlives the lock and changes what every later
/// test in the process derives.
pub(crate) struct EnvScope {
    _guard: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

impl EnvScope {
    /// Take the lock with no overrides installed yet.
    pub(crate) fn lock() -> Self {
        Self {
            _guard: process_globals_test_guard(),
            saved: Vec::new(),
        }
    }

    /// Take the lock and point the kernel disk cache at `root`.
    pub(crate) fn with_kernel_cache_root(root: impl AsRef<std::ffi::OsStr>) -> Self {
        let mut scope = Self::lock();
        scope.set(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, root);
        scope
    }

    /// Set `name` for the lifetime of the scope.
    pub(crate) fn set(&mut self, name: &str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.remember(name);
        // SAFETY: the lock is held, so no other test in this process is
        // mutating or reading the environment concurrently. Production
        // sets these vars once at CLI-dispatch entry, before any thread
        // is spawned.
        unsafe {
            std::env::set_var(name, value);
        }
        self
    }

    /// Remove `name` for the lifetime of the scope.
    pub(crate) fn unset(&mut self, name: &str) -> &mut Self {
        self.remember(name);
        // SAFETY: see `set`.
        unsafe {
            std::env::remove_var(name);
        }
        self
    }

    /// Record the value `name` had on entry, once per variable, so a
    /// loop that rewrites the same variable still restores the original.
    fn remember(&mut self, name: &str) {
        if self.saved.iter().any(|(seen, _)| seen == name) {
            return;
        }
        self.saved.push((name.to_string(), std::env::var_os(name)));
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (name, prior) in self.saved.iter().rev() {
            // SAFETY: the lock is still held — it is released when
            // `_guard` drops, which happens after this.
            unsafe {
                match prior {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

//! Deterministic-check timing ledger.
//!
//! Every invocation of the per-repo `check.py` subprocess (sync-tablet-support,
//! lean-compile-node, materialize-tablet-oleans, etc.) is timed and written
//! here as one JSON line. Purpose: pair with the agent cost-ledger in
//! `.trellis/logs/cost-ledger.jsonl` so the usage report can attribute
//! wall-clock time to specific deterministic checks as well as to agent
//! bursts.
//!
//! `kind` axis:
//!   - "check" — per-repo `check.py` subcommand (default via [`append`])
//!   - "git"   — direct `git -C repo …` subprocesses issued by the kernel
//!               (reset/clean/tag list). Logged via [`append_kind`].
//! New kinds can be added without a schema bump; the reporter groups by
//! `(kind, subcommand)`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

fn ts_unix_seconds_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn ledger_path(repo_path: &Path) -> std::path::PathBuf {
    repo_path
        .join(".trellis")
        .join("logs")
        .join("check-ledger.jsonl")
}

static LOCK: Mutex<()> = Mutex::new(());

/// Record a single check.py invocation. Best-effort: swallows I/O errors so
/// telemetry never masks the underlying subprocess result.
pub fn append(
    repo_path: &Path,
    subcommand: &str,
    duration_s: f64,
    ok: bool,
    stdout_len: usize,
    stderr_len: usize,
) {
    append_kind(
        repo_path, "check", subcommand, duration_s, ok, stdout_len, stderr_len,
    );
}

/// Generalized variant of [`append`] that records an explicit `kind`.
/// Use "check" for the check.py subcommands (via the thin [`append`] wrapper)
/// and "git" for direct `git -C repo …` subprocesses.
pub fn append_kind(
    repo_path: &Path,
    kind: &str,
    subcommand: &str,
    duration_s: f64,
    ok: bool,
    stdout_len: usize,
    stderr_len: usize,
) {
    append_full(
        repo_path, kind, subcommand, duration_s, ok, stdout_len, stderr_len, false,
    );
}

/// Full-detail variant of [`append_kind`] that also records whether the
/// child process was killed by the cgroup OOM-killer (a "wrapper kill",
/// distinct from a legitimate non-zero exit). The `oom` field defaults
/// to false on existing rows; setting it true marks this invocation as
/// having been terminated by `TRELLIS_CHECK_CGROUP`'s `memory.max`.
///
/// When `oom` is true the row is also emitted to stderr with a clearly
/// marked `[trellis-oom]` prefix so the supervisor's tmux pane shows
/// a real-time OOM trail without operators having to grep the ledger.
pub fn append_full(
    repo_path: &Path,
    kind: &str,
    subcommand: &str,
    duration_s: f64,
    ok: bool,
    stdout_len: usize,
    stderr_len: usize,
    oom: bool,
) {
    let path = ledger_path(repo_path);
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let record = json!({
        "ts": ts_unix_seconds_f64(),
        "kind": kind,
        "subcommand": subcommand,
        "duration_seconds": duration_s,
        "ok": ok,
        "oom": oom,
        "stdout_len": stdout_len,
        "stderr_len": stderr_len,
    });
    let Ok(serialized) = serde_json::to_string(&record) else {
        return;
    };
    {
        let _guard = LOCK.lock();
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
            return;
        };
        let _ = writeln!(&mut file, "{}", serialized);
    }
    if oom {
        eprintln!(
            "[trellis-oom] {kind}:{subcommand} killed by cgroup OOM after {:.1}s — see {}",
            duration_s,
            path.display(),
        );
    }
}

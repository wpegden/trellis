//! Disk-persistent JSON cache, keyed by an opaque lookup key + a value-key
//! discriminator.
//!
//! ## Why this exists
//!
//! Four `LazyLock<Mutex<HashMap>>` caches in `tablet_support` and
//! `runtime_cli_observations` short-circuit expensive Lean dispatches when
//! the input closure is content-byte-identical to a prior successful
//! observation. Those caches are *process-local*, but the production call
//! shape is:
//!
//!   supervisor (long-lived) ---bridge---> python wrapper ---Popen---> kernel CLI binary (per-call)
//!
//! The kernel CLI binary lives only for the duration of one
//! `RuntimeCliRequest`. Its in-memory caches die at exit. Cross-cycle (and
//! even within-cycle, across separate kernel CLI invocations) cache hits
//! require disk persistence.
//!
//! This module provides a generic two-function helper plus a small layer
//! that resolves the cache root from an env var; cache call sites layer
//! it under their existing in-memory `LazyLock<Mutex<HashMap>>` to retain
//! the in-process speedup when it applies.
//!
//! ## File layout
//!
//! Each cache namespace gets its own subdirectory under
//! `<runtime_root>/checker-state/kernel-cache/<namespace>/`. Inside each
//! namespace, entries are sharded: SHA-256 of the lookup key, first 2 hex
//! chars become the shard subdir, remaining 62 chars become the filename
//! (with `.json`). Two-level fanout keeps any single directory under ~256
//! shards × ~hundreds of entries per shard, even if a long-running run
//! accumulates tens of thousands of cache entries.
//!
//! ## File contents
//!
//! Each cache file is a JSON document:
//!
//! ```json
//! { "value_key": "<sha256-hex>", "value": <T-as-json> }
//! ```
//!
//! - `value_key` is a discriminator the caller supplies (typically the
//!   closure-content hash). On read, the caller passes the
//!   `expected_value_key`; if it doesn't match, treat as a cache miss
//!   (key collision in the lookup-key hash, content drift, etc.). This
//!   makes the lookup key + value key combination together the
//!   correctness boundary, even when SHA-256 of the lookup key ever
//!   collides (probability ~zero, but the defence is free).
//! - `value` is the cached `T`, JSON-serialised via `serde_json`.
//!
//! ## Atomicity
//!
//! Writes go to a `<filename>.tmp.<random>` sibling, then atomically
//! `rename` into place. Concurrent writers don't corrupt each other's
//! reads; the last writer wins.
//!
//! ## Failure mode
//!
//! Every operation is best-effort and silent on failure: any I/O error
//! during get returns `None` (cache miss), any I/O error during put is
//! swallowed. Callers must always keep their slow path live.
//!
//! ## Pruning
//!
//! Out of scope here — the cache directory grows unbounded. Operators can
//! `rm -rf <runtime_root>/checker-state/kernel-cache/` to clear.

use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Subdirectory under a runtime root where all kernel disk caches live.
const KERNEL_CACHE_SUBDIR: &str = "checker-state/kernel-cache";

/// Env var the supervisor sets so child kernel CLI processes can find
/// the cache root. Set at supervisor `Run` startup and propagated
/// implicitly through every subprocess that inherits its env (the bridge
/// process, the Python wrapper, the Popen child).
///
/// Read+write semantics: the kernel binary writes new entries here, and
/// always consults this directory first on lookup.
pub const KERNEL_CACHE_ROOT_ENV: &str = "TRELLIS_KERNEL_CACHE_ROOT";

/// Optional second cache root, consulted read-only after the writable
/// cache misses. Used by sandboxed worker contexts to read entries the
/// supervisor wrote (the worker's own cache is separate and writable;
/// the supervisor's cache is mounted read-only and surfaced via this env
/// var). The supervisor itself doesn't set this var, so its lookups
/// never escape its own cache — preserving the trust boundary that
/// worker-written entries can never reach the supervisor's reads.
pub const KERNEL_CACHE_READONLY_ROOT_ENV: &str = "TRELLIS_KERNEL_CACHE_READONLY_ROOT";

/// Resolve the per-namespace writable cache directory under
/// `<root>/<KERNEL_CACHE_SUBDIR>/<namespace>/`. `None` when the env var
/// is unset (in which case the disk cache is disabled and call sites
/// fall back to the in-memory `LazyLock<Mutex<HashMap>>` only).
pub fn cache_dir_for_namespace(namespace: &str) -> Option<PathBuf> {
    cache_dir_from_env(KERNEL_CACHE_ROOT_ENV, namespace)
}

/// Resolve the per-namespace read-only cache directory under
/// `<readonly_root>/<KERNEL_CACHE_SUBDIR>/<namespace>/`. `None` when the
/// env var is unset (worker contexts that don't need a fallback simply
/// don't set this var).
pub fn cache_readonly_dir_for_namespace(namespace: &str) -> Option<PathBuf> {
    cache_dir_from_env(KERNEL_CACHE_READONLY_ROOT_ENV, namespace)
}

fn cache_dir_from_env(env_name: &str, namespace: &str) -> Option<PathBuf> {
    let root = std::env::var_os(env_name)?;
    if root.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(root)
            .join(KERNEL_CACHE_SUBDIR)
            .join(namespace),
    )
}

/// All cache directories to try on lookup, in priority order:
///   1. The writable cache (`KERNEL_CACHE_ROOT_ENV`) — entries this
///      process or its peers wrote.
///   2. The read-only fallback (`KERNEL_CACHE_READONLY_ROOT_ENV`) — set
///      in worker contexts to point at the supervisor's cache, which
///      this process can read but not write.
///
/// Returns 0..=2 entries, filtered to those whose env var is set.
/// Callers iterate and return the first hit; writes always go to the
/// writable directory only (`cache_dir_for_namespace`).
pub fn cache_lookup_dirs(namespace: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::with_capacity(2);
    if let Some(d) = cache_dir_for_namespace(namespace) {
        dirs.push(d);
    }
    if let Some(d) = cache_readonly_dir_for_namespace(namespace) {
        dirs.push(d);
    }
    dirs
}

/// Try each cache directory in `dirs` in order, returning the first
/// successful hit (file exists, parses, and `value_key` matches). Used
/// by the multi-tier cache: the writable tier is consulted first, then
/// the read-only fallback (if any). Returns `None` when every directory
/// misses, when no directories are configured, or on any I/O / parse
/// error.
pub fn disk_cache_get_first<T: DeserializeOwned>(
    dirs: &[PathBuf],
    lookup_key: &str,
    expected_value_key: &str,
) -> Option<T> {
    for dir in dirs {
        if let Some(value) = disk_cache_get::<T>(dir, lookup_key, expected_value_key) {
            return Some(value);
        }
    }
    None
}

/// Wrapper for the on-disk JSON record. The `value_key` field is the
/// caller's content-hash discriminator; the `value` is the cached payload.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiskCacheRecord<T> {
    value_key: String,
    value: T,
}

/// Compute the sharded path for a cache entry.
///
/// `<cache_dir>/<first-2-hex>/<remaining-62-hex>.json`.
fn entry_path(cache_dir: &Path, lookup_key: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(lookup_key.as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    // SHA-256 hex is 64 chars; split 2 + 62.
    let (shard, name) = hex.split_at(2);
    cache_dir.join(shard).join(format!("{name}.json"))
}

/// Read a cache entry. Returns `Some(T)` when:
///   - the file exists,
///   - it parses as a `DiskCacheRecord<T>`,
///   - and its `value_key` equals `expected_value_key`.
///
/// Any I/O failure, parse failure, or `value_key` mismatch returns
/// `None`. No panics on malformed contents — operators may zero-out a
/// file, kill -9 mid-write, etc.
pub fn disk_cache_get<T: DeserializeOwned>(
    cache_dir: &Path,
    lookup_key: &str,
    expected_value_key: &str,
) -> Option<T> {
    let path = entry_path(cache_dir, lookup_key);
    let raw = fs::read(&path).ok()?;
    let record: DiskCacheRecord<T> = serde_json::from_slice(&raw).ok()?;
    if record.value_key != expected_value_key {
        return None;
    }
    Some(record.value)
}

/// Write a cache entry. Atomic-on-success, silent-on-failure.
///
/// Writes the JSON body to a unique `<filename>.tmp.<pid>.<nanos>`
/// sibling, then `rename`s into place. The rename is atomic on POSIX
/// (and on tmpfs/ext4/btrfs that we run on), so concurrent writers and
/// readers can never observe a half-written file.
pub fn disk_cache_put<T: Serialize>(
    cache_dir: &Path,
    lookup_key: &str,
    value_key: &str,
    value: &T,
) {
    let final_path = entry_path(cache_dir, lookup_key);
    let Some(shard_dir) = final_path.parent() else {
        return;
    };
    if fs::create_dir_all(shard_dir).is_err() {
        return;
    }
    let record = DiskCacheRecord {
        value_key: value_key.to_string(),
        value,
    };
    let Ok(serialized) = serde_json::to_vec(&record) else {
        return;
    };
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut tmp_path = final_path.clone();
    let final_name = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("entry");
    tmp_path.set_file_name(format!("{final_name}.tmp.{pid}.{nanos}"));
    {
        let Ok(mut file) = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
        else {
            return;
        };
        if file.write_all(&serialized).is_err() {
            // Try to clean up the partial tmp; ignore secondary errors.
            let _ = fs::remove_file(&tmp_path);
            return;
        }
        // Drop closes the file (Linux `rename` doesn't require fsync for
        // visibility; we accept that an unflushed entry may vanish on a
        // crash since the slow path will recover correctly).
    }
    if fs::rename(&tmp_path, &final_path).is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Payload {
        n: u32,
        s: String,
    }

    fn sample_payload(n: u32) -> Payload {
        Payload {
            n,
            s: format!("payload-{n}"),
        }
    }

    #[test]
    fn cold_get_returns_none() {
        let tmp = tempdir().unwrap();
        let result: Option<Payload> = disk_cache_get(tmp.path(), "lookup-1", "value-key-1");
        assert!(result.is_none());
    }

    #[test]
    fn put_then_get_round_trips() {
        let tmp = tempdir().unwrap();
        let payload = sample_payload(42);
        disk_cache_put(tmp.path(), "lookup-1", "value-key-1", &payload);
        let got: Option<Payload> = disk_cache_get(tmp.path(), "lookup-1", "value-key-1");
        assert_eq!(got, Some(payload));
    }

    #[test]
    fn get_with_mismatched_value_key_returns_none() {
        let tmp = tempdir().unwrap();
        let payload = sample_payload(7);
        disk_cache_put(tmp.path(), "lookup-1", "value-key-A", &payload);
        let got: Option<Payload> = disk_cache_get(tmp.path(), "lookup-1", "value-key-B");
        assert!(got.is_none());
    }

    #[test]
    fn corrupt_file_returns_none() {
        let tmp = tempdir().unwrap();
        // Force a corrupt file at the right sharded location.
        let path = entry_path(tmp.path(), "lookup-corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not valid json").unwrap();
        let got: Option<Payload> = disk_cache_get(tmp.path(), "lookup-corrupt", "any-value-key");
        assert!(got.is_none());
    }

    #[test]
    fn empty_file_returns_none() {
        let tmp = tempdir().unwrap();
        let path = entry_path(tmp.path(), "lookup-empty");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"").unwrap();
        let got: Option<Payload> = disk_cache_get(tmp.path(), "lookup-empty", "any-value-key");
        assert!(got.is_none());
    }

    #[test]
    fn concurrent_writes_do_not_corrupt() {
        // N threads write to the same lookup_key with different payloads.
        // Any final read must yield exactly one of the payloads (some
        // writer's value), never a half-written / mixed-bytes record.
        let tmp = tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let lookup = "shared-key";
        let writers = 8usize;
        let iters_per_writer = 50usize;
        let counter = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..writers {
                let dir = dir.clone();
                let counter = &counter;
                scope.spawn(move || {
                    for _ in 0..iters_per_writer {
                        let n = counter.fetch_add(1, Ordering::SeqCst) as u32;
                        let payload = sample_payload(n);
                        let value_key = format!("vk-{n}");
                        disk_cache_put(&dir, lookup, &value_key, &payload);
                    }
                });
            }
        });
        // Final read with any of the legitimate value_keys we wrote
        // either matches (cache hit) or doesn't (caller's expected key
        // didn't win). Either way: no panic, valid JSON, valid Payload
        // shape if read at all.
        let total = (writers * iters_per_writer) as u32;
        let mut hits = 0;
        for n in 0..total {
            let got: Option<Payload> = disk_cache_get(&dir, lookup, &format!("vk-{n}"));
            if let Some(p) = got {
                assert_eq!(p, sample_payload(n));
                hits += 1;
            }
        }
        // Exactly one writer's value can win the rename race (or zero, if
        // the shard dir hadn't been created when the rename happened — but
        // we create_dir_all unconditionally so >=1 should win).
        assert!(
            hits == 1,
            "expected exactly one winner, got {hits} (out of {total} candidates)"
        );
    }

    #[test]
    fn sharded_layout_uses_first_two_hex_chars() {
        let tmp = tempdir().unwrap();
        let path = entry_path(tmp.path(), "deterministic-lookup-key");
        let parent = path.parent().unwrap();
        let shard_name = parent.file_name().and_then(|s| s.to_str()).unwrap();
        assert_eq!(shard_name.len(), 2, "shard dir must be 2 hex chars");
        assert!(shard_name.chars().all(|c| c.is_ascii_hexdigit()));
        let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap();
        assert_eq!(file_stem.len(), 62, "filename stem must be 62 hex chars");
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("json"));
    }

    #[test]
    fn cache_dir_for_namespace_returns_none_when_env_unset() {
        // Best-effort: this test runs in a process that may have the env
        // var set by a prior test. Be defensive.
        let prev = std::env::var_os(KERNEL_CACHE_ROOT_ENV);
        // SAFETY: tests in this module don't rely on the env var being
        // stable between cases; we restore at the end.
        // SAFETY: setting env vars in tests is racy across threads; the
        // operation itself is unsafe in edition 2024+ but stable in 2021.
        unsafe {
            std::env::remove_var(KERNEL_CACHE_ROOT_ENV);
        }
        assert!(cache_dir_for_namespace("any-ns").is_none());
        if let Some(prev) = prev {
            unsafe {
                std::env::set_var(KERNEL_CACHE_ROOT_ENV, prev);
            }
        }
    }

    #[test]
    fn cache_dir_for_namespace_resolves_under_kernel_cache_subdir() {
        let prev = std::env::var_os(KERNEL_CACHE_ROOT_ENV);
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var(KERNEL_CACHE_ROOT_ENV, tmp.path());
        }
        let dir = cache_dir_for_namespace("materialize-oleans").unwrap();
        assert!(dir.starts_with(tmp.path()));
        assert!(dir.ends_with("kernel-cache/materialize-oleans"));
        if let Some(prev) = prev {
            unsafe {
                std::env::set_var(KERNEL_CACHE_ROOT_ENV, prev);
            }
        } else {
            unsafe {
                std::env::remove_var(KERNEL_CACHE_ROOT_ENV);
            }
        }
    }

    #[test]
    fn disk_cache_get_first_falls_back_to_readonly_dir() {
        // Mirrors the worker scenario: the writable cache has nothing for
        // this lookup_key, but a read-only supervisor-side cache does.
        // The helper must consult dirs in order and return the readonly
        // hit. Both dirs are real on-disk tempdirs in this test (we
        // exercise filesystem behaviour, not env-var handling).
        let writable = tempdir().unwrap();
        let readonly = tempdir().unwrap();
        let payload = sample_payload(99);
        // Write the entry only into the readonly dir.
        disk_cache_put(readonly.path(), "shared-key", "vk-A", &payload);

        // Single-dir lookup against the writable dir alone misses, as
        // expected — proves the test setup is right.
        assert!(
            disk_cache_get::<Payload>(writable.path(), "shared-key", "vk-A").is_none(),
            "writable dir was unexpectedly populated"
        );

        // Multi-dir lookup walks the list and finds the readonly hit.
        let dirs = vec![writable.path().to_path_buf(), readonly.path().to_path_buf()];
        let got: Option<Payload> = disk_cache_get_first(&dirs, "shared-key", "vk-A");
        assert_eq!(got, Some(payload));

        // Reverse order: readonly first, writable second (matches the
        // supervisor-only path: only one entry in `cache_lookup_dirs`,
        // exhausted in one read).
        let dirs_single = vec![readonly.path().to_path_buf()];
        let got_single: Option<Payload> = disk_cache_get_first(&dirs_single, "shared-key", "vk-A");
        assert!(got_single.is_some());

        // Empty dir list: cleanly returns None (env-unset path).
        let empty: Vec<PathBuf> = vec![];
        let got_empty: Option<Payload> = disk_cache_get_first(&empty, "shared-key", "vk-A");
        assert!(got_empty.is_none());
    }

    #[test]
    fn disk_cache_get_first_writable_takes_precedence_over_readonly() {
        // Both dirs have the same lookup_key but different payloads.
        // The writable hit should win — this captures the "this process
        // already wrote the answer" path, where consulting readonly
        // would be wasted I/O.
        let writable = tempdir().unwrap();
        let readonly = tempdir().unwrap();
        let writable_payload = sample_payload(1);
        let readonly_payload = sample_payload(2);
        disk_cache_put(writable.path(), "shared-key", "vk-X", &writable_payload);
        disk_cache_put(readonly.path(), "shared-key", "vk-X", &readonly_payload);

        let dirs = vec![writable.path().to_path_buf(), readonly.path().to_path_buf()];
        let got: Option<Payload> = disk_cache_get_first(&dirs, "shared-key", "vk-X");
        assert_eq!(got, Some(writable_payload));
    }

    #[test]
    fn cache_lookup_dirs_returns_writable_then_readonly() {
        // Save and restore both env vars; they're shared global state.
        let prev_w = std::env::var_os(KERNEL_CACHE_ROOT_ENV);
        let prev_r = std::env::var_os(KERNEL_CACHE_READONLY_ROOT_ENV);
        let tmp_w = tempdir().unwrap();
        let tmp_r = tempdir().unwrap();
        unsafe {
            std::env::set_var(KERNEL_CACHE_ROOT_ENV, tmp_w.path());
            std::env::set_var(KERNEL_CACHE_READONLY_ROOT_ENV, tmp_r.path());
        }
        let dirs = cache_lookup_dirs("ns-a");
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].starts_with(tmp_w.path()));
        assert!(dirs[1].starts_with(tmp_r.path()));
        // Now unset just the readonly: only writable shows up.
        unsafe {
            std::env::remove_var(KERNEL_CACHE_READONLY_ROOT_ENV);
        }
        let dirs = cache_lookup_dirs("ns-b");
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].starts_with(tmp_w.path()));
        // Both unset: empty.
        unsafe {
            std::env::remove_var(KERNEL_CACHE_ROOT_ENV);
        }
        assert!(cache_lookup_dirs("ns-c").is_empty());
        // Restore.
        unsafe {
            if let Some(v) = prev_w {
                std::env::set_var(KERNEL_CACHE_ROOT_ENV, v);
            }
            if let Some(v) = prev_r {
                std::env::set_var(KERNEL_CACHE_READONLY_ROOT_ENV, v);
            }
        }
    }
}

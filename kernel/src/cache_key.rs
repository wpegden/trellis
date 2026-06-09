//! Shared content-hash cache-key construction.
//!
//! Consumed by both:
//!   - `tablet_support::materialize_tablet_oleans` (this lib) — gates the
//!     Lean olean-materialization round trip.
//!   - `runtime_cli_observations` (binary side) — gates
//!     `lean-semantic-payloads`, `lean-compile-node`, and `print-axioms`.
//!
//! Both consumers share the same correctness contract:
//!
//!   * **Pure content hashing.** Every input is read from disk and SHA-256'd;
//!     mtimes are never consulted. A `git checkout` of an identical worker
//!     commit produces an identical key, so the cache hits.
//!   * **Conservative on key-construction failure.** Any I/O failure during
//!     key construction returns `None`, which the caller treats as a cache
//!     skip (slow path runs unchanged).
//!   * **Versioned blob.** A `cache_v=` prefix line lets us bump the key
//!     space when the input set evolves; old keys won't collide with new
//!     ones.
//!
//! The blob format is a `\n`-delimited list of `<tag>:<value>` lines, ordered
//! lake → script → preamble → self → deps. Determinism comes from
//! `BTreeSet` iteration on the transitive-import closure.

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Bare name used for the preamble node. Excluded from the per-node import
/// closure (it's already captured by a dedicated `preamble.lean=` line).
const PREAMBLE_NAME: &str = "Preamble";

/// SHA-256 hex digest of a byte slice.
pub fn hash_bytes(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// SHA-256 hex digest of a UTF-8 string.
pub fn hash_text(content: &str) -> String {
    hash_bytes(content.as_bytes())
}

/// Path to the per-repo dispatch script (`.trellis/scripts/check.py`).
/// Used as a cache-key input so a schema change in the script invalidates
/// every memoised observation.
pub fn repo_check_script_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".trellis").join("scripts").join("check.py")
}

/// Extract bare `Tablet.<X>` import names from a Lean source file.
fn extract_tablet_imports(lean_content: &str) -> BTreeSet<String> {
    lean_content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("import Tablet.")
                .map(str::trim)
                .filter(|suffix| !suffix.is_empty())
                .map(str::to_string)
        })
        .collect()
}

fn direct_imports(repo_path: &Path, node_name: &str) -> BTreeSet<String> {
    let lean_path = repo_path.join("Tablet").join(format!("{node_name}.lean"));
    let lean_content = fs::read_to_string(&lean_path).unwrap_or_default();
    extract_tablet_imports(&lean_content)
}

/// Walk the transitive `Tablet.X` import closure of a node, populating
/// `visited`. Mirrors the binary-side `recursive_imports` exactly so the
/// shared cache key matches what the binary previously computed.
pub fn recursive_imports(repo_path: &Path, node_name: &str, visited: &mut BTreeSet<String>) {
    if node_name.is_empty() || node_name == PREAMBLE_NAME || visited.contains(node_name) {
        return;
    }
    visited.insert(node_name.to_string());
    for dep in direct_imports(repo_path, node_name) {
        if dep != PREAMBLE_NAME {
            recursive_imports(repo_path, &dep, visited);
        }
    }
}

/// Compute the per-node Lean-state cache key, or `None` if any input
/// can't be read (treated as a cache skip — slow path takes over).
///
/// Inputs hashed (in this order):
///   - `cache_v=2` version tag.
///   - Lake state files: `lakefile.lean`, `lakefile.toml`,
///     `lake-manifest.json`, `lean-toolchain` — emitted unconditionally
///     so absence vs. presence is differentiated.
///   - `.trellis/scripts/check.py` — required (None on missing).
///   - `Tablet/Preamble.lean` — required (None on missing).
///   - `Tablet/<node>.lean` — required (None on missing).
///   - Every transitive `Tablet/<dep>.lean` — required (None on missing).
///
/// The `cache_v=2` prefix matches the binary-side cache namespace (the
/// existing `lean-semantic-payloads` / `lean-compile-node` / `print-axioms`
/// caches), so a single content-hash check serves all four cache namespaces.
pub fn lean_closure_cache_key(repo_path: &Path, node_name: &str) -> Option<String> {
    let mut closure: BTreeSet<String> = BTreeSet::new();
    recursive_imports(repo_path, node_name, &mut closure);
    closure.remove(node_name);
    closure.remove(PREAMBLE_NAME);

    let tablet_dir = repo_path.join("Tablet");
    let mut blob = String::new();
    blob.push_str("cache_v=2\n");

    for fname in &[
        "lakefile.lean",
        "lakefile.toml",
        "lake-manifest.json",
        "lean-toolchain",
    ] {
        let path = repo_path.join(fname);
        let content = fs::read(&path).unwrap_or_default();
        blob.push_str(&format!("lake:{fname}={}\n", hash_bytes(&content)));
    }

    let script_path = repo_check_script_path(repo_path);
    let script_bytes = fs::read(&script_path).ok()?;
    blob.push_str(&format!("script={}\n", hash_bytes(&script_bytes)));

    let preamble_lean_path = tablet_dir.join("Preamble.lean");
    let preamble_lean_bytes = fs::read(&preamble_lean_path).ok()?;
    blob.push_str(&format!(
        "preamble.lean={}\n",
        hash_bytes(&preamble_lean_bytes)
    ));

    let own_lean_path = tablet_dir.join(format!("{node_name}.lean"));
    let own_lean_bytes = fs::read(&own_lean_path).ok()?;
    blob.push_str(&format!(
        "self={node_name}={}\n",
        hash_bytes(&own_lean_bytes)
    ));

    for dep in &closure {
        let dep_path = tablet_dir.join(format!("{dep}.lean"));
        let dep_bytes = fs::read(&dep_path).ok()?;
        blob.push_str(&format!("dep:{dep}={}\n", hash_bytes(&dep_bytes)));
    }

    Some(hash_text(&blob))
}

/// Compute a multi-node cache key for a set of nodes — a deterministic
/// hash over each node's individual `lean_closure_cache_key`.
///
/// Returns `None` if any node's per-node key fails (cache skip ⇒ slow
/// path). The set order is deterministic because `BTreeSet` sorts; the
/// blob is `\n`-delimited `<node>=<per_node_key>` lines plus the same
/// global lake/script/preamble headers.
///
/// Why a separate function (rather than just hashing per-node keys
/// piecewise): callers need a single key for the multi-node operation
/// (e.g. `materialize-tablet-oleans` accepts a `BTreeSet<NodeId>`),
/// and we want to share lake/script/preamble inputs across nodes
/// without re-reading them N times.
pub fn lean_closure_cache_key_for_nodes(
    repo_path: &Path,
    nodes: &BTreeSet<String>,
) -> Option<String> {
    let mut blob = String::new();
    blob.push_str("multi_cache_v=1\n");

    for fname in &[
        "lakefile.lean",
        "lakefile.toml",
        "lake-manifest.json",
        "lean-toolchain",
    ] {
        let path = repo_path.join(fname);
        let content = fs::read(&path).unwrap_or_default();
        blob.push_str(&format!("lake:{fname}={}\n", hash_bytes(&content)));
    }

    let script_path = repo_check_script_path(repo_path);
    let script_bytes = fs::read(&script_path).ok()?;
    blob.push_str(&format!("script={}\n", hash_bytes(&script_bytes)));

    let preamble_lean_path = repo_path.join("Tablet").join("Preamble.lean");
    let preamble_lean_bytes = fs::read(&preamble_lean_path).ok()?;
    blob.push_str(&format!(
        "preamble.lean={}\n",
        hash_bytes(&preamble_lean_bytes)
    ));

    for node in nodes {
        // Each node contributes its own transitive closure hash.
        let key = lean_closure_cache_key(repo_path, node)?;
        blob.push_str(&format!("node:{node}={key}\n"));
    }

    Some(hash_text(&blob))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn seed_minimal_repo(repo: &Path) {
        write(
            &repo.join(".trellis/scripts/check.py"),
            "#!/usr/bin/env python3\n",
        );
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
    }

    #[test]
    fn key_stable_for_unchanged_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        seed_minimal_repo(&repo);
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\ntheorem A : True := trivial\n",
        );
        let k1 = lean_closure_cache_key(&repo, "A").unwrap();
        let k2 = lean_closure_cache_key(&repo, "A").unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_changes_when_self_lean_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        seed_minimal_repo(&repo);
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\ntheorem A : True := trivial\n",
        );
        let k1 = lean_closure_cache_key(&repo, "A").unwrap();
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\ntheorem A : True := by trivial\n",
        );
        let k2 = lean_closure_cache_key(&repo, "A").unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn multi_node_key_changes_when_one_node_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        seed_minimal_repo(&repo);
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "import Tablet.Preamble\ntheorem B : True := trivial\n",
        );
        let nodes: BTreeSet<String> = ["A".to_string(), "B".to_string()].into_iter().collect();
        let k1 = lean_closure_cache_key_for_nodes(&repo, &nodes).unwrap();

        // Touch B only.
        write(
            &repo.join("Tablet/B.lean"),
            "import Tablet.Preamble\ntheorem B : True := by trivial\n",
        );
        let k2 = lean_closure_cache_key_for_nodes(&repo, &nodes).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn multi_node_key_independent_of_input_order() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        seed_minimal_repo(&repo);
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "import Tablet.Preamble\ntheorem B : True := trivial\n",
        );
        // BTreeSet sorts, so the two constructions give the same set.
        let s1: BTreeSet<String> = ["A".to_string(), "B".to_string()].into_iter().collect();
        let s2: BTreeSet<String> = ["B".to_string(), "A".to_string()].into_iter().collect();
        let k1 = lean_closure_cache_key_for_nodes(&repo, &s1).unwrap();
        let k2 = lean_closure_cache_key_for_nodes(&repo, &s2).unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_is_none_when_self_lean_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        seed_minimal_repo(&repo);
        // No Tablet/A.lean.
        assert!(lean_closure_cache_key(&repo, "A").is_none());
    }

    #[test]
    fn key_is_none_when_check_script_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        // Don't seed the check script.
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\ntheorem A : True := trivial\n",
        );
        assert!(lean_closure_cache_key(&repo, "A").is_none());
    }
}

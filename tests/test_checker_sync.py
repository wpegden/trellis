"""Tests for ``trellis.checker.sync``.

These tests pin the threat-model-mitigation 3 invariants: ``O_NOFOLLOW``
on both source and destination paths, ``st_nlink > 1`` rejection, and
``*.lean`` (plus auto-managed-doc) sync filter.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path

import pytest

from trellis.checker.sync import (
    SEMANTIC_PAYLOAD_CACHE_VERSION,
    SyncError,
    compute_semantic_payload_cache_key,
    load_fingerprint_cache,
    load_semantic_payload,
    semantic_payload_cache_dir,
    store_semantic_payload,
    sync_tablet_dir,
    write_fingerprint_cache,
)


def _seed_worker_repo(root: Path) -> tuple[Path, Path, Path]:
    worker = root / "worker"
    supervisor = root / "supervisor"
    cache_path = root / "fp.json"
    (worker / "Tablet").mkdir(parents=True)
    (supervisor / "Tablet").mkdir(parents=True)
    return worker, supervisor, cache_path


def test_sync_copies_new_file_into_supervisor_tablet(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    (worker / "Tablet" / "LemmaA.lean").write_text("import Tablet.Preamble\n", encoding="utf-8")

    result = sync_tablet_dir(worker, supervisor, cache_path)

    assert result["scanned"] == 1
    assert result["changed"] == ["LemmaA.lean"]
    assert (supervisor / "Tablet" / "LemmaA.lean").read_text(encoding="utf-8") == "import Tablet.Preamble\n"
    assert cache_path.is_file()


def test_sync_short_circuits_on_identical_mtime_and_size(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    src = worker / "Tablet" / "LemmaA.lean"
    src.write_text("hello\n", encoding="utf-8")

    sync_tablet_dir(worker, supervisor, cache_path)

    # Modify supervisor copy so we can detect whether the second sync rewrote it.
    sentinel = "supervisor-sentinel\n"
    (supervisor / "Tablet" / "LemmaA.lean").write_text(sentinel, encoding="utf-8")

    result = sync_tablet_dir(worker, supervisor, cache_path)
    assert result["changed"] == []
    assert (supervisor / "Tablet" / "LemmaA.lean").read_text(encoding="utf-8") == sentinel


def test_sync_skips_copy_when_mtime_changes_but_content_matches(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    src = worker / "Tablet" / "LemmaA.lean"
    src.write_text("immutable-content\n", encoding="utf-8")

    sync_tablet_dir(worker, supervisor, cache_path)
    sentinel = "supervisor-side-sentinel\n"
    (supervisor / "Tablet" / "LemmaA.lean").write_text(sentinel, encoding="utf-8")

    # Touch mtime to a future second so the fast-skip mtime check fails;
    # the SHA tiebreaker should still suppress the rewrite.
    new_ns = (time.time_ns() // 1_000_000_000 + 60) * 1_000_000_000
    os.utime(src, ns=(new_ns, new_ns))

    result = sync_tablet_dir(worker, supervisor, cache_path)
    assert result["changed"] == []
    assert (supervisor / "Tablet" / "LemmaA.lean").read_text(encoding="utf-8") == sentinel


def test_sync_copies_when_content_changes(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    src = worker / "Tablet" / "LemmaA.lean"
    src.write_text("v1\n", encoding="utf-8")

    sync_tablet_dir(worker, supervisor, cache_path)
    src.write_text("v2\n", encoding="utf-8")
    result = sync_tablet_dir(worker, supervisor, cache_path)

    assert result["changed"] == ["LemmaA.lean"]
    assert (supervisor / "Tablet" / "LemmaA.lean").read_text(encoding="utf-8") == "v2\n"


def test_sync_detects_same_size_same_mtime_in_place_edit(tmp_path: Path) -> None:
    """Regression: a same-size content edit that lands within the same mtime
    tick must still be detected. (mtime_ns, size) is not a reliable change
    signal — pinning the mtime back after a same-size rewrite reproduces the
    "fast worker rewrite in the same tick" hazard. The content hash, not the
    metadata, must drive the changed decision.
    """
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    src = worker / "Tablet" / "LemmaA.lean"
    src.write_text("v1\n", encoding="utf-8")
    sync_tablet_dir(worker, supervisor, cache_path)

    # Capture the mtime the first sync recorded, then rewrite to a same-size
    # but different payload and force the mtime back to its prior value so the
    # (mtime_ns, size) fast-path would see "unchanged".
    cached = load_fingerprint_cache(cache_path)["LemmaA.lean"]
    prior_mtime_ns = int(cached["mtime_ns"])
    src.write_text("v2\n", encoding="utf-8")  # same 3 bytes, different content
    os.utime(src, ns=(prior_mtime_ns, prior_mtime_ns))
    assert src.stat().st_mtime_ns == prior_mtime_ns
    assert src.stat().st_size == int(cached["size"])

    result = sync_tablet_dir(worker, supervisor, cache_path)

    assert result["changed"] == ["LemmaA.lean"]
    assert (supervisor / "Tablet" / "LemmaA.lean").read_text(encoding="utf-8") == "v2\n"


def test_sync_unlinks_supervisor_file_when_worker_drops_it(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    src = worker / "Tablet" / "LemmaA.lean"
    src.write_text("dropped\n", encoding="utf-8")
    sync_tablet_dir(worker, supervisor, cache_path)
    assert (supervisor / "Tablet" / "LemmaA.lean").exists()

    src.unlink()
    result = sync_tablet_dir(worker, supervisor, cache_path)

    assert result["removed"] == ["LemmaA.lean"]
    assert not (supervisor / "Tablet" / "LemmaA.lean").exists()


def test_sync_filters_to_lean_tex_and_auto_managed_docs(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    (worker / "Tablet" / "LemmaA.lean").write_text("ok\n", encoding="utf-8")
    (worker / "Tablet" / "LemmaA.tex").write_text("\\begin{theorem}\n", encoding="utf-8")
    (worker / "Tablet" / "INDEX.md").write_text("auto-managed\n", encoding="utf-8")
    (worker / "Tablet" / "stray.txt").write_text("nope\n", encoding="utf-8")
    (worker / "Tablet" / "rogue.tmp.42").write_text("bait\n", encoding="utf-8")

    sync_tablet_dir(worker, supervisor, cache_path)

    assert (supervisor / "Tablet" / "LemmaA.lean").exists()
    assert (supervisor / "Tablet" / "LemmaA.tex").exists()
    assert (supervisor / "Tablet" / "INDEX.md").exists()
    assert not (supervisor / "Tablet" / "stray.txt").exists()
    assert not (supervisor / "Tablet" / "rogue.tmp.42").exists()


def test_sync_rejects_symlink_source(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    target = tmp_path / "secret.txt"
    target.write_text("not-leakable", encoding="utf-8")
    link = worker / "Tablet" / "LemmaA.lean"
    os.symlink(target, link)

    result = sync_tablet_dir(worker, supervisor, cache_path)
    assert "LemmaA.lean" in result["rejected"]
    assert not (supervisor / "Tablet" / "LemmaA.lean").exists()


def test_sync_rejects_hardlinked_source(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    canonical = tmp_path / "elsewhere.txt"
    canonical.write_text("shared inode\n", encoding="utf-8")
    src = worker / "Tablet" / "LemmaA.lean"
    os.link(canonical, src)

    result = sync_tablet_dir(worker, supervisor, cache_path)
    assert "LemmaA.lean" in result["rejected"]
    assert not (supervisor / "Tablet" / "LemmaA.lean").exists()


def test_sync_does_not_descend_directory_symlinks(tmp_path: Path) -> None:
    worker, supervisor, cache_path = _seed_worker_repo(tmp_path)
    # Sneak a symlink to the homedir into Tablet/.
    home = tmp_path / "fake-home"
    home.mkdir()
    (home / "secret.txt").write_text("private\n", encoding="utf-8")
    os.symlink(home, worker / "Tablet" / "evil")

    result = sync_tablet_dir(worker, supervisor, cache_path)
    # The walk does not follow the directory symlink, so its contents
    # are never enumerated; the symlink itself is filtered as non-regular.
    assert not (supervisor / "Tablet" / "evil").exists()
    assert not (supervisor / "Tablet" / "evil" / "secret.txt").exists()
    assert result["scanned"] == 0


def test_load_fingerprint_cache_returns_empty_on_missing_file(tmp_path: Path) -> None:
    assert load_fingerprint_cache(tmp_path / "missing.json") == {}


def test_load_fingerprint_cache_returns_empty_on_corrupt_file(tmp_path: Path) -> None:
    p = tmp_path / "fp.json"
    p.write_text("{not json", encoding="utf-8")
    assert load_fingerprint_cache(p) == {}


def test_write_fingerprint_cache_atomic_replace(tmp_path: Path) -> None:
    p = tmp_path / "fp.json"
    write_fingerprint_cache(p, {"a.lean": {"mtime_ns": 1, "size": 2, "sha256": "x"}})
    assert json.loads(p.read_text(encoding="utf-8")) == {
        "a.lean": {"mtime_ns": 1, "size": 2, "sha256": "x"}
    }
    write_fingerprint_cache(p, {"b.lean": {"mtime_ns": 3, "size": 4, "sha256": "y"}})
    assert "a.lean" not in json.loads(p.read_text(encoding="utf-8"))


# --------------------------------------------------------------------------
# Semantic-payload cache helpers
# --------------------------------------------------------------------------


def _olean_path(repo: Path, node_name: str) -> Path:
    """Mirror ``observations._tablet_olean_path``: the path the cache-key
    derivation will sha256 to lock the compiled artifact's content into
    the key alongside the source ``.lean``."""
    return repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node_name}.olean"


def _write_olean_stub(repo: Path, node_name: str, content: bytes) -> None:
    """Write a stub ``.olean`` blob so cache-key derivation can sha256 it.

    Cache-key derivation does not require a real Lean-format olean — it
    only sha256's the bytes. Tests therefore stub a plain bytes blob at
    the expected on-disk path.
    """
    p = _olean_path(repo, node_name)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(content)


def _seed_supervisor_repo_with_chain(
    root: Path,
    *,
    leaf_imports: list[str] | None = None,
    seed_oleans: bool = True,
) -> Path:
    """Create a minimal supervisor repo with a Tablet/ chain.

    Returns the supervisor repo path. Layout:
        repo/Tablet/Leaf.lean                                          (imports `leaf_imports`)
        repo/Tablet/Dep1.lean                                          (no imports)
        repo/Tablet/Dep2.lean                                          (no imports)
        repo/.lake/build/lib/lean/Tablet/{Leaf,Dep1,Dep2}.olean        (stubs, if seed_oleans)
    The default (`leaf_imports=None`) wires Leaf to Dep1+Dep2. When
    ``seed_oleans=False``, the test wants to exercise the
    "expected olean missing" branch and the stub file is omitted.
    """
    repo = root / "supervisor"
    (repo / "Tablet").mkdir(parents=True)
    if leaf_imports is None:
        leaf_imports = ["Dep1", "Dep2"]
    (repo / "Tablet" / "Dep1.lean").write_text("-- dep1\n", encoding="utf-8")
    (repo / "Tablet" / "Dep2.lean").write_text("-- dep2\n", encoding="utf-8")
    leaf_text = "".join(f"import Tablet.{dep}\n" for dep in leaf_imports) + "-- leaf\n"
    (repo / "Tablet" / "Leaf.lean").write_text(leaf_text, encoding="utf-8")
    if seed_oleans:
        _write_olean_stub(repo, "Leaf", b"olean-leaf-v0")
        _write_olean_stub(repo, "Dep1", b"olean-dep1-v0")
        _write_olean_stub(repo, "Dep2", b"olean-dep2-v0")
    return repo


def _make_sync_cache(**files: str) -> dict[str, dict[str, object]]:
    """Build a sync-fingerprint-cache-shaped dict from filename -> sha256."""
    return {
        name: {"mtime_ns": 1, "size": 1, "sha256": sha}
        for name, sha in files.items()
    }


def test_compute_cache_key_is_deterministic(tmp_path: Path) -> None:
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    sync_cache = _make_sync_cache(**{
        "Leaf.lean": "leafsha",
        "Dep1.lean": "dep1sha",
        "Dep2.lean": "dep2sha",
    })
    k1 = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    k2 = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert k1 is not None
    assert k1 == k2
    assert len(k1) == 64  # sha256 hex digest


def test_compute_cache_key_changes_when_self_changes(tmp_path: Path) -> None:
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    base = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    bumped = _make_sync_cache(
        **{"Leaf.lean": "BUMPED", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    k_base = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        base,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    k_bump = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        bumped,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert k_base != k_bump


def test_compute_cache_key_changes_when_dep_changes(tmp_path: Path) -> None:
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    base = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    bumped = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "DEP1NEW", "Dep2.lean": "dep2sha"}
    )
    k_base = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        base,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    k_bump = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        bumped,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert k_base != k_bump


def test_compute_cache_key_changes_when_toolchain_changes(tmp_path: Path) -> None:
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    sync_cache = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    k_a = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tc-A",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    k_b = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tc-B",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert k_a != k_b


def test_compute_cache_key_changes_when_script_changes(tmp_path: Path) -> None:
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    sync_cache = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    k_a = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "script-A",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    k_b = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "script-B",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert k_a != k_b


def test_compute_cache_key_changes_when_manifest_changes(tmp_path: Path) -> None:
    """``lake update`` rewrites lake-manifest.json without touching
    lean-toolchain. The cache key must therefore include the manifest
    sha — otherwise a mathlib bump that lands a new olean set would
    silently serve stale fingerprints under the same key."""
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    sync_cache = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    k_a = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifest-A",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    k_b = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifest-B",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert k_a is not None and k_b is not None
    assert k_a != k_b


def test_compute_cache_key_changes_when_olean_changes(tmp_path: Path) -> None:
    """The fingerprint script reads ``.olean`` files, never re-parses
    sources, so a stale olean against a fresh ``.lean`` would otherwise
    produce a quietly-wrong cached payload. Hashing every closure
    member's olean bytes into the key makes that scenario a hard miss
    instead."""
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    sync_cache = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    k_before = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    # Mutate Dep1's olean while leaving its .lean and the sync_cache
    # entry untouched. Without olean hashing the key would be unchanged.
    _write_olean_stub(repo, "Dep1", b"olean-dep1-v1-RECOMPILED")
    k_after = compute_semantic_payload_cache_key(
        repo,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert k_before is not None and k_after is not None
    assert k_before != k_after


def test_compute_cache_key_returns_none_when_olean_missing(tmp_path: Path) -> None:
    """When a closure member's ``.olean`` doesn't exist on disk we
    return ``None`` so the lean call runs anyway and builds the missing
    artifact. Matches the "missing from sync_cache → None" contract:
    a hard miss, never a silent stale-cache hit."""
    repo = _seed_supervisor_repo_with_chain(tmp_path, seed_oleans=False)
    # Hand-seed only Leaf's olean so we exercise the dep-olean-missing
    # branch specifically.
    _write_olean_stub(repo, "Leaf", b"olean-leaf-only")
    sync_cache = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    assert (
        compute_semantic_payload_cache_key(
            repo,
            "Leaf",
            sync_cache,
            "scriptsha",
            "tcsha",
            "manifestsha",
            SEMANTIC_PAYLOAD_CACHE_VERSION,
        )
        is None
    )

    # And: even if every dep olean is present, missing the self-olean
    # also forces None (covers the symmetric branch).
    repo2 = _seed_supervisor_repo_with_chain(tmp_path / "alt", seed_oleans=False)
    _write_olean_stub(repo2, "Dep1", b"olean-dep1")
    _write_olean_stub(repo2, "Dep2", b"olean-dep2")
    assert (
        compute_semantic_payload_cache_key(
            repo2,
            "Leaf",
            sync_cache,
            "scriptsha",
            "tcsha",
            "manifestsha",
            SEMANTIC_PAYLOAD_CACHE_VERSION,
        )
        is None
    )


def test_compute_cache_key_returns_none_when_self_missing_from_sync_cache(
    tmp_path: Path,
) -> None:
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    # Self entry absent: should return None (cache skip).
    sync_cache = _make_sync_cache(
        **{"Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    assert (
        compute_semantic_payload_cache_key(
            repo,
            "Leaf",
            sync_cache,
            "scriptsha",
            "tcsha",
            "manifestsha",
            SEMANTIC_PAYLOAD_CACHE_VERSION,
        )
        is None
    )


def test_compute_cache_key_returns_none_when_dep_missing_from_sync_cache(
    tmp_path: Path,
) -> None:
    repo = _seed_supervisor_repo_with_chain(tmp_path)
    sync_cache = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha"}  # Dep2 missing
    )
    assert (
        compute_semantic_payload_cache_key(
            repo,
            "Leaf",
            sync_cache,
            "scriptsha",
            "tcsha",
            "manifestsha",
            SEMANTIC_PAYLOAD_CACHE_VERSION,
        )
        is None
    )


def test_compute_cache_key_independent_of_dep_order(tmp_path: Path) -> None:
    """The blob sorts deps by name, so changing the order in which the
    closure walk visits them must not change the key."""
    repo_a = _seed_supervisor_repo_with_chain(
        tmp_path / "a", leaf_imports=["Dep1", "Dep2"]
    )
    repo_b = _seed_supervisor_repo_with_chain(
        tmp_path / "b", leaf_imports=["Dep2", "Dep1"]
    )
    sync_cache = _make_sync_cache(
        **{"Leaf.lean": "leafsha", "Dep1.lean": "dep1sha", "Dep2.lean": "dep2sha"}
    )
    k_a = compute_semantic_payload_cache_key(
        repo_a,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    k_b = compute_semantic_payload_cache_key(
        repo_b,
        "Leaf",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert k_a == k_b


def test_compute_cache_key_handles_no_deps(tmp_path: Path) -> None:
    repo = tmp_path / "supervisor"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "Standalone.lean").write_text("-- nothing\n", encoding="utf-8")
    _write_olean_stub(repo, "Standalone", b"olean-standalone")
    sync_cache = _make_sync_cache(**{"Standalone.lean": "stsha"})
    k = compute_semantic_payload_cache_key(
        repo,
        "Standalone",
        sync_cache,
        "scriptsha",
        "tcsha",
        "manifestsha",
        SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    assert isinstance(k, str)
    assert len(k) == 64


def test_store_then_load_round_trip(tmp_path: Path) -> None:
    cache_dir = semantic_payload_cache_dir(tmp_path)
    key = "a" * 64
    store_semantic_payload(
        cache_dir,
        key,
        node_name="Leaf",
        ok=True,
        payload="FP\tLeaf\tdigest-bytes",
        error="",
        cache_version=SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    loaded = load_semantic_payload(
        cache_dir, key, expected_version=SEMANTIC_PAYLOAD_CACHE_VERSION
    )
    assert loaded is not None
    assert loaded["ok"] is True
    assert loaded["payload"] == "FP\tLeaf\tdigest-bytes"
    assert loaded["node_name"] == "Leaf"
    assert loaded["key_blob_sha256"] == key
    assert loaded["cache_version"] == SEMANTIC_PAYLOAD_CACHE_VERSION


def test_load_returns_none_on_missing_sidecar(tmp_path: Path) -> None:
    cache_dir = semantic_payload_cache_dir(tmp_path)
    key = "b" * 64
    assert (
        load_semantic_payload(
            cache_dir, key, expected_version=SEMANTIC_PAYLOAD_CACHE_VERSION
        )
        is None
    )


def test_load_returns_none_on_version_mismatch(tmp_path: Path) -> None:
    cache_dir = semantic_payload_cache_dir(tmp_path)
    key = "c" * 64
    store_semantic_payload(
        cache_dir,
        key,
        node_name="Leaf",
        ok=True,
        payload="anything",
        error="",
        cache_version=SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    # Caller asks for a different schema version -> reject the sidecar.
    assert (
        load_semantic_payload(
            cache_dir,
            key,
            expected_version=SEMANTIC_PAYLOAD_CACHE_VERSION + 1,
        )
        is None
    )


def test_load_returns_none_on_corrupt_json(tmp_path: Path) -> None:
    cache_dir = semantic_payload_cache_dir(tmp_path)
    cache_dir.mkdir(parents=True)
    key = "d" * 64
    sidecar = cache_dir / f"{key}.json"
    sidecar.write_text("{not json", encoding="utf-8")
    assert (
        load_semantic_payload(
            cache_dir, key, expected_version=SEMANTIC_PAYLOAD_CACHE_VERSION
        )
        is None
    )


def test_load_returns_none_on_basename_mismatch(tmp_path: Path) -> None:
    """Sidecar's basename must equal its self-attested key.

    Pins the corruption-detection invariant: a file written under one key
    but renamed to another path must be rejected so a partial-write or
    manual rename can't poison the cache.
    """
    cache_dir = semantic_payload_cache_dir(tmp_path)
    real_key = "e" * 64
    other_key = "f" * 64
    store_semantic_payload(
        cache_dir,
        real_key,
        node_name="Leaf",
        ok=True,
        payload="payload",
        error="",
        cache_version=SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    # Move the sidecar to a different basename without rewriting its
    # internal `key_blob_sha256` field.
    src = cache_dir / f"{real_key}.json"
    dst = cache_dir / f"{other_key}.json"
    src.rename(dst)
    assert (
        load_semantic_payload(
            cache_dir, other_key, expected_version=SEMANTIC_PAYLOAD_CACHE_VERSION
        )
        is None
    )


def test_store_atomicity(tmp_path: Path) -> None:
    """Two consecutive stores of the same key produce a single, valid file
    (no temp leftovers, no half-written state)."""
    cache_dir = semantic_payload_cache_dir(tmp_path)
    key = "1" * 64
    store_semantic_payload(
        cache_dir,
        key,
        node_name="Leaf",
        ok=True,
        payload="first",
        error="",
        cache_version=SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    store_semantic_payload(
        cache_dir,
        key,
        node_name="Leaf",
        ok=True,
        payload="second",
        error="",
        cache_version=SEMANTIC_PAYLOAD_CACHE_VERSION,
    )
    files = sorted(p.name for p in cache_dir.iterdir())
    # Exactly one sidecar; no .tmp.* leftovers.
    assert files == [f"{key}.json"]
    loaded = load_semantic_payload(
        cache_dir, key, expected_version=SEMANTIC_PAYLOAD_CACHE_VERSION
    )
    assert loaded is not None
    assert loaded["payload"] == "second"

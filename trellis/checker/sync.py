"""Fingerprint-cached worker → supervisor Tablet sync.

Implements the §3 algorithm of the unified-checker design plan, with the
threat-model mitigation 3 hardening: ``O_NOFOLLOW`` open on the source,
``os.fstat``-based metadata (no symlink follow), reject ``st_nlink > 1``
(refuse hardlinked sources that could let the worker mutate the same
inode under a different name in a kernel-trusted location), and filter
sync to ``*.lean`` plus the auto-managed Tablet docs.

The fingerprint cache persists across requests at
``<runtime_root>/checker-state/sync-fingerprints.json``. The fast-skip
path (mtime_ns + size match) avoids any I/O on identical source trees;
the SHA-256 tiebreaker covers the cases where mtime preservation through
``shutil.copy2`` (per ``supervisor_workspace.propagate_tablet_back_to_worker``
at supervisor_workspace.py:97-150) could have masked content drift.
"""

from __future__ import annotations

import errno
import hashlib
import json
import logging
import os
import stat
import tempfile
import time
from pathlib import Path
from typing import Any, Dict, List, Mapping, Optional


_LOGGER = logging.getLogger("trellis.checker.sync")


# Semantic-payload cache schema. Bumping this constant invalidates every
# previously written sidecar (load_semantic_payload returns ``None`` on
# version mismatch); the version is also baked into the key blob below
# (``v={cache_version}\n``) so on-disk filenames change too. Bump on any
# change to ``scripts/lean_semantic_fingerprint.lean``'s output format.
# v=4 (2026-04-29): switched fingerprint payload from textual
# `serializeExpr` to pointer-memoized structural hash (`fingerprintExprs`)
# — fixes exponential blow-up on nested-`Classical.choose` defs and
# matches the prior text serializer's mdata-stripped, binder-name-blind
# precision.
SEMANTIC_PAYLOAD_CACHE_VERSION = 4
SEMANTIC_PAYLOAD_DIRNAME = "semantic-payloads"


# ``print_axioms`` cache schema. Mirrors the semantic-payload cache exactly
# (same key derivation surface, same atomic sidecar storage) but lives in a
# separate namespace so the two caches age independently. Bump this constant
# whenever the persisted record shape changes; ``load_print_axioms`` will
# treat older sidecars as cold misses on the next request.
PRINT_AXIOMS_CACHE_VERSION = 1
PRINT_AXIOMS_DIRNAME = "print-axioms-cache"


# ``local_closure_axioms`` cache schema (LOCAL_CLOSURE_IMPL_PLAN.md §5.5 deferred
# optimization). The probe's output depends on the same closure-walked surface
# as print_axioms — node source + transitive olean closure + toolchain pin +
# lake manifest — plus the local-closure Lean script's own sha (used as
# `script_sha256` in `compute_semantic_payload_cache_key`). Bump on any
# stored-schema change.
LOCAL_CLOSURE_AXIOMS_CACHE_VERSION = 1
LOCAL_CLOSURE_AXIOMS_DIRNAME = "local-closure-axioms-cache"


# Tablet sync surface: source ``.lean`` and the three auto-managed docs that
# `supervisor_workspace.propagate_tablet_back_to_worker` already mirrors back
# (kept on the allowlist so tablet introspection works under the unified
# checker; INDEX/README/header are kernel-regenerated, but worker-side reads
# need them on the supervisor too).
_AUTO_MANAGED_NAMES = frozenset({"INDEX.md", "README.md", "header.tex"})


def _is_synced_relpath(rel: Path) -> bool:
    """Filter sync to ``*.lean`` (any depth) and the auto-managed docs at the
    Tablet root. Mirror ``*.tex`` files too — workers author them and reviewers
    expect them to round-trip — but exclude ``*.tmp.*`` sync-bait (caught by
    the suffix check) and any dotfiles. Mitigation 5 of the threat model:
    drop the ``.tmp.``-substring class of sync bait by enforcing positive
    suffix matching here rather than negative-prefix filtering at the walk."""
    name = rel.name
    if name.startswith("."):
        return False
    if rel.suffix == ".lean":
        return True
    if rel.suffix == ".tex":
        return True
    if len(rel.parts) == 1 and name in _AUTO_MANAGED_NAMES:
        return True
    return False


class SyncError(Exception):
    """Sync-time failure (symlink rejection, hardlink rejection, I/O).

    The dispatcher converts this into an ``rpc_error.kind="sync_failed"``
    envelope so the caller sees a protocol-level failure rather than a
    stale supervisor workspace.
    """


def load_fingerprint_cache(cache_path: Path) -> Dict[str, Dict[str, Any]]:
    """Read the sidecar fingerprint cache, returning ``{}`` on any error.

    The cache is authoritative for fast-skip but never load-bearing for
    correctness — worst case a missing/corrupt cache forces a full SHA
    sweep on the next request, ~4 ms over a 150-node tablet.
    """
    if not cache_path.is_file():
        return {}
    try:
        text = cache_path.read_text(encoding="utf-8")
    except OSError:
        return {}
    try:
        decoded = json.loads(text)
    except json.JSONDecodeError:
        return {}
    if not isinstance(decoded, dict):
        return {}
    out: Dict[str, Dict[str, Any]] = {}
    for key, value in decoded.items():
        if not isinstance(key, str) or not isinstance(value, dict):
            continue
        out[key] = {
            "mtime_ns": int(value.get("mtime_ns", 0) or 0),
            "size": int(value.get("size", 0) or 0),
            "sha256": str(value.get("sha256", "") or ""),
        }
    return out


def write_fingerprint_cache(
    cache_path: Path, payload: Dict[str, Dict[str, Any]]
) -> None:
    """Atomically replace the fingerprint cache via temp+rename."""
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(
        prefix=cache_path.name + ".tmp.",
        dir=str(cache_path.parent),
    )
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(payload, handle, sort_keys=True, separators=(",", ":"))
            handle.write("\n")
        os.replace(tmp_name, cache_path)
    except Exception:
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise


def _safe_open_source(src: Path) -> int:
    """Open ``src`` with ``O_NOFOLLOW | O_CLOEXEC | O_RDONLY``.

    Returns the file descriptor on success. Raises ``SyncError`` if the
    source is a symlink (``ELOOP``), refuses to open as regular file, or
    yields any other OSError.
    """
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW
    try:
        return os.open(str(src), flags)
    except OSError as exc:
        if exc.errno == errno.ELOOP:
            raise SyncError(f"refusing to follow symlink at {src}")
        raise SyncError(f"could not open source {src}: {exc}") from exc


def _read_full_fd(fd: int) -> bytes:
    chunks: list[bytes] = []
    while True:
        chunk = os.read(fd, 1 << 16)
        if not chunk:
            break
        chunks.append(chunk)
    return b"".join(chunks)


def _atomic_write(dst: Path, data: bytes) -> None:
    """Write ``data`` to ``dst`` via temp+rename. The temp file is created
    in ``dst.parent`` so the rename is atomic (same filesystem). The temp
    file open uses ``O_EXCL`` (via ``tempfile.mkstemp``) so a pre-existing
    symlink at the temp path causes ``EEXIST``; ``mkstemp`` retries with a
    fresh randomized name, guaranteeing we never write through a
    pre-existing symlink at ``dst.parent/<tmp>``."""
    dst.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(
        prefix=dst.name + ".tmp.", dir=str(dst.parent)
    )
    try:
        try:
            os.write(fd, data)
        finally:
            os.close(fd)
        os.replace(tmp_name, dst)
    except Exception:
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise


def _safe_unlink(path: Path) -> None:
    """Unlink ``path`` if it exists, ignoring ENOENT."""
    try:
        os.unlink(str(path))
    except FileNotFoundError:
        pass
    except OSError:
        # Reraise non-ENOENT — a stuck file in supervisor's Tablet is a
        # real sync failure the caller should see.
        raise


def sync_tablet_dir(
    worker_repo: Path,
    supervisor_repo: Path,
    fp_cache_path: Path,
) -> Dict[str, Any]:
    """Mirror ``worker_repo/Tablet`` → ``supervisor_repo/Tablet`` using the
    persistent fingerprint cache at ``fp_cache_path``.

    Returns ``{"changed": [...], "removed": [...], "scanned": N,
    "rejected": [...]}``. ``rejected`` contains paths that were silently
    skipped (symlinks, ``st_nlink > 1`` sources). ``changed`` contains
    paths whose content was copied. ``removed`` lists files that no
    longer exist in the worker tablet and were unlinked supervisor-side.

    Raises ``SyncError`` if a permission or I/O failure prevents the sync
    from completing — the caller should fail the RPC rather than hand a
    stale workspace to lake.
    """
    worker_tablet = (worker_repo / "Tablet").resolve()
    super_tablet = (supervisor_repo / "Tablet").resolve()
    super_tablet.mkdir(parents=True, exist_ok=True)

    prior_cache = load_fingerprint_cache(fp_cache_path)
    new_cache: Dict[str, Dict[str, Any]] = {}
    changed: List[str] = []
    removed: List[str] = []
    rejected: List[str] = []
    scanned = 0
    seen_rels: set[str] = set()

    if worker_tablet.is_dir():
        # Walk with followlinks=False so directory-symlinks cannot point
        # at ${TRELLIS_ROOT:-/path/to/trellis} and trick us into copying the whole homedir.
        for current_root, dirnames, filenames in os.walk(
            str(worker_tablet), followlinks=False
        ):
            current_path = Path(current_root)
            try:
                rel_dir = current_path.relative_to(worker_tablet)
            except ValueError:
                continue
            for name in filenames:
                rel = rel_dir / name if rel_dir != Path(".") else Path(name)
                rel_str = str(rel)
                src = worker_tablet / rel
                scanned += 1

                # 1. Reject symlinks (lstat'd before opening to short-circuit).
                try:
                    lst = os.lstat(str(src))
                except OSError as exc:
                    raise SyncError(f"lstat({src}) failed: {exc}") from exc
                if stat.S_ISLNK(lst.st_mode):
                    rejected.append(rel_str)
                    continue
                if not stat.S_ISREG(lst.st_mode):
                    rejected.append(rel_str)
                    continue
                if not _is_synced_relpath(rel):
                    continue

                # Open with O_NOFOLLOW for the actual read; the lstat above
                # is advisory only — the open is the security-load-bearing
                # syscall.
                src_fd = _safe_open_source(src)
                try:
                    fst = os.fstat(src_fd)
                    if fst.st_nlink > 1:
                        rejected.append(rel_str)
                        continue
                    if not stat.S_ISREG(fst.st_mode):
                        rejected.append(rel_str)
                        continue

                    seen_rels.add(rel_str)
                    prior = prior_cache.get(rel_str)

                    # Always read + hash: (mtime_ns, size) is NOT a reliable
                    # change signal for this source. A same-size in-place edit
                    # that lands within the same mtime tick (e.g. "v1\n" →
                    # "v2\n", both 3 bytes, same nanosecond from a fast worker
                    # rewrite, or mtime pinned through shutil.copy2) leaves the
                    # (mtime_ns, size) pair unchanged while the content differs.
                    # Trusting the cached digest on an (mtime, size) match would
                    # then silently miss the edit and hand lake a stale tablet.
                    # The Tablet dir is small (~150 nodes, ~4 ms full SHA sweep),
                    # so we hash every scanned source and let the digest — not
                    # the metadata — decide changed-vs-unchanged.
                    data = _read_full_fd(src_fd)
                    digest = hashlib.sha256(data).hexdigest()

                    if prior and prior.get("sha256") == digest:
                        # Content matches the cached digest — no rewrite needed.
                        # Refresh the metadata so the cache entry tracks the
                        # current (mtime_ns, size) even when only mtime drifted.
                        new_cache[rel_str] = {
                            "mtime_ns": int(fst.st_mtime_ns),
                            "size": int(fst.st_size),
                            "sha256": digest,
                        }
                        continue

                    dst = super_tablet / rel
                    _atomic_write(dst, data)
                    new_cache[rel_str] = {
                        "mtime_ns": int(fst.st_mtime_ns),
                        "size": int(fst.st_size),
                        "sha256": digest,
                    }
                    changed.append(rel_str)
                finally:
                    try:
                        os.close(src_fd)
                    except OSError:
                        pass

    # Removal sweep: anything in prior_cache not seen this round is gone
    # from the worker tree; mirror the deletion supervisor-side. Skip
    # sources we rejected this run — keeping the destination in place
    # avoids "delete a file because the source became a symlink" surprises.
    for rel_str in list(prior_cache):
        if rel_str in seen_rels:
            continue
        rel = Path(rel_str)
        try:
            _safe_unlink(super_tablet / rel)
        except OSError as exc:
            raise SyncError(f"unlink({super_tablet / rel}) failed: {exc}") from exc
        removed.append(rel_str)

    # Persist cache atomically.
    write_fingerprint_cache(fp_cache_path, new_cache)

    return {
        "changed": changed,
        "removed": removed,
        "rejected": rejected,
        "scanned": scanned,
    }


def semantic_payload_cache_dir(state_dir: Path) -> Path:
    """Return the directory holding per-key semantic-payload sidecar files.

    Sibling to ``sync-fingerprints.json`` under ``checker-state/`` so it
    shares the same lifecycle as the fingerprint cache: GC happens when
    the runtime root is wiped, never independently of the rest of the
    checker state.
    """
    return state_dir / SEMANTIC_PAYLOAD_DIRNAME


def _sha256_of_file_or_none(path: Path) -> Optional[str]:
    """Return the file's SHA-256 hex digest, or ``None`` if the file does
    not exist. Any other I/O error is propagated to the caller.

    Used by ``compute_semantic_payload_cache_key`` to hash each closure
    member's compiled ``.olean``. Distinguishes "file missing" (forces a
    cache skip — the lean call needs to run anyway to build the olean)
    from "transient I/O failure" (let it propagate so we don't silently
    serve a key derived from incomplete inputs).
    """
    try:
        with open(path, "rb") as handle:
            digest = hashlib.sha256()
            for chunk in iter(lambda: handle.read(1 << 16), b""):
                digest.update(chunk)
            return digest.hexdigest()
    except FileNotFoundError:
        return None


def compute_semantic_payload_cache_key(
    supervisor_repo: Path,
    node_name: str,
    sync_cache: Mapping[str, Mapping[str, Any]],
    script_sha256: str,
    toolchain_sha256: str,
    manifest_sha256: str,
    cache_version: int,
) -> Optional[str]:
    """Derive the SHA-256 cache key for a node's semantic payload.

    Returns ``None`` (cache skip) if any closure-walk dependency is
    missing from ``sync_cache``, or if any expected ``.olean`` is absent
    on disk — the caller falls through to the live observation path.
    Reading the closure off the *supervisor* repo matches the on-disk
    state lake will see when invoked, so the key captures exactly the
    inputs that affect the lean run.

    The key blob is line-based, deterministic, and hashes:

    - the cache schema literal (``v=<cache_version>``);
    - the fingerprint script's sha256 (changes when the script does);
    - the lean-toolchain pin's sha256 (changes on toolchain bumps);
    - the lake-manifest sha256 (changes after ``lake update`` even when
      the toolchain is unchanged — pins the mathlib rev independently
      from ``lean-toolchain``);
    - the node's own ``Tablet/<node>.lean`` sha256;
    - every transitive Tablet import's ``.lean`` sha256, sorted by dep name;
    - the node's own ``Tablet/<node>.olean`` sha256;
    - every transitive Tablet import's ``.olean`` sha256, sorted by dep
      name (covers the "edited .lean but stale .olean" trap — the
      fingerprint script's ``importModules`` reads oleans, never sources).

    Returning ``None`` when any expected olean is missing matches the
    "missing from sync_cache → None" contract: a hard miss, because the
    lean call needs to happen anyway to build the missing olean.
    """
    # Local import keeps observations.py the canonical home for closure
    # walking; sync.py stays a leaf module that can be imported from any
    # observation site without circularity.
    from trellis.atomic_actions.observations import (
        _tablet_olean_path,
        materialization_order,
    )

    closure = materialization_order(supervisor_repo, [node_name])
    if node_name not in closure:
        return None

    self_entry = sync_cache.get(f"{node_name}.lean")
    if not self_entry or not self_entry.get("sha256"):
        return None

    self_olean_sha = _sha256_of_file_or_none(
        _tablet_olean_path(supervisor_repo, node_name)
    )
    if self_olean_sha is None:
        return None

    deps_sorted: list[tuple[str, str]] = []
    olean_deps_sorted: list[tuple[str, str]] = []
    for dep in closure:
        if dep == node_name:
            continue
        dep_entry = sync_cache.get(f"{dep}.lean")
        if not dep_entry or not dep_entry.get("sha256"):
            return None
        deps_sorted.append((dep, str(dep_entry["sha256"])))
        dep_olean_sha = _sha256_of_file_or_none(
            _tablet_olean_path(supervisor_repo, dep)
        )
        if dep_olean_sha is None:
            return None
        olean_deps_sorted.append((dep, dep_olean_sha))
    deps_sorted.sort(key=lambda pair: pair[0])
    olean_deps_sorted.sort(key=lambda pair: pair[0])

    blob = bytearray()
    blob.extend(f"v={cache_version}\n".encode("utf-8"))
    blob.extend(f"script={script_sha256}\n".encode("utf-8"))
    blob.extend(f"toolchain={toolchain_sha256}\n".encode("utf-8"))
    blob.extend(f"manifest={manifest_sha256}\n".encode("utf-8"))
    blob.extend(f"node={node_name}\n".encode("utf-8"))
    blob.extend(f"self={self_entry['sha256']}\n".encode("utf-8"))
    blob.extend(f"self_olean={self_olean_sha}\n".encode("utf-8"))
    for dep_name, dep_sha in deps_sorted:
        blob.extend(f"dep={dep_name}={dep_sha}\n".encode("utf-8"))
    for dep_name, dep_olean_sha in olean_deps_sorted:
        blob.extend(f"olean={dep_name}={dep_olean_sha}\n".encode("utf-8"))
    return hashlib.sha256(bytes(blob)).hexdigest()


def load_semantic_payload(
    cache_dir: Path,
    cache_key: str,
    *,
    expected_version: int,
) -> Optional[Dict[str, Any]]:
    """Read a previously stored semantic-payload sidecar by cache key.

    Returns ``None`` on any of:
    - missing/unreadable file (cold cache),
    - JSON corruption,
    - schema-version mismatch,
    - filename / cache-key mismatch (defends against partial writes,
      manual rename, or other on-disk drift).

    Returning ``None`` is always safe — the caller falls through to the
    live observation path. The cache is never load-bearing for
    correctness.
    """
    sidecar = cache_dir / f"{cache_key}.json"
    if not sidecar.is_file():
        return None
    try:
        text = sidecar.read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        decoded = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(decoded, dict):
        return None
    try:
        version = int(decoded.get("cache_version", 0) or 0)
    except (TypeError, ValueError):
        return None
    if version != expected_version:
        return None
    # Guard against a sidecar that someone (or a partial write) left at a
    # stale path: the basename must equal the cache_key the file claims
    # to encode. This makes corruption detection an O(1) string compare.
    stored_key = str(decoded.get("key_blob_sha256", "") or "")
    if stored_key != cache_key:
        return None
    return decoded


def store_semantic_payload(
    cache_dir: Path,
    cache_key: str,
    *,
    node_name: str,
    ok: bool,
    payload: str,
    error: str,
    cache_version: int,
) -> None:
    """Atomically persist a single node's semantic payload under ``cache_key``.

    Reuses the same temp+rename atomicity primitive (``_atomic_write``) as
    ``write_fingerprint_cache`` so a crash mid-write either leaves the
    sidecar absent (caller treats as cold cache) or commits the full
    record. The ``key_blob_sha256`` field echoes the basename so a
    misplaced/renamed file is rejected by ``load_semantic_payload``.

    ``OSError`` from the underlying mkdir / write / rename (disk full,
    permission denied, etc.) is caught and logged: the sidecar write is
    a best-effort persistence step, never load-bearing for correctness.
    The freshly-computed payload remains usable to the caller; only the
    cache write is lost. The next request retries the same key.
    """
    sidecar = cache_dir / f"{cache_key}.json"
    record: Dict[str, Any] = {
        "cache_version": int(cache_version),
        "node_name": str(node_name),
        "key_blob_sha256": str(cache_key),
        "ok": bool(ok),
        "payload": str(payload),
        "error": str(error),
        "created_ts": time.time(),
    }
    encoded = (
        json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    try:
        cache_dir.mkdir(parents=True, exist_ok=True)
        _atomic_write(sidecar, encoded)
    except OSError as exc:
        _LOGGER.warning(
            "store_semantic_payload could not persist sidecar at %s: %s",
            sidecar,
            exc,
        )


def print_axioms_cache_dir(state_dir: Path) -> Path:
    """Return the directory holding per-key ``print_axioms`` sidecar files.

    Sibling to ``semantic-payloads/`` under ``checker-state/`` so it shares
    the same lifecycle as the rest of the checker state: GC happens when
    the runtime root is wiped, never independently.
    """
    return state_dir / PRINT_AXIOMS_DIRNAME


def load_print_axioms(
    cache_dir: Path,
    cache_key: str,
    *,
    expected_version: int,
) -> Optional[Dict[str, Any]]:
    """Read a previously stored ``print_axioms`` sidecar by cache key.

    Returns ``None`` on any of:
    - missing/unreadable file (cold cache),
    - JSON corruption,
    - schema-version mismatch,
    - filename / cache-key mismatch (defends against partial writes,
      manual rename, or other on-disk drift).

    Returning ``None`` is always safe — the caller falls through to the
    live observation path. The cache is never load-bearing for
    correctness.
    """
    sidecar = cache_dir / f"{cache_key}.json"
    if not sidecar.is_file():
        return None
    try:
        text = sidecar.read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        decoded = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(decoded, dict):
        return None
    try:
        version = int(decoded.get("cache_version", 0) or 0)
    except (TypeError, ValueError):
        return None
    if version != expected_version:
        return None
    stored_key = str(decoded.get("key_blob_sha256", "") or "")
    if stored_key != cache_key:
        return None
    return decoded


def store_print_axioms(
    cache_dir: Path,
    cache_key: str,
    *,
    node_name: str,
    returncode: Any,
    stdout: str,
    stderr: str,
    timed_out: bool,
    spawn_error: str,
    cache_version: int,
) -> None:
    """Atomically persist one node's ``print_axioms`` payload under
    ``cache_key``.

    Reuses the same temp+rename atomicity primitive (``_atomic_write``) as
    ``store_semantic_payload`` so a crash mid-write either leaves the
    sidecar absent (caller treats as cold cache) or commits the full
    record. The ``key_blob_sha256`` field echoes the basename so a
    misplaced/renamed file is rejected by ``load_print_axioms``.

    ``OSError`` from the underlying mkdir / write / rename (disk full,
    permission denied, etc.) is caught and logged: the sidecar write is
    a best-effort persistence step, never load-bearing for correctness.
    The freshly-computed payload remains usable to the caller; only the
    cache write is lost. The next request retries the same key.
    """
    sidecar = cache_dir / f"{cache_key}.json"
    record: Dict[str, Any] = {
        "cache_version": int(cache_version),
        "node_name": str(node_name),
        "key_blob_sha256": str(cache_key),
        "returncode": returncode,
        "stdout": str(stdout),
        "stderr": str(stderr),
        "timed_out": bool(timed_out),
        "spawn_error": str(spawn_error),
        "created_ts": time.time(),
    }
    encoded = (
        json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    try:
        cache_dir.mkdir(parents=True, exist_ok=True)
        _atomic_write(sidecar, encoded)
    except OSError as exc:
        _LOGGER.warning(
            "store_print_axioms could not persist sidecar at %s: %s",
            sidecar,
            exc,
        )


def local_closure_axioms_cache_dir(state_dir: Path) -> Path:
    """Per-key sidecar directory for ``local_closure_axioms`` (Patch C deferred
    cache). Sibling to ``print-axioms-cache`` and ``semantic-payloads`` under
    ``checker-state/``.
    """
    return state_dir / LOCAL_CLOSURE_AXIOMS_DIRNAME


def load_local_closure_axioms(
    cache_dir: Path,
    cache_key: str,
    *,
    expected_version: int,
) -> Optional[Dict[str, Any]]:
    """Read a previously stored ``local_closure_axioms`` sidecar by cache key.

    Mirrors ``load_print_axioms`` exactly: returns ``None`` on missing file,
    JSON corruption, schema-version mismatch, or filename/cache-key mismatch.
    The cache is never load-bearing for correctness — the caller falls through
    to the live probe on any miss.

    The sidecar stores the *full response envelope* the server would have
    returned: status, kernel_axioms, boundary_theorems, strict_*_deps, errors,
    axiomization_check, plus the transport-level returncode/stdout/stderr/etc.
    """
    sidecar = cache_dir / f"{cache_key}.json"
    if not sidecar.is_file():
        return None
    try:
        text = sidecar.read_text(encoding="utf-8")
    except OSError:
        return None
    try:
        decoded = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(decoded, dict):
        return None
    try:
        version = int(decoded.get("cache_version", 0) or 0)
    except (TypeError, ValueError):
        return None
    if version != expected_version:
        return None
    stored_key = str(decoded.get("key_blob_sha256", "") or "")
    if stored_key != cache_key:
        return None
    return decoded


def store_local_closure_axioms(
    cache_dir: Path,
    cache_key: str,
    *,
    node_name: str,
    response: Mapping[str, Any],
    cache_version: int,
) -> None:
    """Atomically persist one node's ``local_closure_axioms`` response envelope
    under ``cache_key``. Best-effort: ``OSError`` is caught and logged.

    Stores the full envelope verbatim (alongside cache metadata) so a cache
    hit can be replayed by the server without re-deriving any fields. Mirrors
    ``store_print_axioms``'s atomicity primitive (``_atomic_write``).
    """
    sidecar = cache_dir / f"{cache_key}.json"
    record: Dict[str, Any] = {
        "cache_version": int(cache_version),
        "node_name": str(node_name),
        "key_blob_sha256": str(cache_key),
        "response": dict(response),
        "created_ts": time.time(),
    }
    encoded = (
        json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
    ).encode("utf-8")
    try:
        cache_dir.mkdir(parents=True, exist_ok=True)
        _atomic_write(sidecar, encoded)
    except OSError as exc:
        _LOGGER.warning(
            "store_local_closure_axioms could not persist sidecar at %s: %s",
            sidecar,
            exc,
        )


__all__ = [
    "PRINT_AXIOMS_CACHE_VERSION",
    "PRINT_AXIOMS_DIRNAME",
    "LOCAL_CLOSURE_AXIOMS_CACHE_VERSION",
    "LOCAL_CLOSURE_AXIOMS_DIRNAME",
    "load_local_closure_axioms",
    "local_closure_axioms_cache_dir",
    "store_local_closure_axioms",
    "SEMANTIC_PAYLOAD_CACHE_VERSION",
    "SEMANTIC_PAYLOAD_DIRNAME",
    "SyncError",
    "compute_semantic_payload_cache_key",
    "load_fingerprint_cache",
    "load_print_axioms",
    "load_semantic_payload",
    "print_axioms_cache_dir",
    "semantic_payload_cache_dir",
    "store_print_axioms",
    "store_semantic_payload",
    "sync_tablet_dir",
    "write_fingerprint_cache",
]

"""Content-hash olean-freshness tests (olean-staleness fix).

Root cause being guarded against: the checker's olean-freshness decision was
mtime-based and content-blind, and the closure / ``#print axioms`` probes read
oleans without rebuilding. An operator ``touch`` (or a ``git reset``) that made
a content-stale ``sorry``-bearing olean look mtime-fresh caused that stale
olean to be served to a probe, which then rejected a genuine close.

The fix decides freshness by a SOURCE-CLOSURE CONTENT HASH
(``observations.tablet_source_closure_hash``, a faithful port of the kernel's
``cache_key::lean_closure_cache_key``), recorded in a sidecar beside each
olean. These tests prove both success criteria:

  (a) mtime-changed-but-content-identical  ⇒ NOT rebuilt / still served.
  (b) content-changed-but-mtime-frozen-newer (the exact ``touch`` scenario:
      a ``sorry`` olean frozen mtime-newer than a now-closed source)
      ⇒ treated stale ⇒ rebuilt ⇒ the closure probe reads the CLOSED olean,
      not the ``sorry`` one.

plus the checker server's content gates (``_compute_compile_cache_subset``,
``_prewarm_oleans_known_current``) and the Rust/Python key equivalence pin.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Callable, Tuple

import pytest

from trellis.atomic_actions import observations
from trellis.checker.server import CheckerServer


# Must match the constant pinned in
# ``kernel/src/cache_key.rs::tests::python_equivalence_pin``.
KERNEL_PIN = "a3967795e3d913306ab6974bdf36c943964b23e317a0cfa9546f6d80cf45f995"

CLOSED_SRC = "import Tablet.Preamble\ntheorem A : True := trivial\n"
SORRY_SRC = "import Tablet.Preamble\ntheorem A : True := by sorry\n"


def _olean_dir(repo: Path) -> Path:
    return repo / ".lake" / "build" / "lib" / "lean" / "Tablet"


def _olean_path(repo: Path, node: str) -> Path:
    return _olean_dir(repo) / f"{node}.olean"


def _make_repo(tmp: Path) -> Path:
    """A minimal supervisor-shaped repo: check script + lake state + Preamble."""
    repo = tmp / "repo"
    (repo / ".trellis" / "scripts").mkdir(parents=True)
    (repo / ".trellis" / "scripts" / "check.py").write_text(
        "#!/usr/bin/env python3\n", encoding="utf-8"
    )
    (repo / "lakefile.lean").write_text("package «stub»\n", encoding="utf-8")
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "Preamble.lean").write_text(
        "import Mathlib.Data.Nat.Basic\n", encoding="utf-8"
    )
    _olean_dir(repo).mkdir(parents=True)
    return repo


def _write_olean(repo: Path, node: str, data: bytes) -> Path:
    olean = _olean_path(repo, node)
    olean.write_bytes(data)
    return olean


def _set_mtime(path: Path, when: float) -> None:
    os.utime(path, (when, when))


def _make_stub_lake(
    fresh_prefix: bytes = b"OLEAN-FRESH-",
) -> Tuple[Callable[..., Any], list]:
    """Stub for ``observations._run_lake_command`` that mimics lake's contract
    AFTER the content gate has prepared the tree: build a target's olean only
    when it is MISSING (a present olean is treated as already-current and left
    untouched), recording each freshly built olean with ``fresh_prefix``.
    """
    calls: list = []

    def stub(repo: Path, args, *, timeout_secs, bwrap_role=None):
        calls.append(list(args))
        olean_dir = _olean_dir(Path(repo))
        olean_dir.mkdir(parents=True, exist_ok=True)
        for arg in args:
            if isinstance(arg, str) and arg.startswith("Tablet."):
                node = arg[len("Tablet."):]
                olean = olean_dir / f"{node}.olean"
                if not olean.exists():
                    olean.write_bytes(fresh_prefix + node.encode("utf-8"))
        return {
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }

    return stub, calls


# ---------------------------- equivalence pin ----------------------------


def test_source_closure_hash_matches_kernel_pin(tmp_path: Path) -> None:
    repo = _make_repo(tmp_path)
    (repo / "Tablet" / "A.lean").write_text(CLOSED_SRC, encoding="utf-8")
    assert observations.tablet_source_closure_hash(repo, "A") == KERNEL_PIN


def test_source_closure_hash_none_when_script_missing(tmp_path: Path) -> None:
    repo = _make_repo(tmp_path)
    (repo / "Tablet" / "A.lean").write_text(CLOSED_SRC, encoding="utf-8")
    (repo / ".trellis" / "scripts" / "check.py").unlink()
    assert observations.tablet_source_closure_hash(repo, "A") is None


# ------------------------ olean_is_content_current ------------------------


def test_content_current_ignores_mtime(tmp_path: Path) -> None:
    """Criterion (a) at the predicate level: an olean whose sidecar matches
    the current source-closure hash is current even when its mtime is OLDER
    than the source (the post-``git reset`` shape)."""
    repo = _make_repo(tmp_path)
    src = repo / "Tablet" / "A.lean"
    src.write_text(CLOSED_SRC, encoding="utf-8")
    olean = _write_olean(repo, "A", b"OLEAN-A-v1")
    observations.write_olean_srcclosure(
        repo, "A", observations.tablet_source_closure_hash(repo, "A")
    )
    # Make the olean look STALE by mtime (older than its source).
    _set_mtime(olean, src.stat().st_mtime - 100)
    assert observations.olean_is_content_current(repo, "A") is True


def test_content_current_false_on_content_change(tmp_path: Path) -> None:
    """Criterion (b) at the predicate level: a content-stale olean is NOT
    current even when its mtime is frozen NEWER than the source."""
    repo = _make_repo(tmp_path)
    src = repo / "Tablet" / "A.lean"
    src.write_text(SORRY_SRC, encoding="utf-8")
    olean = _write_olean(repo, "A", b"OLEAN-A-SORRY")
    observations.write_olean_srcclosure(
        repo, "A", observations.tablet_source_closure_hash(repo, "A")
    )
    # Close the node (source content changes) but freeze the olean newer.
    src.write_text(CLOSED_SRC, encoding="utf-8")
    _set_mtime(olean, src.stat().st_mtime + 1000)
    assert observations.olean_is_content_current(repo, "A") is False


def test_content_current_false_without_sidecar(tmp_path: Path) -> None:
    """Fail-closed: no provenance sidecar ⇒ not current (even if mtime-fresh)."""
    repo = _make_repo(tmp_path)
    src = repo / "Tablet" / "A.lean"
    src.write_text(CLOSED_SRC, encoding="utf-8")
    olean = _write_olean(repo, "A", b"OLEAN-A")
    _set_mtime(olean, src.stat().st_mtime + 1000)
    assert observations.olean_is_content_current(repo, "A") is False


# -------------------- materialize_tablet_oleans gate --------------------


def test_materialize_criterion_a_content_identical_not_rebuilt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Criterion (a): mtime changed (olean older than source) but content
    identical ⇒ the olean is preserved (NOT purged, NOT rebuilt) and still
    reported materialized."""
    repo = _make_repo(tmp_path)
    src = repo / "Tablet" / "A.lean"
    src.write_text(CLOSED_SRC, encoding="utf-8")
    olean = _write_olean(repo, "A", b"OLEAN-A-ORIGINAL")
    observations.write_olean_srcclosure(
        repo, "A", observations.tablet_source_closure_hash(repo, "A")
    )
    _set_mtime(olean, src.stat().st_mtime - 100)  # mtime-stale, content-fresh

    stub, _calls = _make_stub_lake()
    monkeypatch.setattr(observations, "_run_lake_command", stub)

    result = observations.materialize_tablet_oleans(
        repo, ["A"], bwrap_role="lake_compiler"
    )

    # Original bytes survive ⇒ not purged and the stub (build-if-missing) did
    # not rebuild it ⇒ "still served".
    assert olean.read_bytes() == b"OLEAN-A-ORIGINAL"
    assert "A" in result["materialized_nodes"]
    assert result["returncode"] == 0
    assert observations.olean_is_content_current(repo, "A") is True


def test_materialize_criterion_b_stale_sorry_olean_rebuilt(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Criterion (b): a ``sorry`` olean frozen mtime-newer than a now-closed
    source is treated stale ⇒ purged ⇒ rebuilt ⇒ a downstream probe reads the
    CLOSED olean, never the ``sorry`` one."""
    repo = _make_repo(tmp_path)
    src = repo / "Tablet" / "A.lean"

    # 1. Node open (sorry): build a sorry olean + provenance for the sorry src.
    src.write_text(SORRY_SRC, encoding="utf-8")
    olean = _write_olean(repo, "A", b"OLEAN-A-SORRY-AX")
    observations.write_olean_srcclosure(
        repo, "A", observations.tablet_source_closure_hash(repo, "A")
    )

    # 2. Worker closes the node; operator-touch freezes the olean mtime-newer.
    src.write_text(CLOSED_SRC, encoding="utf-8")
    _set_mtime(olean, src.stat().st_mtime + 1000)

    stub, _calls = _make_stub_lake(fresh_prefix=b"OLEAN-CLOSED-")
    monkeypatch.setattr(observations, "_run_lake_command", stub)

    result = observations.materialize_tablet_oleans(
        repo, ["A"], bwrap_role="lake_compiler"
    )

    # The stale sorry olean was purged and rebuilt from the closed source.
    rebuilt = olean.read_bytes()
    assert rebuilt == b"OLEAN-CLOSED-A"
    assert b"SORRY" not in rebuilt  # a probe reading this sees closed axioms
    assert "A" in result["materialized_nodes"]
    # Provenance now matches the CLOSED source closure.
    assert observations.read_olean_srcclosure(repo, "A") == (
        observations.tablet_source_closure_hash(repo, "A")
    )
    assert observations.olean_is_content_current(repo, "A") is True


def test_materialize_fail_closed_without_sidecar_rebuilds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Fail-closed: an olean with no provenance sidecar (e.g. first deploy, or
    an operator-touched olean) is purged + rebuilt regardless of mtime."""
    repo = _make_repo(tmp_path)
    src = repo / "Tablet" / "A.lean"
    src.write_text(CLOSED_SRC, encoding="utf-8")
    olean = _write_olean(repo, "A", b"OLEAN-A-UNVOUCHED")
    _set_mtime(olean, src.stat().st_mtime + 1000)  # mtime-fresh but no sidecar

    stub, _calls = _make_stub_lake(fresh_prefix=b"OLEAN-REBUILT-")
    monkeypatch.setattr(observations, "_run_lake_command", stub)

    observations.materialize_tablet_oleans(repo, ["A"], bwrap_role="lake_compiler")
    assert olean.read_bytes() == b"OLEAN-REBUILT-A"
    assert observations.olean_is_content_current(repo, "A") is True


# ----------------------- checker server content gates -----------------------


def _server_for(tmp_path: Path, content_repo: Path) -> CheckerServer:
    runtime = tmp_path / "r" / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    (tmp_path / "r" / "Tablet").mkdir(parents=True, exist_ok=True)
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
    # Point the gate at our content fixture; never started, no socket bound.
    server.supervisor_repo = content_repo
    return server


def test_compute_compile_cache_subset_is_content_based(tmp_path: Path) -> None:
    repo = _make_repo(tmp_path)
    src = repo / "Tablet" / "A.lean"
    src.write_text(CLOSED_SRC, encoding="utf-8")
    olean = _write_olean(repo, "A", b"OLEAN-A")
    observations.write_olean_srcclosure(
        repo, "A", observations.tablet_source_closure_hash(repo, "A")
    )
    _set_mtime(olean, src.stat().st_mtime - 100)  # mtime-stale, content-fresh

    server = _server_for(tmp_path / "srv1", repo)
    server._oleans_known_current.add("A")
    # Content-current despite stale mtime.
    assert server._compute_compile_cache_subset(["A"]) == {"A"}

    # Now change the source (close→reopen shape) without updating provenance;
    # freeze the olean mtime-newer. Content gate must evict it.
    src.write_text(SORRY_SRC, encoding="utf-8")
    _set_mtime(olean, src.stat().st_mtime + 1000)
    server._oleans_known_current.add("A")
    assert server._compute_compile_cache_subset(["A"]) == set()
    assert "A" not in server._oleans_known_current  # evicted


def test_prewarm_is_content_based(tmp_path: Path) -> None:
    repo = _make_repo(tmp_path)
    src = repo / "Tablet" / "A.lean"
    src.write_text(CLOSED_SRC, encoding="utf-8")

    # B.lean: stale olean (operator-touched mtime-newer, wrong provenance).
    bsrc = repo / "Tablet" / "B.lean"
    bsrc.write_text("import Tablet.Preamble\ntheorem B : True := trivial\n", "utf-8")

    # A: content-current sidecar, but mtime-stale.
    aolean = _write_olean(repo, "A", b"OLEAN-A")
    observations.write_olean_srcclosure(
        repo, "A", observations.tablet_source_closure_hash(repo, "A")
    )
    _set_mtime(aolean, src.stat().st_mtime - 100)

    # B: olean mtime-frozen-newer but provenance points at a different source.
    bolean = _write_olean(repo, "B", b"OLEAN-B-STALE")
    observations.write_olean_srcclosure(repo, "B", "deadbeef" * 8)
    _set_mtime(bolean, bsrc.stat().st_mtime + 1000)

    server = _server_for(tmp_path / "srv2", repo)
    server._prewarm_oleans_known_current()
    assert "A" in server._oleans_known_current  # content-current
    assert "B" not in server._oleans_known_current  # stale provenance, not mtime

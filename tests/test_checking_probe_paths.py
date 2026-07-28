"""Tests for the StuckMathAudit probe-path validator in trellis.checking.

Regression coverage for the cycle-0 probe-path validator mismatch: a revision
run's first cycle is ``0``. The bridge planner prompt assigns the audit scratch
dir as ``cycle-0-request-NNN`` (``request.get('cycle', ...)``, which yields the
literal ``0``). The validator must compute the SAME scratch path; it must not
coerce the falsy integer ``0`` to ``"unknown"`` (which produced
``cycle-unknown-request-NNN`` and rejected every probe path the planner wrote).
"""

from __future__ import annotations

from pathlib import Path

from trellis.checking import _stuck_math_audit_probe_path_errors


def _write_probe(repo: Path, rel: str) -> None:
    path = repo / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("-- probe\n", encoding="utf-8")


def test_cycle_zero_probe_path_validates(tmp_path: Path) -> None:
    """Cycle ``0`` must map to ``cycle-0-request-NNN`` (not ``cycle-unknown``)
    so a probe the planner wrote there validates."""
    rel = ".trellis/stuck-math-audit/cycle-0-request-42/probe.lean"
    _write_probe(tmp_path, rel)

    errors = _stuck_math_audit_probe_path_errors(
        {"probe_paths": [rel]},
        audit_request={"id": 42, "cycle": 0},
        repo=tmp_path,
    )

    assert errors == []


def test_cycle_zero_does_not_become_unknown(tmp_path: Path) -> None:
    """A probe under the (wrong, pre-fix) ``cycle-unknown`` dir must now be
    rejected, proving cycle ``0`` is no longer coerced to ``"unknown"``."""
    rel = ".trellis/stuck-math-audit/cycle-unknown-request-42/probe.lean"
    _write_probe(tmp_path, rel)

    errors = _stuck_math_audit_probe_path_errors(
        {"probe_paths": [rel]},
        audit_request={"id": 42, "cycle": 0},
        repo=tmp_path,
    )

    assert errors
    assert any("cycle-0-request-42" in err for err in errors)


def test_nonzero_cycle_still_validates(tmp_path: Path) -> None:
    rel = ".trellis/stuck-math-audit/cycle-7-request-99/probe.lean"
    _write_probe(tmp_path, rel)

    errors = _stuck_math_audit_probe_path_errors(
        {"probe_paths": [rel]},
        audit_request={"id": 99, "cycle": 7},
        repo=tmp_path,
    )

    assert errors == []


def test_missing_cycle_falls_back_to_unknown(tmp_path: Path) -> None:
    """A genuinely-missing cycle key still falls back to ``unknown`` (the
    assign side does the same), preserving prior behavior."""
    rel = ".trellis/stuck-math-audit/cycle-unknown-request-5/probe.lean"
    _write_probe(tmp_path, rel)

    errors = _stuck_math_audit_probe_path_errors(
        {"probe_paths": [rel]},
        audit_request={"id": 5},
        repo=tmp_path,
    )

    assert errors == []

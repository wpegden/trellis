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


def test_prior_cycle_probe_path_citation_validates(tmp_path: Path) -> None:
    """A read-only citation of a PRIOR cycle's scratch artifact validates
    (dec2flt burst 248: demoting the old witness citation to report prose is
    the exact mechanism by which the false-target finding decayed). The
    integrity checks that motivated the old restriction — existing regular
    file, no symlink, no hardlink, inside the audit scratch tree — still
    apply to the cited path."""
    rel = ".trellis/stuck-math-audit/cycle-3-request-11/witness.json"
    _write_probe(tmp_path, rel)

    errors = _stuck_math_audit_probe_path_errors(
        {"probe_paths": [rel]},
        audit_request={"id": 42, "cycle": 9},
        repo=tmp_path,
    )

    assert errors == []


def test_probe_path_outside_cycle_dirs_rejected(tmp_path: Path) -> None:
    """Only ``cycle-*`` scratch dirs are citable — a loose file directly
    under ``.trellis/stuck-math-audit/`` is rejected."""
    rel = ".trellis/stuck-math-audit/notes.md"
    _write_probe(tmp_path, rel)

    errors = _stuck_math_audit_probe_path_errors(
        {"probe_paths": [rel]},
        audit_request={"id": 42, "cycle": 9},
        repo=tmp_path,
    )

    assert errors
    assert any("cycle-" in err for err in errors)


def test_prior_cycle_probe_path_must_exist(tmp_path: Path) -> None:
    rel = ".trellis/stuck-math-audit/cycle-3-request-11/gone.json"

    errors = _stuck_math_audit_probe_path_errors(
        {"probe_paths": [rel]},
        audit_request={"id": 42, "cycle": 9},
        repo=tmp_path,
    )

    assert errors
    assert any("existing regular file" in err for err in errors)


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

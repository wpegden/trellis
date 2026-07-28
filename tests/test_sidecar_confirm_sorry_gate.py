"""Confirm-gate sorry detection (``SidecarCompileLoop._lake_build``).

Pure-mock unit tests: no lake, no toolchain, no server. We stub
``build_lake_command`` and ``subprocess.run`` inside the ``compile_loop``
module so the confirm gate sees a fabricated ``lake build`` result and we
assert its clean/unclean verdict.

The load-bearing case is the Lean v4.30.0-rc1 warning phrasing, which uses
BACKTICKS — ``declaration uses `sorry` `` — emitted as a WARNING at exit
code 0. A returncode-only gate false-greens a sorry body; the gate must
reject it on the warning text regardless of quote style.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from trellis.sidecar import compile_loop as _cl
from trellis.sidecar.compile_loop import SidecarCompileLoop
from trellis.sidecar.config import SidecarConfig


class _FakeProc:
    def __init__(self, returncode: int, stdout: str = "", stderr: str = "") -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def _loop() -> SidecarCompileLoop:
    return SidecarCompileLoop(Path("/nonexistent-repo"), SidecarConfig(enabled=True, sandbox_role=""))


@pytest.fixture
def stub_lake(monkeypatch: pytest.MonkeyPatch):
    """Return a setter that pins the fabricated ``lake build`` proc."""
    monkeypatch.setattr(_cl, "build_lake_command", lambda *a, **k: ["true"])
    box: dict = {}

    def _run(*args, **kwargs):
        return box["proc"]

    monkeypatch.setattr(_cl.subprocess, "run", _run)

    def _set(proc: _FakeProc) -> None:
        box["proc"] = proc

    return _set


# ``warning: ...: declaration uses `sorry` `` — the v4.30.0-rc1 phrasing,
# exit code 0. This is the counterfactual: it PASSES the pre-fix gate.
_BACKTICK_SORRY = "warning: ././Tablet/Rung.lean:4:8: declaration uses `sorry`\n"
_STRAIGHT_SORRY = "warning: ././Tablet/Rung.lean:4:8: declaration uses 'sorry'\n"
_CLEAN = "Build completed successfully.\n"


def test_backtick_sorry_is_unclean(stub_lake) -> None:
    """v4.30 backtick warning at rc 0 => NOT closed. Fails pre-fix."""
    stub_lake(_FakeProc(returncode=0, stdout=_BACKTICK_SORRY))
    result = _loop()._lake_build("Rung")
    assert result.ok is False
    assert "sorry" in result.log.lower()


def test_backtick_sorry_on_stderr_is_unclean(stub_lake) -> None:
    """Same warning routed to stderr must also be caught."""
    stub_lake(_FakeProc(returncode=0, stdout="", stderr=_BACKTICK_SORRY))
    result = _loop()._lake_build("Rung")
    assert result.ok is False


def test_straight_quote_sorry_is_unclean(stub_lake) -> None:
    """Legacy straight-quote phrasing stays rejected (no regression)."""
    stub_lake(_FakeProc(returncode=0, stdout=_STRAIGHT_SORRY))
    result = _loop()._lake_build("Rung")
    assert result.ok is False


def test_sorryax_axiom_is_unclean(stub_lake) -> None:
    """Axiom-level ``sorryAx`` in the output is rejected."""
    stub_lake(_FakeProc(returncode=0, stdout="'Rung' depends on axioms: [sorryAx]\n"))
    result = _loop()._lake_build("Rung")
    assert result.ok is False


def test_clean_build_is_clean(stub_lake) -> None:
    """A genuinely clean rc-0 build passes the gate."""
    stub_lake(_FakeProc(returncode=0, stdout=_CLEAN))
    result = _loop()._lake_build("Rung")
    assert result.ok is True


def test_nonzero_returncode_is_unclean(stub_lake) -> None:
    """A hard build error (rc != 0) is unclean regardless of text."""
    stub_lake(_FakeProc(returncode=1, stdout="error: unknown identifier\n"))
    result = _loop()._lake_build("Rung")
    assert result.ok is False

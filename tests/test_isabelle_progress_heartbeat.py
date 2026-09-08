"""Tests for the Isabelle acceptance progress heartbeat.

The Lean acceptance path streams sub-progress to the calling worker agent
through the ``TRELLIS_ACCEPTANCE_PROGRESS_LOG`` side channel
(`observations._progress_emit` + the tail-and-forward thread in
`trellis.runtime.kernel_cli._run_kernel_cli_once`). These tests pin the
ported Isabelle emissions:

* the ``*_server_side`` observation functions emit per-cert-probe start /
  completion lines (with the worker-build / probe-build sub-steps);
* the ``_def`` principal-fact retry announces itself;
* the warm gate's anomaly → cold-fallback announces the (~15 min) cold
  session before it starts;
* the worker-side ``check.py`` Isabelle dispatch (the process that actually
  inherits the env var during acceptance) emits a start/done pair around the
  checker-server round-trip;
* and the whole mechanism is a strict no-op when the env var is unset
  (no file is ever created or written).

No real Isabelle is spawned anywhere: sessions are stubs returning scripted
:class:`CheckOutcome`s, exactly as in ``test_isabelle_observations.py``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Dict, List

import pytest

from trellis.atomic_actions import cli as cli_mod
from trellis.atomic_actions import isabelle_observations as iso
from trellis.checker.isabelle_session import CheckOutcome, IsabelleSessionError

PROGRESS_ENV = "TRELLIS_ACCEPTANCE_PROGRESS_LOG"


class _StubSession:
    """Session stub: scripted :class:`CheckOutcome` per ``check_theory``."""

    def __init__(self, responder, *, warm_prefix_enabled: bool = False) -> None:
        self._responder = responder
        self.warm_prefix_enabled = warm_prefix_enabled
        self.calls: List[Dict[str, Any]] = []

    def check_theory(self, **kwargs: Any) -> CheckOutcome:
        self.calls.append(kwargs)
        return self._responder(**kwargs)

    def close(self) -> None:  # pragma: no cover - reap bookkeeping only
        pass


def _ok_outcome(*, theorem_exists: bool = True) -> CheckOutcome:
    return CheckOutcome(
        ok=True,
        failed=0,
        finished=10,
        theory_name="Draft.Tablet_Foo",
        node_name="/tmp/Tablet_Foo.thy",
        oracles=[],
        dependencies=["refl"],
        theorem_exists=theorem_exists,
        statement_repr="Foo holds",
        statement_repr_long="Tablet_Foo.Foo holds",
    )


def _lines(log: Path) -> List[str]:
    return log.read_text(encoding="utf-8").splitlines()


# ------------------------- server-side observation fns -------------------------


def test_thm_deps_server_side_emits_probe_progress(tmp_path, monkeypatch) -> None:
    log = tmp_path / "progress.log"
    monkeypatch.setenv(PROGRESS_ENV, str(log))
    master = tmp_path / "session"
    master.mkdir()

    sess = _StubSession(lambda **kw: _ok_outcome())
    cert = iso.thm_deps_server_side(
        sess, master_dir=str(master), theory="Tablet_Foo", cert_theorem="Foo"
    )
    assert cert["status"] == iso.STATUS_OK

    lines = _lines(log)
    # Per-cert-probe start (node + op) and completion (duration + verdict).
    assert any(
        line.startswith("[acceptance]   isabelle-thm-deps Tablet_Foo: cert probe starting")
        for line in lines
    ), lines
    assert any(
        "isabelle-thm-deps Tablet_Foo: done status=ok" in line and line.endswith("s")
        for line in lines
    ), lines
    # The two long sub-steps announce themselves too.
    assert any("cert-probe Tablet_Foo: worker theory build starting" in line for line in lines)
    assert any("cert-probe Tablet_Foo: worker theory build done returncode=0" in line for line in lines)
    assert any("cert-probe Tablet_Foo: checker probe build starting" in line for line in lines)
    assert any("cert-probe Tablet_Foo: checker probe build done returncode=0" in line for line in lines)


def test_thm_deps_server_side_emits_worker_proof_failure(tmp_path, monkeypatch) -> None:
    log = tmp_path / "progress.log"
    monkeypatch.setenv(PROGRESS_ENV, str(log))
    master = tmp_path / "session"
    master.mkdir()

    def responder(**kw: Any) -> CheckOutcome:
        return CheckOutcome(
            ok=False,
            failed=1,
            finished=9,
            theory_name="Draft.Tablet_Foo",
            node_name="/tmp/Tablet_Foo.thy",
            error_lines=["Failed to apply initial proof method"],
        )

    cert = iso.thm_deps_server_side(
        _StubSession(responder),
        master_dir=str(master),
        theory="Tablet_Foo",
        cert_theorem="Foo",
    )
    assert cert["status"] == iso.STATUS_INVALID_PROOF

    lines = _lines(log)
    assert any(
        "isabelle-thm-deps Tablet_Foo: done status=invalid_proof (worker proof failed)" in line
        for line in lines
    ), lines


def test_def_retry_emits_progress(tmp_path, monkeypatch) -> None:
    """The definition-node `_def` retry announces itself on the channel."""
    log = tmp_path / "progress.log"
    monkeypatch.setenv(PROGRESS_ENV, str(log))
    master = tmp_path / "session"
    master.mkdir()

    def responder(**kw: Any) -> CheckOutcome:
        theory = str(kw.get("theory", ""))
        cert_theorem = str(kw.get("cert_theorem", ""))
        if "__Cert" in theory and not cert_theorem.endswith("_def"):
            # The bare-name probe: Isabelle reports the principal undefined
            # (every error is that one cause, so the retry fires).
            return CheckOutcome(
                ok=False,
                failed=1,
                finished=9,
                theory_name="Draft.Probe",
                node_name="/tmp/probe.thy",
                error_lines=['Undefined fact: "Tablet_Foo.Foo"'],
            )
        return _ok_outcome()

    cert = iso.thm_deps_server_side(
        _StubSession(responder),
        master_dir=str(master),
        theory="Tablet_Foo",
        cert_theorem="Foo",
        qualified_thm="Tablet_Foo.Foo",
    )
    assert cert["status"] == iso.STATUS_OK

    lines = _lines(log)
    assert any(
        "cert-probe Tablet_Foo: principal fact undefined; retrying certificate as Foo_def"
        in line
        for line in lines
    ), lines


def test_progress_noop_when_env_unset(tmp_path, monkeypatch) -> None:
    """Env unset ⇒ the heartbeat is a strict no-op: no file is written."""
    log = tmp_path / "progress.log"
    monkeypatch.delenv(PROGRESS_ENV, raising=False)
    master = tmp_path / "session"
    master.mkdir()

    cert = iso.thm_deps_server_side(
        _StubSession(lambda **kw: _ok_outcome()),
        master_dir=str(master),
        theory="Tablet_Foo",
        cert_theorem="Foo",
    )
    assert cert["status"] == iso.STATUS_OK
    assert not log.exists()
    # Nothing else got dropped in the tmp dir either (only the session dir we
    # made; the probe .thy is deleted by run_cert_probe's finally).
    assert sorted(p.name for p in tmp_path.iterdir()) == ["session"]


# ------------------------- warm gate cold fallback -------------------------


def test_warm_gate_cold_fallback_emits_progress(tmp_path, monkeypatch) -> None:
    from trellis.checker import isabelle_scaffold
    from trellis.checker.isabelle_warm_gate import IsabelleWarmGate
    from trellis.checker.isabelle_warm_config import IsabelleWarmSessionConfig

    log = tmp_path / "progress.log"
    monkeypatch.setenv(PROGRESS_ENV, str(log))

    session_dir = tmp_path / "isabelle"
    session_dir.mkdir()
    (session_dir / f"{isabelle_scaffold.PREAMBLE_THEORY}.thy").write_text(
        f"theory {isabelle_scaffold.PREAMBLE_THEORY}\n  imports Complex_Main\nbegin\nend\n",
        encoding="utf-8",
    )
    (session_dir / "Tablet_Foo.thy").write_text(
        "theory Tablet_Foo\n  imports Tablet_Preamble\nbegin\n"
        'lemma Foo: "True" by simp\nend\n',
        encoding="utf-8",
    )
    runtime_root = tmp_path / "rt"
    runtime_root.mkdir()

    def warm_factory():
        raise IsabelleSessionError("connect_failed", "warm socket dropped")

    gate = IsabelleWarmGate(
        session_dir=session_dir,
        runtime_root=runtime_root,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session_factory=warm_factory,
        reap_warm_session=lambda: None,
        cold_session_factory=lambda: _StubSession(lambda **kw: _ok_outcome()),
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="Foo",
        theory="Tablet_Foo",
        cert_theorem="Foo",
        qualified_thm="Tablet_Foo.Foo",
        timeout_secs=5.0,
    )
    assert used_cold is True
    assert payload["status"] == iso.STATUS_OK

    lines = _lines(log)
    # The cold fallback names the node and warns about its duration BEFORE the
    # ~15-minute cold session starts, then reports completion.
    assert any(
        "isabelle isabelle_thm_deps Foo: warm-session anomaly" in line
        and "cold fallback starting" in line
        for line in lines
    ), lines
    assert any(
        "isabelle isabelle_thm_deps Foo: cold fallback done" in line for line in lines
    ), lines


# ------------------------- worker-side check.py seam -------------------------


def test_cli_isabelle_dispatch_emits_progress(tmp_path, monkeypatch, capsys) -> None:
    """The check.py child (the process that inherits the env var during
    acceptance) emits a start/done pair around the checker-server round-trip."""
    log = tmp_path / "progress.log"
    monkeypatch.setenv(PROGRESS_ENV, str(log))
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", str(tmp_path / "checker.sock"))

    def fake_client(socket_path, node_name, *, timeout_secs):
        return {"request_id": "r1", "node": node_name, "status": "ok", "returncode": 0}

    monkeypatch.setattr(cli_mod, "client_isabelle_thm_deps", fake_client)

    rc = cli_mod.main(["isabelle-thm-deps", "Foo"])
    assert rc == 0
    out = capsys.readouterr().out
    assert '"status": "ok"' in out

    lines = _lines(log)
    assert any(
        line.startswith("[acceptance]   isabelle-thm-deps Foo: dispatching to checker server")
        for line in lines
    ), lines
    assert any(
        "isabelle-thm-deps Foo: done status=ok in" in line for line in lines
    ), lines


def test_cli_isabelle_dispatch_emits_failure(tmp_path, monkeypatch, capsys) -> None:
    from trellis.atomic_actions.checker_client import CheckerRpcError

    log = tmp_path / "progress.log"
    monkeypatch.setenv(PROGRESS_ENV, str(log))
    monkeypatch.setenv("TRELLIS_CHECKER_SOCKET", str(tmp_path / "checker.sock"))

    def fake_client(socket_path, node_name, *, timeout_secs):
        raise CheckerRpcError("connect_failed", "server socket closed")

    monkeypatch.setattr(cli_mod, "client_isabelle_thm_deps", fake_client)

    rc = cli_mod.main(["isabelle-thm-deps", "Foo"])
    assert rc == 2
    assert "connect_failed" in capsys.readouterr().out

    lines = _lines(log)
    assert any(
        "isabelle-thm-deps Foo: failed (connect_failed) in" in line for line in lines
    ), lines

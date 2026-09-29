"""Tests for the Phase-2 warm Isabelle node-check GATE + cold backstop.

The SENSITIVE seam (gate closure-correctness). These deterministic tests use a
STUB ``IsabelleSession`` (no real ``isabelle`` spawned) that returns scripted
:class:`CheckOutcome`s for ``check_theory``; the observation layer
(``isabelle_observations.*_server_side``) drives that stub exactly as it drives a
real session, so the cert/envelope synthesis is exercised end-to-end. The live
warm==cold-through-the-gate proof is the opt-in ``isabelle_live`` test at the
bottom.

Coverage (the brief's a–e):
* (a) flag-OFF is a true no-op — the dispatcher never enters the gate and the
  cold path is byte-for-byte the pre-Phase-2 path;
* (b) warm verdict == cold verdict on a real closed node (LIVE, cert byte-equal),
  through the gate;
* (c) the backstop CATCHES a forced warm/cold mismatch and HALTS (writes the
  marker, returns the COLD cert, does NOT accept the warm one);
* (d) reconciliation add / remove / content-change + the gate's accepted-set
  enumeration excludes the in-flight node and the cert probe;
* (e) a warm-session anomaly degrades gracefully to a fresh cold session.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import pytest

from trellis.atomic_actions import isabelle_observations as iso
from trellis.checker import isabelle_scaffold, isabelle_warm_gate as wg
from trellis.checker.isabelle_session import (
    CheckOutcome,
    IsabelleReconcileBudgetExhausted,
    IsabelleSession,
    IsabelleSessionError,
)
from trellis.checker.isabelle_warm_config import IsabelleWarmSessionConfig


# ============================ stub session ============================


class _StubSession:
    """A minimal stand-in for :class:`IsabelleSession`.

    Records reconcile/check calls and returns scripted :class:`CheckOutcome`s.
    ``check_responder(theory)`` returns the outcome (or raises an
    :class:`IsabelleSessionError`) for a ``check_theory`` of ``theory``. The
    observation layer calls ``check_theory`` (the worker build) then a second
    ``check_theory`` for the ``*__Cert`` probe — the responder sees both.
    """

    def __init__(
        self,
        *,
        check_responder,
        warm_prefix_enabled: bool = True,
    ) -> None:
        self.warm_prefix_enabled = warm_prefix_enabled
        self._check_responder = check_responder
        self.reconciled: List[Tuple[str, Tuple[str, ...]]] = []
        # The ``total_budget_secs`` each reconcile was handed (None = legacy
        # per-promote default) — lets tests assert the gate threads the op
        # budget down.
        self.reconcile_budgets: List[Optional[float]] = []
        self.checked: List[str] = []
        self.purged: List[str] = []
        self.closed = False

    # warm-prefix API the gate uses
    def reconcile_accepted_base(
        self, *, master_dir, accepted, timeout_secs=0.0, total_budget_secs=None
    ):
        if not self.warm_prefix_enabled:
            raise IsabelleSessionError("protocol_error", "reconcile on cold session")
        self.reconciled.append((master_dir, tuple(accepted)))
        self.reconcile_budgets.append(total_budget_secs)

    # H1: the gate purges the in-flight theory (+ its probe) before re-checking
    # so a proof-replacing edit re-elaborates fresh. Record the purges so a test
    # can assert it happened; a cold (non-warm) stub must never be purged.
    def purge_inflight(self, *, master_dir, theory, timeout_secs=60.0):
        if not self.warm_prefix_enabled:
            raise IsabelleSessionError("protocol_error", "purge on cold session")
        self.purged.append(theory)

    # H1: ``run_cert_probe`` asks for the content-keyed in-flight alias to
    # elaborate. This stub does not model held-open residency (it scripts
    # outcomes by theory name directly), so it returns the node name UNCHANGED —
    # the real alias mechanism is covered in ``test_isabelle_session.py``.
    def fresh_inflight_theory(self, *, master_dir, theory):
        return theory

    # the observation layer calls THIS
    def check_theory(self, *, master_dir, theory, cert_theorem=None, timeout_secs=0.0):
        self.checked.append(theory)
        return self._check_responder(theory)

    # the Phase-3 warm advisory (run_warm_advisory) calls THIS — a thin
    # {ok, seconds, errors} view over check_theory's CheckOutcome.
    def check_node(self, *, master_dir, theory, cert_theorem=None, timeout_secs=0.0):
        from trellis.checker.isabelle_session import WarmCheckOutcome

        self.checked.append(theory)
        outcome = self._check_responder(theory)  # may raise IsabelleSessionError
        return WarmCheckOutcome(
            ok=(outcome.returncode == 0),
            seconds=0.12,
            errors=list(outcome.error_lines),
            outcome=outcome,
        )

    def close(self) -> None:
        self.closed = True


def _ok_outcome(theory: str, *, oracles=None, deps=None, stmt="P x = Q x") -> CheckOutcome:
    """A clean, closed CheckOutcome (worker build or probe)."""
    return CheckOutcome(
        ok=True,
        failed=0,
        finished=10,
        theory_name=f"Draft.{theory}",
        node_name=f"/tmp/{theory}.thy",
        oracles=list(oracles or []),
        dependencies=list(deps or ["refl", "One_nat_def"]),
        theorem_exists=True,
        extra_shyps=[],
        statement_repr=stmt,
        statement_hash="" if not stmt else _hash(stmt),
        statement_repr_long=stmt,
    )


def _hash(text: str) -> str:
    import hashlib

    from trellis.checker.isabelle_session import normalize_statement_repr

    norm = normalize_statement_repr(text)
    return hashlib.sha256(norm.encode("utf-8")).hexdigest() if norm else ""


# ============================ scaffold helpers ============================


def _write_session_scaffold(
    session_dir: Path,
    nodes: List[str],
    *,
    imports: Optional[Dict[str, List[str]]] = None,
) -> None:
    """Write the Preamble + per-node ``Tablet_<Node>.thy`` into the session dir.

    Enough for ``_import_cone_theories`` to parse each theory header and walk the
    cone. The bodies are inert — the stub session never really elaborates them.

    ``imports`` maps a node to the EXTRA ``Tablet_*`` theories its header imports
    (every node always imports the Preamble). Node theories are import-isolated
    by default, which is the realistic case: the scaffold has each node import
    only the Preamble unless the worker declares a real dependency.
    """
    session_dir.mkdir(parents=True, exist_ok=True)
    (session_dir / f"{isabelle_scaffold.PREAMBLE_THEORY}.thy").write_text(
        f"theory {isabelle_scaffold.PREAMBLE_THEORY}\n  imports Complex_Main\nbegin\nend\n",
        encoding="utf-8",
    )
    extra = imports or {}
    for node in nodes:
        deps = " ".join([isabelle_scaffold.PREAMBLE_THEORY, *extra.get(node, [])])
        (session_dir / f"Tablet_{node}.thy").write_text(
            f"theory Tablet_{node}\n  imports {deps}\n"
            f"begin\nlemma {node}: \"True\" by simp\nend\n",
            encoding="utf-8",
        )


def _make_gate(
    tmp_path: Path,
    *,
    config: IsabelleWarmSessionConfig,
    warm_session,
    cold_session_factory,
    runtime_root: Optional[Path] = None,
):
    """Build an :class:`IsabelleWarmGate` over stub sessions + an in-mem reaper.

    The cold cross-check is NODE-SCOPED: it recomputes the cert via
    ``run_cert_probe`` on a fresh ``cold_session_factory()`` session (the active
    node + its import cone only — exactly the warm verdict's scope), never a
    whole-session ``isabelle build``. So the gate takes no ``cold_build``.
    """
    session_dir = tmp_path / "isabelle"
    rt = runtime_root if runtime_root is not None else (tmp_path / "rt")
    rt.mkdir(parents=True, exist_ok=True)
    held = {"warm": warm_session}

    def warm_factory():
        if held["warm"] is None:
            held["warm"] = warm_session_rebuild()
        return held["warm"]

    rebuilt = {"n": 0}

    def warm_session_rebuild():
        rebuilt["n"] += 1
        return warm_session  # in these tests a fresh handle is unnecessary

    def reap_warm():
        held["warm"] = None

    gate = wg.IsabelleWarmGate(
        session_dir=session_dir,
        runtime_root=rt,
        config=config,
        warm_session_factory=warm_factory,
        reap_warm_session=reap_warm,
        cold_session_factory=cold_session_factory,
    )
    return gate, session_dir, rt, held, rebuilt


# ============================ (d) reconciliation enumeration ============================


def test_import_cone_excludes_unimported_siblings(tmp_path: Path) -> None:
    """The warm base is the in-flight node's IMPORT CONE — not every sibling.

    The Lean analogue is `lake build Tablet.<Node>`: only the node's import
    closure is named. A sibling the node does not import must never enter the
    base, because a failure in that sibling would otherwise reach this node's
    certificate (live-observed: two nodes that passed `isabelle_check_node`
    rc=0 were returned `invalid_proof` because an unrelated sibling in the same
    batch did not compile).
    """
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["Alpha", "Beta", "InFlight"])
    # A leftover checker probe theory must never be promoted into the base.
    (session_dir / "Tablet_InFlight__Cert.thy").write_text(
        "theory Tablet_InFlight__Cert\n  imports Tablet_InFlight\nbegin\nend\n",
        encoding="utf-8",
    )
    cone = wg._import_cone_theories(session_dir, theory="Tablet_InFlight")
    # InFlight imports only the Preamble, so Alpha/Beta are unreachable from it.
    assert cone == ["Tablet_Preamble"]
    assert "Tablet_Alpha" not in cone
    assert "Tablet_Beta" not in cone
    assert "Tablet_InFlight" not in cone
    assert "Tablet_InFlight__Cert" not in cone


def test_import_cone_includes_transitive_deps_dependency_first(tmp_path: Path) -> None:
    """A real dependency IS held warm, transitively, dependencies before dependents."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(
        session_dir,
        ["Base", "Mid", "InFlight", "Unrelated"],
        imports={"Mid": ["Tablet_Base"], "InFlight": ["Tablet_Mid"]},
    )
    cone = wg._import_cone_theories(session_dir, theory="Tablet_InFlight")
    assert cone == ["Tablet_Preamble", "Tablet_Base", "Tablet_Mid"]
    # Dependency-first ordering: a theory precedes anything importing it.
    assert cone.index("Tablet_Base") < cone.index("Tablet_Mid")
    # An unrelated sibling stays out even though it sits in the same directory.
    assert "Tablet_Unrelated" not in cone


def test_broken_sibling_in_same_batch_cannot_reach_a_clean_node(tmp_path: Path) -> None:
    """REGRESSION (live incident, conn-isa 2026-07-29).

    A worker authored EIGHT new definition nodes in one batch. One (`Connected`)
    failed to elaborate — the shared preamble imports `Complex_Main`, which does
    not export the `pmf` type. The checker then returned `invalid_proof` from
    `isabelle_thm_deps` for ALL eight, including `PotentialEdges` and `VertexSet`
    which had just passed `isabelle_check_node` with rc=0. A cold build of the
    same source passed: correct mathematics was rejected because a SIBLING was
    broken.

    Cause: the warm base was every `Tablet_*.thy` file present in the session
    dir. The projected files are whatever the worker last authored — accepted or
    not — so seven unaccepted, possibly-broken siblings were promoted as the
    "accepted base" of every node's check.

    The warm base must contain only what the node under check actually imports.
    """
    session_dir = tmp_path / "isabelle"
    batch = ["Connected", "PotentialEdges", "VertexSet", "GnpMeasure"]
    _write_session_scaffold(session_dir, batch)
    # `Connected` does not elaborate (mirrors the live `pmf`-not-in-scope failure).
    (session_dir / "Tablet_Connected.thy").write_text(
        "theory Tablet_Connected\n  imports Tablet_Preamble\n"
        'begin\ndefinition broken :: "nat pmf" where "broken = undefined"\nend\n',
        encoding="utf-8",
    )

    # Checking a clean node that does NOT import the broken one.
    for clean in ("PotentialEdges", "VertexSet"):
        cone = wg._import_cone_theories(session_dir, theory=f"Tablet_{clean}")
        assert "Tablet_Connected" not in cone, (
            f"the broken sibling must not enter {clean}'s warm base — that is "
            "what rejected correct nodes in the live run"
        )
        assert cone == ["Tablet_Preamble"]

    # And a node that genuinely DOES import it still gets it (no over-correction:
    # a real dependency must still be held warm, and its breakage is legitimately
    # this node's problem).
    (session_dir / "Tablet_GnpMeasure.thy").write_text(
        "theory Tablet_GnpMeasure\n  imports Tablet_Preamble Tablet_Connected\n"
        'begin\nlemma GnpMeasure: "True" by simp\nend\n',
        encoding="utf-8",
    )
    cone = wg._import_cone_theories(session_dir, theory="Tablet_GnpMeasure")
    assert "Tablet_Connected" in cone


def test_import_cone_survives_worker_authored_import_cycle(tmp_path: Path) -> None:
    """A cyclic import (Isabelle would reject it) must not hang the checker."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(
        session_dir,
        ["A", "B"],
        imports={"A": ["Tablet_B"], "B": ["Tablet_A"]},
    )
    cone = wg._import_cone_theories(session_dir, theory="Tablet_A")
    assert "Tablet_B" in cone
    assert "Tablet_A" not in cone


def test_gate_reconciles_then_checks_inflight_only(tmp_path: Path) -> None:
    """run_node_op reconciles the warm prefix (excluding the in-flight node),
    then the observation layer checks the in-flight node against it."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(
        session_dir, ["Dep", "InFlight"], imports={"InFlight": ["Tablet_Dep"]}
    )

    def responder(theory):
        return _ok_outcome(theory)

    warm = _StubSession(check_responder=responder)
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=responder, warm_prefix_enabled=False
        ),
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_check_node",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert used_cold is False
    assert payload["returncode"] == 0
    # reconcile saw the accepted set sans the in-flight node.
    assert warm.reconciled and warm.reconciled[-1][1] == ("Tablet_Preamble", "Tablet_Dep")
    # the check_node check elaborated the in-flight theory.
    assert "Tablet_InFlight" in warm.checked


# ============================ (Phase 3) warm advisory (inner loop) ============================


def _err_outcome(theory: str, *, errors) -> CheckOutcome:
    """A failing CheckOutcome (proof error), returncode != 0."""
    return CheckOutcome(
        ok=False,
        failed=1,
        finished=0,
        theory_name=f"Draft.{theory}",
        node_name=f"/tmp/{theory}.thy",
        error_lines=list(errors),
    )


def test_warm_advisory_passes_on_green_node_no_cert_no_crosscheck(
    tmp_path: Path, monkeypatch
) -> None:
    """run_warm_advisory reconciles the prefix then warm-checks ONLY the in-flight
    node, returning {ok, errors} — and NEVER runs a cert probe or cold cross-check
    (it is advisory, not the gate). Green node => ok=True, no errors."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(
        session_dir, ["Dep", "InFlight"], imports={"InFlight": ["Tablet_Dep"]}
    )

    def responder(theory):
        return _ok_outcome(theory)

    warm = _StubSession(check_responder=responder)
    cold_made = {"n": 0}

    def cold_factory():
        cold_made["n"] += 1
        return _StubSession(check_responder=responder, warm_prefix_enabled=False)

    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    # Avoid touching the worker Tablet/ sibling projection (none in the fixture).
    monkeypatch.setattr(wg.isabelle_scaffold, "sync_session", lambda _d: {})

    advisory = gate.run_warm_advisory(
        node_name="InFlight", theory="Tablet_InFlight", timeout_secs=60.0
    )
    assert advisory["ok"] is True
    assert advisory["errors"] == []
    assert advisory["advisory_unavailable"] is False
    # reconcile saw the accepted set sans the in-flight node.
    assert warm.reconciled and warm.reconciled[-1][1] == ("Tablet_Preamble", "Tablet_Dep")
    # ONLY the in-flight node elaborated (no `*__Cert` probe theory).
    assert warm.checked == ["Tablet_InFlight"]
    assert not any(t.endswith("__Cert") for t in warm.checked)
    # NO cold cross-check ran (cadence=1 would force one in run_node_op, but the
    # advisory MUST NOT trigger it) — so no cold session was spun up.
    assert cold_made["n"] == 0


def test_warm_advisory_reports_fail_with_errors_on_broken_node(
    tmp_path: Path, monkeypatch
) -> None:
    """A node that does not elaborate => ok=False with the `*** …` error lines
    surfaced (so the worker sees what to fix), advisory_unavailable=False."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    errs = ["*** Failed to finish proof", "*** At command \"by\" (line 4)"]

    def responder(theory):
        return _err_outcome(theory, errors=errs)

    warm = _StubSession(check_responder=responder)
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=responder, warm_prefix_enabled=False
        ),
    )
    monkeypatch.setattr(wg.isabelle_scaffold, "sync_session", lambda _d: {})

    advisory = gate.run_warm_advisory(
        node_name="InFlight", theory="Tablet_InFlight", timeout_secs=60.0
    )
    assert advisory["ok"] is False
    assert advisory["advisory_unavailable"] is False
    assert advisory["errors"] == errs


def test_warm_advisory_anomaly_is_unavailable_and_reaps(
    tmp_path: Path, monkeypatch
) -> None:
    """A warm-session anomaly during the advisory NEVER yields a green: it reaps
    the warm session (re-created next op) and returns advisory_unavailable=True so
    the worker falls back to `isabelle build`. It does NOT degrade to a cold cert
    (the advisory is not the gate)."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def boom(theory):
        raise IsabelleSessionError("protocol_error", "warm server died mid-check")

    warm = _StubSession(check_responder=boom)
    cold_made = {"n": 0}

    def cold_factory():
        cold_made["n"] += 1
        return _StubSession(check_responder=boom, warm_prefix_enabled=False)

    gate, _sd, _rt, held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    monkeypatch.setattr(wg.isabelle_scaffold, "sync_session", lambda _d: {})

    advisory = gate.run_warm_advisory(
        node_name="InFlight", theory="Tablet_InFlight", timeout_secs=60.0
    )
    assert advisory["ok"] is False
    assert advisory["advisory_unavailable"] is True
    assert advisory["errors"]  # carries the anomaly cause
    # Reaped the warm session (held handle nulled) → recreated next op.
    assert held["warm"] is None
    # The advisory did NOT spin up a cold session (no cold fallback — that is the
    # gate's job, not the inner loop).
    assert cold_made["n"] == 0


# ============================ (e) anomaly → graceful cold fallback ============================


def test_warm_anomaly_falls_back_to_cold(tmp_path: Path) -> None:
    """A warm-session anomaly (IsabelleSessionError) NEVER accepts on the broken
    warm session: it reaps the warm session and serves the verdict from a fresh
    cold session, which is recreated on the next op."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def warm_responder(theory):
        raise IsabelleSessionError("protocol_error", "warm server died mid-check")

    def cold_responder(theory):
        return _ok_outcome(theory, deps=["refl"])

    warm = _StubSession(check_responder=warm_responder)
    cold_sessions: List[_StubSession] = []

    def cold_factory():
        s = _StubSession(check_responder=cold_responder, warm_prefix_enabled=False)
        cold_sessions.append(s)
        return s

    gate, _sd, _rt, held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert used_cold is True
    # The cold session produced the verdict (status ok from the clean cold probe).
    assert payload["status"] == "ok"
    assert payload["kernel_axioms"] == ["refl"]
    # The warm session was reaped (held handle nulled) → recreated next op.
    assert held["warm"] is None
    # The cold session was used + closed.
    assert cold_sessions and cold_sessions[0].closed is True


def test_warm_anomaly_both_channels_fail_is_fail_closed(tmp_path: Path) -> None:
    """If BOTH the warm AND the cold channel fail, the gate fails CLOSED (an
    internal_error cert), never an accept."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def boom(theory):
        raise IsabelleSessionError("timed_out", "dead")

    warm = _StubSession(check_responder=boom)
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=boom, warm_prefix_enabled=False
        ),
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert used_cold is True
    assert payload["status"] == "internal_error"
    assert payload["oracles_used"] == []  # fail-closed: never a clean accept


# ============================ reconcile budget (tranche 4) ============================


class _ReconcileBoomSession(_StubSession):
    """A stub whose ``reconcile_accepted_base`` raises a scripted
    :class:`IsabelleSessionError` (the check responder is never reached).
    ``exc_type`` selects the raised type: the base class models a promote
    that timed out (or any transport error); the
    :class:`IsabelleReconcileBudgetExhausted` subclass models the
    pre-dispatch budget exhaustion (no in-flight task)."""

    def __init__(
        self,
        *,
        check_responder,
        kind: str,
        message: str,
        exc_type=IsabelleSessionError,
    ) -> None:
        super().__init__(check_responder=check_responder)
        self._boom_kind = kind
        self._boom_message = message
        self._boom_type = exc_type

    def reconcile_accepted_base(
        self, *, master_dir, accepted, timeout_secs=0.0, total_budget_secs=None
    ):
        self.reconcile_budgets.append(total_budget_secs)
        raise self._boom_type(self._boom_kind, self._boom_message)


def test_reconcile_receives_op_budget(tmp_path: Path) -> None:
    """run_node_op threads ITS OWN ``timeout_secs`` into the reconcile as the
    TOTAL promote budget — the fix for the 240 s session-START constant being
    misused as a per-member elaboration budget."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(
        session_dir, ["Dep", "InFlight"], imports={"InFlight": ["Tablet_Dep"]}
    )

    def responder(theory):
        return _ok_outcome(theory)

    warm = _StubSession(check_responder=responder)
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=responder, warm_prefix_enabled=False
        ),
    )
    gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=1234.5,
    )
    assert warm.reconcile_budgets == [1234.5]


def test_reconcile_budget_exceeded_serves_cold_without_reaping(
    tmp_path: Path, caplog
) -> None:
    """Case (a): a PRE-DISPATCH budget exhaustion
    (:class:`IsabelleReconcileBudgetExhausted` — no promote task in flight)
    is BUDGET-EXCEEDED, not a warm-session anomaly: the op is served via the
    existing cold path, but the warm session is NOT reaped (its
    ``_promoted`` map keeps whatever finished; the socket is idle) and the
    log says "reconcile budget exceeded", never "warm session anomaly"."""
    import logging

    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def cold_responder(theory):
        return _ok_outcome(theory, deps=["refl"])

    warm = _ReconcileBoomSession(
        check_responder=cold_responder,
        kind="timed_out",
        message="reconcile budget exhausted before promoting Tablet_Dep",
        exc_type=IsabelleReconcileBudgetExhausted,
    )
    cold_sessions: List[_StubSession] = []

    def cold_factory():
        s = _StubSession(check_responder=cold_responder, warm_prefix_enabled=False)
        cold_sessions.append(s)
        return s

    gate, _sd, _rt, held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    with caplog.at_level(
        logging.WARNING, logger="trellis.checker.isabelle_warm_gate"
    ):
        payload, used_cold = gate.run_node_op(
            op="isabelle_thm_deps",
            node_name="InFlight",
            theory="Tablet_InFlight",
            cert_theorem="InFlight",
            qualified_thm="Tablet_InFlight.InFlight",
            timeout_secs=60.0,
        )
    assert used_cold is True
    # The cold path served the verdict; the warm session never observed.
    assert payload["status"] == "ok"
    assert cold_sessions and cold_sessions[0].closed is True
    assert warm.checked == []
    # NOT reaped: the warm session (and its finished promotes) survives for
    # the next op.
    assert held["warm"] is warm
    # Logged distinctly: budget-exceeded, never an anomaly.
    log_text = caplog.text
    assert "reconcile budget exceeded" in log_text
    assert "warm session anomaly" not in log_text


def test_reconcile_inflight_promote_timeout_reaps_before_cold_serve(
    tmp_path: Path, caplog
) -> None:
    """Case (b): a member promote that itself timed out CLIENT-side (the
    plain ``timed_out`` base class — the server may still be elaborating the
    task on that socket) must REAP the warm session BEFORE the cold serve:
    a kept session with an in-flight task risks two concurrent Isabelle
    processes on shared heaps and a stale-reply socket desync. The op is
    still served cold, and the log names the in-flight case distinctly."""
    import logging

    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def cold_responder(theory):
        return _ok_outcome(theory, deps=["refl"])

    warm = _ReconcileBoomSession(
        check_responder=cold_responder,
        kind="timed_out",
        message="no reply from isabelle server within 480s",
        exc_type=IsabelleSessionError,
    )
    cold_sessions: List[_StubSession] = []

    def cold_factory():
        s = _StubSession(check_responder=cold_responder, warm_prefix_enabled=False)
        cold_sessions.append(s)
        return s

    gate, _sd, _rt, held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    with caplog.at_level(
        logging.WARNING, logger="trellis.checker.isabelle_warm_gate"
    ):
        payload, used_cold = gate.run_node_op(
            op="isabelle_thm_deps",
            node_name="InFlight",
            theory="Tablet_InFlight",
            cert_theorem="InFlight",
            qualified_thm="Tablet_InFlight.InFlight",
            timeout_secs=60.0,
        )
    assert used_cold is True
    assert payload["status"] == "ok"
    assert cold_sessions and cold_sessions[0].closed is True
    assert warm.checked == []
    # REAPED before the cold serve: the warm server may still hold an
    # in-flight elaboration on its socket.
    assert held["warm"] is None
    # Logged distinctly: the in-flight case, never the pre-dispatch
    # budget-kept case.
    log_text = caplog.text
    assert "timed out in-flight" in log_text
    assert "reconcile budget exceeded" not in log_text


def test_reconcile_protocol_error_still_reaps(tmp_path: Path, caplog) -> None:
    """A genuine transport/protocol error during reconcile keeps today's
    behaviour EXACTLY: reap the warm session and degrade to cold as a
    warm-session anomaly. Only ``timed_out`` is budget-exceeded."""
    import logging

    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def cold_responder(theory):
        return _ok_outcome(theory)

    warm = _ReconcileBoomSession(
        check_responder=cold_responder,
        kind="protocol_error",
        message="warm server wedged during promote",
    )
    gate, _sd, _rt, held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=cold_responder, warm_prefix_enabled=False
        ),
    )
    with caplog.at_level(
        logging.WARNING, logger="trellis.checker.isabelle_warm_gate"
    ):
        payload, used_cold = gate.run_node_op(
            op="isabelle_thm_deps",
            node_name="InFlight",
            theory="Tablet_InFlight",
            cert_theorem="InFlight",
            qualified_thm="Tablet_InFlight.InFlight",
            timeout_secs=60.0,
        )
    assert used_cold is True
    assert payload["status"] == "ok"
    # Reaped — a broken warm session is never kept.
    assert held["warm"] is None
    assert "warm session anomaly" in caplog.text
    assert "reconcile budget exceeded" not in caplog.text


# ============================ (c) backstop catches mismatch + HALTS ============================


def test_cross_check_halts_on_oracle_mismatch(tmp_path: Path) -> None:
    """THE backstop test: a forced warm/cold cross-check where the warm cert says
    CLEAN (no oracles) but the cold rebuild surfaces ``skip_proof`` — the gate
    must HALT (write the marker) and return the COLD cert, NOT accept the warm."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    # WARM: a false-green — clean oracles (a hypothetical warm-heap drift).
    def warm_responder(theory):
        return _ok_outcome(theory, oracles=[], deps=["refl"])

    # COLD: the authoritative rebuild surfaces the skip_proof oracle (sorry).
    def cold_responder(theory):
        return _ok_outcome(theory, oracles=["skip_proof"], deps=["refl"])

    warm = _StubSession(check_responder=warm_responder)
    gate, _sd, rt, held, _rebuilt = _make_gate(
        tmp_path,
        # force the cross-check on this very check (cadence-independent).
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=cold_responder, warm_prefix_enabled=False
        ),
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    # The HALT marker was written into the runtime root, where the supervisor
    # Run loop + the bridge poll it.
    marker = rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME
    assert marker.exists(), "warm/cold mismatch MUST write the halt marker"
    doc = json.loads(marker.read_text())
    assert doc["kind"] == "checker_disagreement"
    assert doc["source"] == "isabelle_warm_cold_cross_check"
    assert doc["active_node"] == "InFlight"
    assert any("oracles_used" in d for d in doc["diffs"])
    assert "rm " in doc["clear_instructions"]
    # The returned verdict is the COLD cert (skip_proof present) — the warm
    # false-green is NOT accepted.
    assert payload["oracles_used"] == ["skip_proof"]
    # And the warm session was reaped for the one-process cold cross-check.
    assert held["warm"] is None


def test_cross_check_no_halt_when_active_node_clean_with_sibling_sorries(
    tmp_path: Path,
) -> None:
    """THE bug regression: the active node is a CLEAN closed node, but the tablet
    also holds an UNRELATED sibling lemma carrying ``sorry`` (legitimate at a
    mid-formalization phase, e.g. TheoremStating). The NODE-SCOPED cold recompute
    elaborates only the active node + its import cone, which does NOT include the
    sibling ``sorry``, so the cold cert matches the warm cert and the cross-check
    AGREES — no halt marker.

    Before the fix the cross-check ran a WHOLE-SESSION ``isabelle build`` for the
    cold side, which went red on the unrelated ``sorry`` and spuriously HALTed a
    live run (comparing a single-node warm verdict to a whole-tablet cold build).
    """
    session_dir = tmp_path / "isabelle"
    # ``InFlight`` is the clean active node; ``Sibling`` is an unrelated sibling
    # lemma. We give the sibling an actual ``sorry`` body so it is visibly the
    # kind of theory a whole-session build would choke on — but the cold recompute
    # is node-scoped and never elaborates it.
    _write_session_scaffold(session_dir, ["InFlight"])
    (session_dir / "Tablet_Sibling.thy").write_text(
        "theory Tablet_Sibling\n  imports Tablet_Preamble\n"
        'begin\nlemma sibling_open: "True" sorry\nend\n',
        encoding="utf-8",
    )

    # BOTH warm and the node-scoped cold recompute see the SAME clean active-node
    # cert (the active node is genuinely closed); the responder only ever fields
    # the active node + its probe, never the sibling.
    def clean_active(theory):
        assert theory in ("Tablet_InFlight",) or theory.startswith(
            "Tablet_InFlight__Cert"
        ), f"the node-scoped cold recompute must not elaborate {theory!r}"
        return _ok_outcome(theory, oracles=[], deps=["refl"])

    warm = _StubSession(check_responder=clean_active)
    cold_made = {"n": 0}

    def cold_factory():
        cold_made["n"] += 1
        return _StubSession(check_responder=clean_active, warm_prefix_enabled=False)

    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    # The cold cross-check ran (a fresh cold session was made) and AGREED — NO halt.
    assert cold_made["n"] == 1, "the cadence cross-check must still run"
    assert not (rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists(), (
        "a clean active node with an unrelated sibling sorry MUST NOT halt — the "
        "node-scoped cold recompute does not elaborate the sibling"
    )
    assert used_cold is False
    assert payload["status"] == "ok"
    assert payload["oracles_used"] == []


def test_cross_check_halts_when_active_node_red_cold(tmp_path: Path) -> None:
    """The backstop still bites: the warm gate accepts the active node CLEAN, but
    the authoritative NODE-SCOPED cold recompute finds the SAME active node does
    NOT build clean (a hypothetical warm-heap stale-clean drift). The cold proof
    failure surfaces as ``status == invalid_proof`` in the cold cert, which the
    cert-field comparison catches → HALT + prefer cold."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def warm_clean(theory):
        return _ok_outcome(theory, oracles=[], deps=["refl"])

    # COLD: the worker node theory itself does not build clean (returncode != 0) —
    # run_cert_probe raises _ProbeWorkerFailed, yielding an invalid_proof cert.
    def cold_red(theory):
        return _err_outcome(theory, errors=["*** Failed to finish proof"])

    warm = _StubSession(check_responder=warm_clean)
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=cold_red, warm_prefix_enabled=False
        ),
    )
    payload, _used = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    marker = rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME
    assert marker.exists(), "a real warm-clean vs cold-red active node MUST halt"
    doc = json.loads(marker.read_text())
    # The divergence shows up as a status (ok vs invalid_proof) cert-field diff.
    assert any("status" in d for d in doc["diffs"]), doc["diffs"]
    # And the returned verdict is the COLD (invalid_proof) cert — warm not accepted.
    assert payload["status"] == "invalid_proof"


def _sorry_outcome(theory: str, *, deps=None, stmt="P x = Q x") -> CheckOutcome:
    """A built-ok outcome whose theorem checked WITH a ``skip_proof`` oracle.

    Maps to ``status=ok, theorem_exists=True, oracles_used=['skip_proof']`` — the
    warm gate REJECTS it (an oracle is outside the empty floor), so it is NOT a
    clean closure.
    """
    return _ok_outcome(theory, oracles=["skip_proof"], deps=deps, stmt=stmt)


def _theorem_absent_outcome(theory: str) -> CheckOutcome:
    """A built-ok outcome whose principal theorem is ABSENT (oops/elision).

    Maps to ``status=internal_error, theorem_exists=False`` — NOT a clean
    closure (the warm gate rejects it).
    """
    return CheckOutcome(
        ok=True,
        failed=0,
        finished=10,
        theory_name=f"Draft.{theory}",
        node_name=f"/tmp/{theory}.thy",
        oracles=[],
        dependencies=["refl"],
        theorem_exists=False,
        extra_shyps=[],
        statement_repr="",
        statement_hash="",
        statement_repr_long="",
    )


def test_cross_check_no_halt_when_warm_sorry_and_cold_invalid(tmp_path: Path) -> None:
    """THE live regression (node ``conn``): the WARM verdict is NOT a clean
    closure — it found the theorem but with a ``skip_proof`` oracle (``status=ok,
    theorem_exists=True, oracles_used=['skip_proof']``), which the warm gate
    REJECTS. The cold recompute also rejects but diverges on the detail
    (``status=invalid_proof, theorem_exists=False, oracles_used=[]`` — its cone
    did not build). BOTH reject; there is no warm false-accept → the gate must NOT
    write the halt marker."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    # WARM: found the theorem but under `sorry` → skip_proof oracle (NOT closed).
    def warm_sorry(theory):
        return _sorry_outcome(theory, deps=["refl"])

    # COLD: the node-scoped cone did not build → invalid_proof, theorem absent.
    def cold_invalid(theory):
        return _err_outcome(theory, errors=["*** Failed to load theory cone"])

    warm = _StubSession(check_responder=warm_sorry)
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=cold_invalid, warm_prefix_enabled=False
        ),
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert not (rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists(), (
        "warm `skip_proof` (non-closing) vs cold `invalid_proof` is two rejections "
        "disagreeing on detail — NOT a false-accept — and MUST NOT halt"
    )
    # The warm verdict (already non-accepting: carries skip_proof) stands.
    assert payload["oracles_used"] == ["skip_proof"]
    assert payload["status"] == "ok"  # ok+skip_proof = the kernel still rejects it


def test_cross_check_no_halt_when_warm_theorem_absent_and_cold_differs(
    tmp_path: Path,
) -> None:
    """The WARM verdict is NOT a clean closure because the principal theorem is
    ABSENT (``status=internal_error, theorem_exists=False``) — the warm gate
    rejects it. The cold recompute differs (a clean ``ok`` cert, or any other
    detail). No warm false-accept → no halt."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    # WARM: theorem absent (oops/elision) → theorem_exists=False (NOT closed).
    def warm_absent(theory):
        return _theorem_absent_outcome(theory)

    # COLD: a different (here: clean) verdict — diverges from the warm one.
    def cold_clean(theory):
        return _ok_outcome(theory, oracles=[], deps=["refl"])

    warm = _StubSession(check_responder=warm_absent)
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=cold_clean, warm_prefix_enabled=False
        ),
    )
    payload, _used = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert not (rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists(), (
        "a warm verdict with theorem_exists=False is already rejecting — a "
        "divergence from cold is benign and MUST NOT halt"
    )
    # The warm (non-accepting) verdict stands — never accepted as closed.
    assert payload["theorem_exists"] is False
    assert payload["status"] == "internal_error"


def test_cross_check_no_halt_when_warm_oracles_op_not_clean(tmp_path: Path) -> None:
    """The same semantic gate on the ``isabelle_thm_oracles`` op (stdout-envelope
    wire shape): the WARM surface is NOT a clean closure (it carries a
    ``skip_proof`` oracle / non-zero returncode), so a warm-vs-cold surface
    divergence is benign — no halt."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    # WARM: the probe surfaced a skip_proof oracle (NOT a clean closure surface).
    def warm_sorry(theory):
        return _sorry_outcome(theory, deps=["refl"])

    # COLD: a clean surface (no oracle) — diverges from the warm one.
    def cold_clean(theory):
        return _ok_outcome(theory, oracles=[], deps=["refl"])

    warm = _StubSession(check_responder=warm_sorry)
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=cold_clean, warm_prefix_enabled=False
        ),
    )
    gate.run_node_op(
        op="isabelle_thm_oracles",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert not (rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists(), (
        "warm thm_oracles surface carrying skip_proof is non-closing — a surface "
        "divergence from cold MUST NOT halt"
    )


def test_warm_clean_closure_predicate_units() -> None:
    """The closure-clean predicate (the kernel's `isabelle_cert_gate_violation`
    +`status==ok` port) classifies certs as the kernel accept-gate would."""
    clean = {
        "status": "ok",
        "oracles_used": [],
        "extra_shyps": [],
        "theorem_exists": True,
    }
    assert wg._cert_is_clean_closure(clean) is True
    # skip_proof oracle → NOT clean (outside the empty floor).
    assert wg._cert_is_clean_closure(dict(clean, oracles_used=["skip_proof"])) is False
    # any solver oracle → NOT clean.
    assert wg._cert_is_clean_closure(dict(clean, oracles_used=["z3"])) is False
    # non-ok status → NOT clean.
    assert wg._cert_is_clean_closure(dict(clean, status="invalid_proof")) is False
    # theorem absent → NOT clean.
    assert wg._cert_is_clean_closure(dict(clean, theorem_exists=False)) is False
    # dangling shyps → NOT clean.
    assert wg._cert_is_clean_closure(dict(clean, extra_shyps=["'a::foo"])) is False
    # The floor is empty (mirrors ISABELLE_HOL_APPROVED_ORACLES_FLOOR).
    assert wg.ISABELLE_APPROVED_ORACLES_FLOOR == frozenset()


def test_cross_check_passes_when_warm_equals_cold(tmp_path: Path) -> None:
    """The happy backstop path: warm cert == cold cert → no marker, the warm
    verdict stands (this is the normal, Phase-0-proven case)."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def same(theory):
        return _ok_outcome(theory, oracles=[], deps=["refl", "One_nat_def"])

    warm = _StubSession(check_responder=same)
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=same, warm_prefix_enabled=False
        ),
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert not (rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists()
    assert used_cold is False
    assert payload["status"] == "ok"
    assert payload["oracles_used"] == []


def test_cross_check_cold_channel_failure_does_not_halt(tmp_path: Path) -> None:
    """A cold-CHANNEL failure during the cross-check (the cold session/build
    can't spawn) is infrastructure flakiness, NOT a disagreement — it must NOT
    halt; the warm verdict stands and a marker is NOT written."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def warm_clean(theory):
        return _ok_outcome(theory, oracles=[])

    # The cold session factory itself fails (spawn anomaly).
    def cold_factory():
        raise IsabelleSessionError("spawn_failed", "no isabelle binary in this env")

    warm = _StubSession(check_responder=warm_clean)
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    # NO halt marker (infrastructure failure, not a closure hole).
    assert not (rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists()
    # The warm verdict (clean) stands.
    assert payload["status"] == "ok"
    assert payload["oracles_used"] == []


def test_cross_check_cold_probe_timeout_does_not_halt(tmp_path: Path) -> None:
    """A cold-probe transport TIMEOUT (infrastructure) is not a disagreement → no
    halt. The cold recompute's session/probe timing out is a cold-CHANNEL failure
    (``run_cert_probe`` raises an ``IsabelleSessionError`` of kind ``timed_out``),
    distinct from a genuine cold proof verdict — the warm verdict stands."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def warm_clean(theory):
        return _ok_outcome(theory, oracles=[])

    # The cold probe's worker check times out on the wire (a transport anomaly,
    # not a proof failure) → run_cert_probe re-raises it → cold-channel failure.
    def cold_timeout(theory):
        raise IsabelleSessionError("timed_out", "isabelle probe timed out")

    warm = _StubSession(check_responder=warm_clean)
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=cold_timeout, warm_prefix_enabled=False
        ),
    )
    payload, _used = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert not (rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists()
    assert payload["status"] == "ok"


def test_cross_check_statement_hash_mismatch_halts(tmp_path: Path) -> None:
    """A statement-hash divergence (the warm heap elaborated a DIFFERENT theorem)
    is the canonical false-green channel — it must HALT."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t, stmt="P x = Q x"))
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=lambda t: _ok_outcome(t, stmt="P x = WRONG x"),
            warm_prefix_enabled=False,
        ),
    )
    gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    marker = rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME
    assert marker.exists()
    doc = json.loads(marker.read_text())
    assert any("statement_hash" in d for d in doc["diffs"])


def test_marker_write_once_preserves_first(tmp_path: Path) -> None:
    """A second mismatch does NOT overwrite the first marker (the first is the
    load-bearing diagnostic; mirrors the kernel's write-once discipline)."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])
    rt = tmp_path / "rt"
    rt.mkdir(parents=True, exist_ok=True)
    marker = rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME
    marker.write_text(json.dumps({"kind": "checker_disagreement", "active_node": "FIRST"}))

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t, oracles=[]))
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=lambda t: _ok_outcome(t, oracles=["skip_proof"]),
            warm_prefix_enabled=False,
        ),
        runtime_root=rt,
    )
    gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="SECOND",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    doc = json.loads(marker.read_text())
    assert doc["active_node"] == "FIRST", "first diagnostic must be preserved"


# ============================ cadence behavior ============================


def test_cadence_only_cross_checks_every_nth(tmp_path: Path) -> None:
    """With cadence N, only every Nth soundness check runs the (expensive) cold
    cross-check; the others take the pure warm path."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])
    cold_calls = {"n": 0}

    def cold_factory():
        cold_calls["n"] += 1
        return _StubSession(
            check_responder=lambda t: _ok_outcome(t), warm_prefix_enabled=False
        )

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t))
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=3),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    for _ in range(6):
        gate.run_node_op(
            op="isabelle_thm_deps",
            node_name="InFlight",
            theory="Tablet_InFlight",
            cert_theorem="InFlight",
            qualified_thm="Tablet_InFlight.InFlight",
            timeout_secs=60.0,
        )
    # 6 checks, cadence 3 → cold cross-check fired exactly twice (checks 3 and 6).
    assert cold_calls["n"] == 2


def test_check_node_op_never_drives_cross_check(tmp_path: Path) -> None:
    """``isabelle_check_node`` (build-only, no cert) does NOT trigger the cadence
    cross-check even at cadence 1 — only the cert ops do."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])
    cold_calls = {"n": 0}

    def cold_factory():
        cold_calls["n"] += 1
        return _StubSession(
            check_responder=lambda t: _ok_outcome(t), warm_prefix_enabled=False
        )

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t))
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    gate.run_node_op(
        op="isabelle_check_node",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert cold_calls["n"] == 0


def test_forced_cross_check_env_overrides_cadence(tmp_path: Path, monkeypatch) -> None:
    """The forced-cross-check env switch (the completion stand-in) triggers a
    cold cross-check even when the periodic cadence is disabled (0)."""
    from trellis.checker import isabelle_warm_config as cfg_mod

    monkeypatch.setenv(cfg_mod.FORCE_CROSS_CHECK_ENV, "1")
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])
    cold_calls = {"n": 0}

    def cold_factory():
        cold_calls["n"] += 1
        return _StubSession(
            check_responder=lambda t: _ok_outcome(t), warm_prefix_enabled=False
        )

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t))
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert cold_calls["n"] == 1


# ============================ cert-comparison unit tests ============================


def test_cert_field_diffs_set_order_insensitive() -> None:
    """Oracle/dep/shyps lists compare as SETS (observation order is not load-
    order-stable) so a mere reordering is NOT a spurious halt."""
    warm = {
        "status": "ok",
        "oracles_used": [],
        "kernel_axioms": ["refl", "One_nat_def"],
        "extra_shyps": [],
        "statement_hash": "h",
        "statement_repr": "s",
        "statement_repr_long": "s",
        "theorem_exists": True,
    }
    cold = dict(warm, kernel_axioms=["One_nat_def", "refl"])  # reordered
    assert wg._cert_field_diffs(warm, cold) == []


def test_cert_field_diffs_detects_real_divergence() -> None:
    warm = {
        "status": "ok",
        "oracles_used": [],
        "kernel_axioms": ["refl"],
        "extra_shyps": [],
        "statement_hash": "h1",
        "statement_repr": "s",
        "statement_repr_long": "s",
        "theorem_exists": True,
    }
    cold = dict(warm, oracles_used=["skip_proof"], statement_hash="h2")
    diffs = wg._cert_field_diffs(warm, cold)
    assert any("oracles_used" in d for d in diffs)
    assert any("statement_hash" in d for d in diffs)


def test_oracle_surface_diffs_parses_stdout() -> None:
    warm_env = iso.external_command_envelope(
        returncode=0, stdout="oracles: \ndepends on axioms: refl One_nat_def"
    )
    cold_cert = {
        "oracles_used": [],
        "kernel_axioms": ["One_nat_def", "refl"],
        "extra_shyps": [],
        "returncode": 0,
    }
    assert wg._oracle_surface_diffs(warm_env, cold_cert) == []
    # A skip_proof only on the cold side is a diff.
    cold_taint = dict(cold_cert, oracles_used=["skip_proof"])
    assert any("oracles" in d for d in wg._oracle_surface_diffs(warm_env, cold_taint))


# ============================ H1: purge-before in-flight re-check ============================


def test_gate_does_not_purge_inflight_after_check(tmp_path: Path) -> None:
    """REGRESSION: the gate must NOT purge the in-flight node at all.

    It used to, to keep the held-open document residue-free. On Isabelle 2025-2
    the public `purge_theories` commits the resource-state removal but DISCARDS
    the document deletion edits (`src/Pure/PIDE/headless.scala`; the internal
    `clean_theories` is the path that applies them). The resource state then
    reports the theory absent while the PIDE document still holds its text, so
    the next load of that name INSERTS a second copy rather than replacing it,
    the second `theory ... begin` hits `illegal_init`, and every following
    command reports "missing theory context".

    Reproduced deterministically before the fix: an eight-node batch checked in
    the live order returned rc 0,0,0,0,1,0,1,0, and the following round failed
    exactly the six nodes whose import cone touched a previously purged name
    while the two retained predecessors passed.

    Freshness — the reason the purge existed — comes from the content-keyed
    `__In_<sha>` alias instead, so there is no same-name reload to corrupt."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(
        session_dir, ["Dep", "InFlight"], imports={"InFlight": ["Tablet_Dep"]}
    )

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t))
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=lambda t: _ok_outcome(t), warm_prefix_enabled=False
        ),
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert used_cold is False
    assert payload["status"] == "ok"
    # NOTHING is purged — see the regression note above.
    assert warm.purged == []
    assert warm.checked.index("Tablet_InFlight") < len(warm.checked)
    assert "Tablet_Dep" not in warm.purged
    # The reconcile ran (warm prefix kept current).
    assert warm.reconciled and warm.reconciled[-1][1] == ("Tablet_Preamble", "Tablet_Dep")


def test_h1_cold_fallback_session_not_purged(tmp_path: Path) -> None:
    """H1: a fresh COLD session (the anomaly fallback / cross-check channel,
    ``warm_prefix_enabled=False``) is NEVER purged — the after-purge guard skips
    a non-warm session (it has nothing resident and cannot purge)."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def warm_anomaly(theory):
        raise IsabelleSessionError("protocol_error", "warm died")

    cold_sessions: List[_StubSession] = []

    def cold_factory():
        s = _StubSession(
            check_responder=lambda t: _ok_outcome(t, deps=["refl"]),
            warm_prefix_enabled=False,
        )
        cold_sessions.append(s)
        return s

    warm = _StubSession(check_responder=warm_anomaly)
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    payload, used_cold = gate.run_node_op(
        op="isabelle_thm_deps",
        node_name="InFlight",
        theory="Tablet_InFlight",
        cert_theorem="InFlight",
        qualified_thm="Tablet_InFlight.InFlight",
        timeout_secs=60.0,
    )
    assert used_cold is True
    assert payload["status"] == "ok"
    # The cold session ran the check but was never purged (it cannot be).
    assert cold_sessions and cold_sessions[0].purged == []


def test_warm_advisory_does_not_purge_after(tmp_path: Path, monkeypatch) -> None:
    """H1: the worker advisory also purges the in-flight node AFTER its check, so
    repeated advisory checks on the same node do not collide with its residue."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])
    monkeypatch.setattr(wg.isabelle_scaffold, "sync_session", lambda _d: {})

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t))
    gate, _sd, _rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=0),
        warm_session=warm,
        cold_session_factory=lambda: _StubSession(
            check_responder=lambda t: _ok_outcome(t), warm_prefix_enabled=False
        ),
    )
    result = gate.run_warm_advisory(
        node_name="InFlight", theory="Tablet_InFlight", timeout_secs=60.0
    )
    assert result["ok"] is True
    assert result["advisory_unavailable"] is False
    assert warm.purged == []


def test_h1_session_purge_inflight_targets_node_only(tmp_path: Path) -> None:
    """H1 (unit): ``IsabelleSession.purge_inflight`` purges ONLY the in-flight
    node theory via ``purge_theories`` (the cert probe is unique-named + never
    purged), without touching ``_promoted``."""
    from trellis.checker.isabelle_session import IsabelleSession

    sent: List[Tuple[str, Dict[str, Any]]] = []

    class _WireSession(IsabelleSession):
        def __init__(self):
            super().__init__(name="purge-unit-test", warm_prefix_enabled=True)
            self._session_id = "sid"
            self._sock = object()  # satisfy the lifecycle guard

        def _send(self, command, payload):  # capture, do not touch a real socket
            sent.append((command, dict(payload)))

        def _await_terminal(self, *, timeout_secs):
            from trellis.checker.isabelle_session import IsabelleReply

            return IsabelleReply(kind="FINISHED", payload={}, raw_tail="")

        def _await_sync_reply(self, *, timeout_secs):
            # purge_theories' synchronous OK reply (no real socket here).
            from trellis.checker.isabelle_session import IsabelleReply

            return IsabelleReply(
                kind="OK", payload={"purged": [], "retained": []}, raw_tail=""
            )

    s = _WireSession()
    s._promoted["Tablet_Dep"] = "deadbeef"  # an accepted sibling stays put
    s.purge_inflight(master_dir=str(tmp_path), theory="Tablet_InFlight")
    assert len(sent) == 1 and sent[0][0] == "purge_theories"
    purged = sent[0][1]["theories"]
    # Only the in-flight node (no fixed-name probe).
    assert purged == ["Tablet_InFlight"]
    # The accepted-sibling prefix is untouched.
    assert s._promoted == {"Tablet_Dep": "deadbeef"}


# ============================ H2: consecutive-cold-failure alarm ============================


def test_h2_alarm_fires_after_consecutive_cold_failures(tmp_path: Path) -> None:
    """H2: after the configured number of CONSECUTIVE cold-channel failures, the
    gate writes the NON-halting degraded marker (visibility) — but never the
    HALT marker (a channel failure is not a disagreement)."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    # Cold session can never spawn → every cross-check is a cold-channel failure.
    def cold_factory():
        raise IsabelleSessionError("spawn_failed", "no isabelle here")

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t, oracles=[]))
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(
            enabled=True, cross_check_cadence=1, cold_failure_alarm_after=3
        ),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    degraded = rt / wg.COLD_CROSSCHECK_DEGRADED_MARKER_FILENAME
    halt = rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME

    def _run():
        return gate.run_node_op(
            op="isabelle_thm_deps",
            node_name="InFlight",
            theory="Tablet_InFlight",
            cert_theorem="InFlight",
            qualified_thm="Tablet_InFlight.InFlight",
            timeout_secs=60.0,
        )

    _run()  # 1 consecutive failure — below threshold, no marker yet
    assert not degraded.exists()
    _run()  # 2 — still below
    assert not degraded.exists()
    payload, _used = _run()  # 3 — threshold reached → degraded marker written
    assert degraded.exists(), "the H2 degraded marker must be written at the threshold"
    doc = json.loads(degraded.read_text())
    assert doc["kind"] == "cold_crosscheck_degraded"
    assert doc["consecutive_cold_channel_failures"] == 3
    # NEVER the halt marker — a cold-channel failure is not a disagreement.
    assert not halt.exists()
    # The warm verdict still stands (the spine cold build the kernel runs is the
    # real soundness gate; the gate's cross-check is degraded but not unsound).
    assert payload["status"] == "ok"


def test_h2_successful_crosscheck_resets_and_clears_marker(tmp_path: Path) -> None:
    """H2: a cold cross-check that actually RUNS resets the consecutive-failure
    counter and clears the degraded marker — the tier is operational again."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    cold_ok = {"on": False}

    def cold_factory():
        if not cold_ok["on"]:
            raise IsabelleSessionError("spawn_failed", "cold down")
        return _StubSession(
            check_responder=lambda t: _ok_outcome(t, oracles=[]),
            warm_prefix_enabled=False,
        )

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t, oracles=[]))
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(
            enabled=True, cross_check_cadence=1, cold_failure_alarm_after=2
        ),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    degraded = rt / wg.COLD_CROSSCHECK_DEGRADED_MARKER_FILENAME

    def _run():
        return gate.run_node_op(
            op="isabelle_thm_deps",
            node_name="InFlight",
            theory="Tablet_InFlight",
            cert_theorem="InFlight",
            qualified_thm="Tablet_InFlight.InFlight",
            timeout_secs=60.0,
        )

    _run()
    _run()  # threshold 2 reached → marker present
    assert degraded.exists()
    # Now the cold channel recovers; the next cross-check runs + agrees → reset.
    cold_ok["on"] = True
    _run()
    assert not degraded.exists(), "a successful cross-check must clear the marker"
    assert gate._consecutive_cold_failures == 0


def test_h2_alarm_disabled_when_threshold_zero(tmp_path: Path) -> None:
    """H2: ``cold_failure_alarm_after=0`` disables the alarm (no marker ever)."""
    session_dir = tmp_path / "isabelle"
    _write_session_scaffold(session_dir, ["InFlight"])

    def cold_factory():
        raise IsabelleSessionError("spawn_failed", "no isabelle here")

    warm = _StubSession(check_responder=lambda t: _ok_outcome(t, oracles=[]))
    gate, _sd, rt, _held, _rebuilt = _make_gate(
        tmp_path,
        config=IsabelleWarmSessionConfig(
            enabled=True, cross_check_cadence=1, cold_failure_alarm_after=0
        ),
        warm_session=warm,
        cold_session_factory=cold_factory,
    )
    for _ in range(5):
        gate.run_node_op(
            op="isabelle_thm_deps",
            node_name="InFlight",
            theory="Tablet_InFlight",
            cert_theorem="InFlight",
            qualified_thm="Tablet_InFlight.InFlight",
            timeout_secs=60.0,
        )
    assert not (rt / wg.COLD_CROSSCHECK_DEGRADED_MARKER_FILENAME).exists()


# ============================ (a) flag-OFF dispatcher no-op ============================


@pytest.fixture
def dispatcher_runtime(tmp_path: Path):
    """A CheckerServer over a tmp runtime root with an Isabelle scaffold node."""
    import tempfile

    base = Path(tempfile.mkdtemp(prefix="isa-warm-gate-", dir=str(tmp_path)))
    repo = base / "r"
    runtime = repo / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    yield runtime, repo


def test_flag_off_dispatcher_never_enters_gate(dispatcher_runtime, monkeypatch) -> None:
    """(a) THE no-op proof: with the flag OFF (the default), the dispatcher takes
    the cold path and NEVER constructs/enters the warm gate. A spy on the gate
    accessor asserts it is not called; the cold session path is used unchanged."""
    from trellis.checker.protocol import CheckerRequest
    from trellis.checker.server import CheckerServer

    runtime, repo = dispatcher_runtime
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    # Scaffold a node theory so the handler passes the scaffold-present gate.
    session_dir = server._isabelle_session_dir()
    _write_session_scaffold(session_dir, ["NodeA"])

    # Config defaults OFF (no trellis.config.json present).
    assert server._isabelle_warm_cfg().enabled is False

    gate_calls = {"n": 0}

    def _spy_gate():
        gate_calls["n"] += 1
        raise AssertionError("gate must not be constructed when flag is OFF")

    monkeypatch.setattr(server, "_get_or_create_isabelle_warm_gate", _spy_gate)
    monkeypatch.setattr(server, "_handle_isabelle_node_op_warm", _spy_gate)

    # The cold path calls _get_or_create_isabelle_session; stub it so no real
    # isabelle spawns, and assert it IS used (the cold path is taken).
    cold_used = {"n": 0}

    class _ColdStub:
        def check_theory(self, **kw):
            cold_used["n"] += 1
            return _ok_outcome(kw["theory"])

    monkeypatch.setattr(server, "_get_or_create_isabelle_session", lambda: _ColdStub())

    req = CheckerRequest(
        op="isabelle_check_node", request_id=1, node_name="NodeA", timeout_secs=60.0
    )
    response, rc, log_extra = server._handle_isabelle_node_op(req)
    assert gate_calls["n"] == 0, "flag-OFF must never enter the warm gate"
    assert cold_used["n"] >= 1, "flag-OFF must take the cold node-check path"
    assert response["returncode"] == 0
    assert "isabelle_warm_gate" not in log_extra


def test_flag_on_dispatcher_routes_through_gate(dispatcher_runtime, monkeypatch) -> None:
    """Symmetric to the no-op: flag ON routes through the warm gate (the gate
    accessor IS called and the warm-gate log field appears)."""
    from trellis.checker.protocol import CheckerRequest
    from trellis.checker.server import CheckerServer

    runtime, repo = dispatcher_runtime
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    session_dir = server._isabelle_session_dir()
    _write_session_scaffold(session_dir, ["NodeA"])

    # Force the flag ON via the env switch (no config file needed).
    from trellis.checker import isabelle_warm_config as cfg_mod

    monkeypatch.setenv(cfg_mod.WARM_SESSION_ENV, "1")
    server._isabelle_warm_config = None  # re-resolve with the env on
    assert server._isabelle_warm_cfg().enabled is True

    # Stub the gate so no real session/build happens.
    class _FakeGate:
        def run_node_op(self, **kw):
            return ({"returncode": 0, "stdout": "", "stderr": "", "timed_out": False,
                     "spawn_error": ""}, False)

    monkeypatch.setattr(server, "_get_or_create_isabelle_warm_gate", lambda: _FakeGate())

    req = CheckerRequest(
        op="isabelle_check_node", request_id=2, node_name="NodeA", timeout_secs=60.0
    )
    response, rc, log_extra = server._handle_isabelle_node_op(req)
    assert log_extra.get("isabelle_warm_gate") is True
    assert log_extra.get("isabelle_cold_fallback") is False
    assert response["returncode"] == 0


def test_flag_on_gate_construction_failure_falls_back_to_cold(
    dispatcher_runtime, monkeypatch
) -> None:
    """Defensive: if the gate cannot even be constructed (flag ON), the handler
    falls back to the proven cold path rather than dropping the check."""
    from trellis.checker.protocol import CheckerRequest
    from trellis.checker.server import CheckerServer

    runtime, repo = dispatcher_runtime
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    session_dir = server._isabelle_session_dir()
    _write_session_scaffold(session_dir, ["NodeA"])

    from trellis.checker import isabelle_warm_config as cfg_mod

    monkeypatch.setenv(cfg_mod.WARM_SESSION_ENV, "1")
    server._isabelle_warm_config = None

    def _boom_gate():
        raise RuntimeError("gate import exploded")

    monkeypatch.setattr(server, "_get_or_create_isabelle_warm_gate", _boom_gate)

    cold_used = {"n": 0}

    class _ColdStub:
        def check_theory(self, **kw):
            cold_used["n"] += 1
            return _ok_outcome(kw["theory"])

    monkeypatch.setattr(server, "_get_or_create_isabelle_session", lambda: _ColdStub())

    req = CheckerRequest(
        op="isabelle_check_node", request_id=3, node_name="NodeA", timeout_secs=60.0
    )
    response, rc, log_extra = server._handle_isabelle_node_op(req)
    # Fell back to cold (no warm-gate log field, the cold session was used).
    assert cold_used["n"] >= 1
    assert "isabelle_warm_gate" not in log_extra


# ============================ (Phase 3) warm-advisory dispatcher ============================


def test_warm_advisory_dispatcher_inert_when_flag_off(
    dispatcher_runtime, monkeypatch
) -> None:
    """flag OFF: the isabelle_warm_advisory op is structurally unavailable — it
    NEVER constructs the warm gate or spawns a session, returns
    advisory_unavailable=True (so the worker uses `isabelle build`)."""
    from trellis.checker.protocol import CheckerRequest
    from trellis.checker.server import CheckerServer

    runtime, repo = dispatcher_runtime
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _write_session_scaffold(server._isabelle_session_dir(), ["NodeA"])
    assert server._isabelle_warm_cfg().enabled is False

    def _spy(*_a, **_k):
        raise AssertionError("flag-OFF advisory must not construct the gate")

    monkeypatch.setattr(server, "_get_or_create_isabelle_warm_gate", _spy)
    monkeypatch.setattr(server, "_get_or_create_isabelle_session", _spy)

    req = CheckerRequest(
        op="isabelle_warm_advisory", request_id=7, node_name="NodeA", timeout_secs=60.0
    )
    response, rc, log_extra = server._handle_isabelle_warm_advisory(req)
    assert response["advisory_unavailable"] is True
    assert response["ok"] is False
    assert rc is None
    assert log_extra.get("isabelle_warm_advisory_disabled") is True


def test_warm_advisory_dispatcher_routes_to_gate_when_flag_on(
    dispatcher_runtime, monkeypatch
) -> None:
    """flag ON: the op routes to the gate's run_warm_advisory and surfaces its
    {ok, errors} verdict (rc 0 on green) — NOT through run_node_op (no cert)."""
    from trellis.checker.protocol import CheckerRequest
    from trellis.checker.server import CheckerServer
    from trellis.checker import isabelle_warm_config as cfg_mod

    runtime, repo = dispatcher_runtime
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())
    _write_session_scaffold(server._isabelle_session_dir(), ["NodeA"])

    monkeypatch.setenv(cfg_mod.WARM_SESSION_ENV, "1")
    server._isabelle_warm_config = None
    assert server._isabelle_warm_cfg().enabled is True

    seen = {}

    class _FakeGate:
        def run_warm_advisory(self, *, node_name, theory, timeout_secs):
            seen["node"] = node_name
            seen["theory"] = theory
            return {"ok": True, "seconds": 0.2, "errors": [], "advisory_unavailable": False}

        def run_node_op(self, **_k):  # must NOT be called by the advisory
            raise AssertionError("advisory must not drive the cert/gate run_node_op")

    monkeypatch.setattr(server, "_get_or_create_isabelle_warm_gate", lambda: _FakeGate())

    req = CheckerRequest(
        op="isabelle_warm_advisory", request_id=8, node_name="NodeA", timeout_secs=60.0
    )
    response, rc, log_extra = server._handle_isabelle_warm_advisory(req)
    assert seen == {"node": "NodeA", "theory": "Tablet_NodeA"}
    assert response["ok"] is True
    assert response["advisory_unavailable"] is False
    assert rc == 0
    assert log_extra.get("isabelle_warm_advisory") is True
    assert log_extra.get("isabelle_advisory_ok") is True


# ============================ (b) LIVE warm==cold THROUGH THE GATE ============================


@pytest.mark.isabelle_live
def test_warm_gate_verdict_equals_cold_verdict_live(tmp_path: Path) -> None:
    """(b) Through the GATE, against a REAL ``isabelle server``: a closed node's
    soundness cert produced by the warm-gate path is byte-identical to the cold
    path's. Run ONLY in throwaway scratch:
        pytest -m isabelle_live tests/test_isabelle_warm_gate.py
    """
    from trellis.checker import isabelle_session as isa

    isabelle_bin = isa.isabelle_bin()
    if not Path(isabelle_bin).exists():
        pytest.skip(f"isabelle binary not found at {isabelle_bin}")

    # A real closed node + the scaffold so reconciliation + the probe work.
    session_dir = tmp_path / "isabelle"
    session_dir.mkdir(parents=True, exist_ok=True)
    (session_dir / "Tablet_Preamble.thy").write_text(
        "theory Tablet_Preamble\n  imports Complex_Main\nbegin\nend\n", encoding="utf-8"
    )
    node = "Tablet_CertNode"
    principal = "CertNode"
    (session_dir / f"{node}.thy").write_text(
        f"theory {node}\n  imports Tablet_Preamble\nbegin\n\n"
        f"lemma {principal}:\n  fixes n :: nat\n"
        f'  shows "(\\<Sum>i=0..n. (2::nat)*i) = n*(n+1)"\n'
        f"  by (induction n) (auto simp: algebra_simps)\n\nend\n",
        encoding="utf-8",
    )

    held = {"warm": None}

    def warm_factory():
        if held["warm"] is None:
            s = IsabelleSession(
                name=f"trellis-gate-warm-{os.getpid()}",
                session="HOL",
                start_timeout_secs=600.0,
                warm_prefix_enabled=True,
            )
            s.start()
            held["warm"] = s
        return held["warm"]

    def reap():
        s = held["warm"]
        held["warm"] = None
        if s is not None:
            s.close()

    cold_names: List[str] = []

    def cold_factory():
        nm = f"trellis-gate-cold-{os.getpid()}-{len(cold_names)}"
        cold_names.append(nm)
        # `HOL`: no `session_dirs` here, so the scaffold-defined `Tablet_Base`
        # default would not resolve — see test_isabelle_session.py.
        s = IsabelleSession(
            name=nm, session="HOL", start_timeout_secs=600.0, warm_prefix_enabled=False
        )
        s.start()
        return s

    # Force the cross-check on the single check so warm + cold are both produced
    # and compared live; equality ⇒ no marker, the warm verdict stands.
    gate = wg.IsabelleWarmGate(
        session_dir=session_dir,
        runtime_root=tmp_path / "rt",
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session_factory=warm_factory,
        reap_warm_session=reap,
        cold_session_factory=cold_factory,
    )
    (tmp_path / "rt").mkdir(parents=True, exist_ok=True)
    try:
        payload, used_cold = gate.run_node_op(
            op="isabelle_thm_deps",
            node_name=principal,
            theory=node,
            cert_theorem=principal,
            qualified_thm=f"{node}.{principal}",
            timeout_secs=600.0,
        )
        # Equality ⇒ NO halt marker, and the cert is the real clean one.
        assert not (tmp_path / "rt" / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists(), (
            "warm and cold MUST agree on a genuinely closed node — a marker here "
            "means the gate diverged from cold"
        )
        assert payload["status"] == "ok", payload
        assert payload["oracles_used"] == [], payload
        assert payload["extra_shyps"] == [], payload
        assert payload["statement_hash"], "the closed node must hash a statement"
        assert len(payload["kernel_axioms"]) > 50, payload.get("kernel_axioms")
    finally:
        reap()
        for nm in [f"trellis-gate-warm-{os.getpid()}", *cold_names]:
            try:
                subprocess.run(
                    [isabelle_bin, "server", "-n", nm, "-x"],
                    capture_output=True, text=True, timeout=30,
                )
            except (OSError, subprocess.SubprocessError):
                pass


@pytest.mark.isabelle_live
def test_cross_check_node_scoped_cold_no_halt_with_sibling_sorry_live(
    tmp_path: Path,
) -> None:
    """THE bug, reproduced LIVE against a real ``isabelle server``.

    The reported live scenario: at a mid-formalization phase the proposed tablet
    holds a CLEAN definition node (``ConnGraph``, no proof) alongside an unrelated
    sibling lemma carrying ``sorry`` (``AltBinomial``). The active node being
    checked is the clean def.

      1. A WHOLE-SESSION ``isabelle build -b`` of that session dir goes RED on the
         sibling ``sorry`` (``*** Cheating requires quick_and_dirty mode!``) — the
         old cross-check used this and spuriously HALTed.
      2. The NODE-SCOPED cold recompute (``gate._cold_cert`` → ``run_cert_probe``)
         of the active def elaborates ONLY ``ConnGraph`` + its import cone
         (``Tablet_Preamble``), never the sibling, so it succeeds with a clean
         ``status == ok`` cert.
      3. Through the full gate the warm and node-scoped cold certs AGREE → NO halt
         marker.

    Run ONLY in throwaway scratch:
        pytest -m isabelle_live tests/test_isabelle_warm_gate.py
    """
    from trellis.checker import isabelle_session as isa

    isabelle_bin = isa.isabelle_bin()
    if not Path(isabelle_bin).exists():
        pytest.skip(f"isabelle binary not found at {isabelle_bin}")

    session_dir = tmp_path / "isabelle"
    session_dir.mkdir(parents=True, exist_ok=True)
    # The active node: a CLEAN closed node (here a trivially-closed lemma over a
    # local definition — the cert probe certifies the principal fact ``ConnGraph``
    # by qualified name, exactly the scaffold's ``lemma <node>:`` convention). The
    # essential property the bug hinges on: the active node is CLEAN (no sorry).
    node = "Tablet_ConnGraph"
    principal = "ConnGraph"
    (session_dir / f"{node}.thy").write_text(
        f"theory {node}\n  imports Tablet_Preamble\nbegin\n\n"
        f'definition conn_graph :: "nat \\<Rightarrow> bool" where\n'
        f'  "conn_graph n \\<longleftrightarrow> n = n"\n\n'
        f'lemma {principal}: "conn_graph n" by (simp add: conn_graph_def)\n\nend\n',
        encoding="utf-8",
    )
    # An UNRELATED sibling lemma with a `sorry` placeholder (legitimate at e.g.
    # TheoremStating). NOT imported by the active def, so it is outside its cone.
    (session_dir / "Tablet_AltBinomial.thy").write_text(
        "theory Tablet_AltBinomial\n  imports Tablet_Preamble\nbegin\n\n"
        'lemma alt_binomial: "(n::nat) + 0 = n" sorry\n\nend\n',
        encoding="utf-8",
    )
    # Render the REAL scaffold ROOT (declares ``Tablet_Base = HOL-Probability``
    # + the working ``Tablet`` session listing every present theory, including the
    # sibling sorry) + the canonical Preamble + the base subdir. This is the
    # production base-resolution path: the sessions parent on the prebuilt
    # ``Tablet_Base`` heap, resolved from this ROOT via ``session_dirs``.
    isabelle_scaffold.sync_session(session_dir)

    # --- Step 1: the WHOLE-SESSION cold build goes RED on the sibling sorry ---
    # (the old apples-to-oranges cross-check half — proving the halt trigger).
    whole = subprocess.run(
        [
            isabelle_bin, "build", "-b",
            "-o", "system_heaps=true", "-o", "threads=2",
            "-D", str(session_dir),
        ],
        capture_output=True, text=True, timeout=900,
    )
    assert whole.returncode != 0, (
        "the whole-session cold build MUST fail on the sibling sorry (this is the "
        "old cross-check's spurious-halt trigger); if it passed the repro is moot"
    )
    assert "quick_and_dirty" in (whole.stdout + whole.stderr), (
        whole.stdout[-2000:] + whole.stderr[-2000:]
    )

    # The sessions take ``session_dirs=[session_dir]`` so the server resolves the
    # scaffold's ``Tablet_Base`` session DEFINITION (the ``in "base"`` stanza in
    # the rendered ROOT) and parents on its prebuilt heap image — the production
    # base-resolution path. The per-call ``use_theories`` then resolves the node
    # theories + their import cone from ``master_dir`` from source.
    cold_names: List[str] = []

    def cold_factory():
        nm = f"trellis-sibsorry-cold-{os.getpid()}-{len(cold_names)}"
        cold_names.append(nm)
        s = IsabelleSession(
            name=nm,
            session_dirs=[str(session_dir)],
            start_timeout_secs=900.0,
            warm_prefix_enabled=False,
        )
        s.start()
        return s

    held = {"warm": None}

    def warm_factory():
        if held["warm"] is None:
            s = IsabelleSession(
                name=f"trellis-sibsorry-warm-{os.getpid()}",
                session_dirs=[str(session_dir)],
                start_timeout_secs=900.0,
                warm_prefix_enabled=True,
            )
            s.start()
            held["warm"] = s
        return held["warm"]

    def reap():
        s = held["warm"]
        held["warm"] = None
        if s is not None:
            s.close()

    rt = tmp_path / "rt"
    rt.mkdir(parents=True, exist_ok=True)
    gate = wg.IsabelleWarmGate(
        session_dir=session_dir,
        runtime_root=rt,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session_factory=warm_factory,
        reap_warm_session=reap,
        cold_session_factory=cold_factory,
    )

    try:
        # --- Step 2: the NODE-SCOPED cold recompute of the clean def is CLEAN ---
        # even though the sibling sorry sits right next to it on disk.
        cold = gate._cold_cert(
            theory=node,
            cert_theorem=principal,
            qualified_thm=f"{node}.{principal}",
            timeout_secs=900.0,
        )
        assert cold.cold_channel_failed is False, cold.cert
        assert cold.cert["status"] == "ok", cold.cert
        assert cold.cert["oracles_used"] == [], cold.cert
        assert "skip_proof" not in cold.cert["oracles_used"], cold.cert

        # --- Step 3: through the full gate, warm == cold → NO halt marker ---
        payload, used_cold = gate.run_node_op(
            op="isabelle_thm_deps",
            node_name=principal,
            theory=node,
            cert_theorem=principal,
            qualified_thm=f"{node}.{principal}",
            timeout_secs=900.0,
        )
        assert not (rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).exists(), (
            "a clean active def with an unrelated sibling sorry MUST NOT halt — "
            "the node-scoped cold recompute never elaborates the sibling"
        )
        assert payload["status"] == "ok", payload
        assert payload["oracles_used"] == [], payload
    finally:
        reap()
        for nm in [f"trellis-sibsorry-warm-{os.getpid()}", *cold_names]:
            try:
                subprocess.run(
                    [isabelle_bin, "server", "-n", nm, "-x"],
                    capture_output=True, text=True, timeout=30,
                )
            except (OSError, subprocess.SubprocessError):
                pass


@pytest.mark.isabelle_live
def test_cross_check_backstop_still_bites_on_real_divergence_live(
    tmp_path: Path,
) -> None:
    """The backstop STILL HALTS on a genuine warm-vs-cold divergence, LIVE.

    A live warm-vs-cold cert divergence is by design near-impossible (warm≡cold
    is the invariant), so we manufacture the divergence honestly: the active
    node's proof is a real ``sorry``, so the REAL node-scoped cold recompute
    (``gate._cold_cert`` → ``run_cert_probe`` against a live ``isabelle server``)
    surfaces the ``skip_proof`` oracle (the canonical ``sorry``-detection channel
    — the ``thm_oracles`` probe records it regardless of ``quick_and_dirty``). We
    then feed a (hypothetical heap-drift) CLEAN warm cert into the real
    ``_cross_check_and_maybe_halt`` and assert it WRITES the halt marker and
    returns the COLD cert — i.e. the node-scoped cold path still catches a real
    divergence and HALTS.

    Run ONLY in throwaway scratch:
        pytest -m isabelle_live tests/test_isabelle_warm_gate.py
    """
    from trellis.checker import isabelle_session as isa

    isabelle_bin = isa.isabelle_bin()
    if not Path(isabelle_bin).exists():
        pytest.skip(f"isabelle binary not found at {isabelle_bin}")

    session_dir = tmp_path / "isabelle"
    session_dir.mkdir(parents=True, exist_ok=True)
    # The active node's proof is a genuine ``sorry`` → the cold recompute's
    # ``thm_oracles`` probe surfaces the ``skip_proof`` oracle (the qd-independent
    # sorry channel), even though the cold session admits the ``sorry`` at build.
    node = "Tablet_OpenNode"
    principal = "OpenNode"
    (session_dir / f"{node}.thy").write_text(
        f"theory {node}\n  imports Tablet_Preamble\nbegin\n\n"
        f'lemma {principal}: "(n::nat) + 1 = 1 + n" sorry\n\nend\n',
        encoding="utf-8",
    )
    isabelle_scaffold.sync_session(session_dir)

    cold_names: List[str] = []

    def cold_factory():
        nm = f"trellis-bites-cold-{os.getpid()}-{len(cold_names)}"
        cold_names.append(nm)
        s = IsabelleSession(
            name=nm,
            session_dirs=[str(session_dir)],
            start_timeout_secs=900.0,
            warm_prefix_enabled=False,
        )
        s.start()
        return s

    # No warm session is actually used (we hand-build the warm cert), but the gate
    # reaps it first; a no-op reaper suffices.
    rt = tmp_path / "rt"
    rt.mkdir(parents=True, exist_ok=True)
    gate = wg.IsabelleWarmGate(
        session_dir=session_dir,
        runtime_root=rt,
        config=IsabelleWarmSessionConfig(enabled=True, cross_check_cadence=1),
        warm_session_factory=lambda: (_ for _ in ()).throw(
            AssertionError("warm session not needed in this test")
        ),
        reap_warm_session=lambda: None,
        cold_session_factory=cold_factory,
    )

    try:
        # The REAL node-scoped cold recompute of the sorry node: invalid_proof +
        # the skip_proof oracle present (this is the authoritative cold verdict).
        cold = gate._cold_cert(
            theory=node,
            cert_theorem=principal,
            qualified_thm=f"{node}.{principal}",
            timeout_secs=900.0,
        )
        assert cold.cold_channel_failed is False, cold.cert
        # The qd-default cold session admits the sorry at build (status ok), but
        # the probe's thm_oracles surfaces the skip_proof oracle regardless.
        assert "skip_proof" in cold.cert["oracles_used"], cold.cert

        # A hypothetical warm heap-drift FALSE-GREEN: clean, no oracles. Feeding it
        # into the real comparison MUST halt (the node-scoped cold path catches it).
        warm_false_green = _ok_outcome(node, oracles=[], deps=["refl"])
        warm_cert = iso.local_closure_cert_envelope(warm_false_green)
        result = gate._cross_check_and_maybe_halt(
            op="isabelle_thm_deps",
            node_name=principal,
            theory=node,
            cert_theorem=principal,
            qualified_thm=f"{node}.{principal}",
            timeout_secs=900.0,
            warm_payload=warm_cert,
        )
        marker = rt / wg.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME
        assert marker.exists(), "a real warm-clean vs cold-sorry divergence MUST halt"
        doc = json.loads(marker.read_text())
        assert doc["source"] == "isabelle_warm_cold_cross_check"
        assert any("oracles_used" in d for d in doc["diffs"]), doc["diffs"]
        # The returned verdict is the COLD cert (skip_proof present) — the warm
        # false-green is NOT accepted.
        assert "skip_proof" in result["oracles_used"], result
    finally:
        for nm in cold_names:
            try:
                subprocess.run(
                    [isabelle_bin, "server", "-n", nm, "-x"],
                    capture_output=True, text=True, timeout=30,
                )
            except (OSError, subprocess.SubprocessError):
                pass


def test_cold_cross_check_session_is_genuinely_cold(monkeypatch, tmp_path) -> None:
    """The cold channel must not inherit the warm env switch.

    `IsabelleSession.warm_prefix_enabled` defaults to the
    `TRELLIS_ISABELLE_WARM_SESSION` switch, so a cold factory that omits the
    argument inherits it on exactly the runs where the gate is enabled — and the
    cross-check then compares a warm verdict against a second warm-configured
    session, which is the one thing it exists to avoid.
    """
    from trellis.checker import isabelle_session as isa_mod
    from trellis.checker import isabelle_warm_config as cfg_mod
    from trellis.checker.server import CheckerServer

    created = []

    class _Spy:
        def __init__(self, *, name, session_dirs=None, warm_prefix_enabled=None, **kw):
            self.name = name
            self.warm_prefix_enabled = warm_prefix_enabled
            created.append(self)

        def start(self):
            return None

    # `<worker_repo>/.trellis/runtime/<name>` — the layout CheckerServer requires.
    runtime = tmp_path / "repo" / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)

    # The warm switch ON is precisely the condition under which the bug appeared.
    monkeypatch.setenv(cfg_mod.WARM_SESSION_ENV, "1")
    monkeypatch.setattr(isa_mod, "IsabelleSession", _Spy)

    server._create_cold_isabelle_session()

    assert created, "cold factory did not construct a session"
    assert created[-1].warm_prefix_enabled is False, (
        "the cold cross-check session inherited the warm switch"
    )


def test_halt_marker_records_the_cold_channel_diagnostics(tmp_path) -> None:
    """A disagreement marker must carry the cold channel's own error text.

    Regression for the live `GnpWeight` halt: the cold cert came back
    `invalid_proof` with no statement, no theorem and no axioms, and the marker
    recorded status/hash/oracles but no reason — so the cause had to be
    re-derived from the checker log instead of read off the marker.
    """
    import json

    from trellis.checker import isabelle_warm_gate as gate_mod

    gate = gate_mod.IsabelleWarmGate.__new__(gate_mod.IsabelleWarmGate)
    gate.runtime_root = tmp_path

    gate._write_halt_marker(
        node_name="GnpWeight",
        op="isabelle_thm_deps",
        diffs=["cert field 'status' differs: warm='ok' cold='invalid_proof'"],
        warm_payload={"status": "ok", "oracles_used": [], "statement_hash": "abc"},
        cold_cert={
            "status": "invalid_proof",
            "oracles_used": [],
            "statement_hash": "",
            "theorem_exists": False,
            "root_kind": "definition",
            "errors": ['Undefined fact: "Tablet_GnpWeight.GnpWeight"'],
            "message": "probe failed",
            "raw_tail": "*** Undefined fact",
        },
    )

    marker = json.loads(
        (tmp_path / gate_mod.CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME).read_text(
            encoding="utf-8"
        )
    )
    assert marker["cold_errors"] == ['Undefined fact: "Tablet_GnpWeight.GnpWeight"']
    assert marker["cold_message"] == "probe failed"
    assert marker["cold_raw_tail"] == "*** Undefined fact"
    assert marker["cold_theorem_exists"] is False
    assert marker["cold_root_kind"] == "definition"

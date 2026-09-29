"""Phase-4 lifecycle / provisioning tests for the warm Isabelle checker.

The warm session is held on the persistent ``CheckerServer`` process
(``_isabelle_session``, created lazily by ``_get_or_create_isabelle_session``,
reaped at ``shutdown``). The checker server outlives every worker burst (the
workers connect over the AF_UNIX socket; the server is one long-lived host
process), so the warm prefix it holds PERSISTS across bursts. These tests pin
the three lifecycle guarantees the brief asks for, deterministically, with a
STUB ``IsabelleSession`` (no real ``isabelle`` spawned):

1. **Persistence across bursts** — two consecutive node ops reuse the SAME
   cached warm session (no re-spawn). This is "burst N and burst N+1 see the
   same warm heap" at the unit level: a worker burst is a sequence of socket ops
   to the persistent server, and the session is not torn down between them.

2. **Promote-on-acceptance** — a node accepted (its ``Tablet_<Node>.thy``
   projected into the synced ``isabelle/`` dir) BETWEEN two checks is reconciled
   into the warm prefix on the next check, so a newly-accepted node is warm for
   the following one. (``reconcile_accepted_base`` runs every check and folds in
   the synced accepted set.)

3. **Non-durable warmth → transparent rebuild** — after the session is reaped
   (a checker restart / rewind drops the in-memory heap), the FIRST subsequent
   op transparently re-creates the session AND reconciles the warm prefix from
   the synced accepted dir from cold — no stale state, no manual relaunch.

The Lean/flag-OFF path is untouched (these tests force the flag ON via the env
switch; with the flag OFF the warm session/gate are never constructed).
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

import pytest

from trellis.checker import isabelle_scaffold
from trellis.checker import isabelle_session as isa_mod
from trellis.checker import isabelle_warm_config as cfg_mod
from trellis.checker.isabelle_session import CheckOutcome, WarmCheckOutcome
from trellis.checker.protocol import CheckerRequest
from trellis.checker.server import CheckerServer


# ============================ stub warm session ============================


_SESSIONS: List["_LifecycleStubSession"] = []


class _LifecycleStubSession:
    """A stand-in for :class:`IsabelleSession` that records its lifecycle.

    It tracks every reconcile (with the accepted set it was handed) and every
    check/purge, plus whether it is still ``started`` — so the tests can prove
    persistence (one instance reused), promote-on-acceptance (the accepted set
    grows across reconciles), and rebuild (a new instance reconciles from the
    synced dir after a reap). It maintains a ``promoted`` set the same way the
    real ``reconcile_accepted_base`` does (content-hash-free here — name set is
    enough for the lifecycle assertions)."""

    def __init__(self, *, name: str, warm_prefix_enabled: bool = False, **_kw: Any) -> None:
        self.name = name
        self.warm_prefix_enabled = warm_prefix_enabled
        self.started = False
        self.closed = False
        self.promoted: set[str] = set()
        self.reconcile_calls: List[Tuple[str, ...]] = []
        self.checked: List[str] = []
        self.purged: List[str] = []
        _SESSIONS.append(self)

    # lifecycle
    def start(self) -> None:
        self.started = True

    def close(self) -> None:
        self.closed = True
        self.started = False

    # warm-prefix API
    def reconcile_accepted_base(
        self, *, master_dir, accepted, timeout_secs=0.0, total_budget_secs=None
    ) -> None:
        self.reconcile_calls.append(tuple(accepted))
        # Mirror the real reconcile: the promoted set becomes exactly accepted.
        self.promoted = set(accepted)

    def purge_inflight(self, *, master_dir, theory, timeout_secs=60.0) -> None:
        self.purged.append(theory)

    # the observation layer calls THIS (worker build + probe)
    def check_theory(self, *, master_dir, theory, cert_theorem=None, timeout_secs=0.0):
        self.checked.append(theory)
        return _clean_outcome(theory)

    def check_node(self, *, master_dir, theory, cert_theorem=None, timeout_secs=0.0):
        self.checked.append(theory)
        return WarmCheckOutcome(
            ok=True, seconds=0.05, errors=[], outcome=_clean_outcome(theory)
        )


def _clean_outcome(theory: str) -> CheckOutcome:
    return CheckOutcome(
        ok=True,
        failed=0,
        finished=10,
        theory_name=f"Draft.{theory}",
        node_name=f"/tmp/{theory}.thy",
        oracles=[],
        dependencies=["refl"],
        theorem_exists=True,
        extra_shyps=[],
        statement_repr="P x = Q x",
        statement_hash="deadbeef",
        statement_repr_long="P x = Q x",
    )


# ============================ fixtures ============================


@pytest.fixture
def warm_server(tmp_path: Path, monkeypatch):
    """A ``CheckerServer`` with the warm flag ON and a STUB ``IsabelleSession``.

    The stub is injected by monkeypatching ``IsabelleSession`` in the session
    module (the server imports it from there inside
    ``_get_or_create_isabelle_session``). The cross-check cadence is 0 (the cold
    backstop is exercised elsewhere; here we test warmth lifecycle), so no cold
    session/build is needed.
    """
    import tempfile

    _SESSIONS.clear()
    base = Path(tempfile.mkdtemp(prefix="isa-warm-life-", dir=str(tmp_path)))
    repo = base / "r"
    runtime = repo / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    # NOTE: deliberately NO worker ``Tablet/`` dir here — the node-op tests seed
    # their accepted theories DIRECTLY into the session dir (the production
    # ``isabelle-sync-session`` projection output), and a present-but-empty
    # ``Tablet/`` would make the projection sweep delete those directly-seeded
    # theories as "stale". The advisory test (which calls ``sync_session``)
    # creates + seeds the worker ``Tablet/`` itself.

    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)
    server.set_expected_peer_uid(os.geteuid())

    # Flag ON, cadence OFF (warmth-only lifecycle).
    monkeypatch.setenv(cfg_mod.WARM_SESSION_ENV, "1")
    monkeypatch.setenv(cfg_mod.CROSS_CHECK_ENV, "0")
    server._isabelle_warm_config = None
    assert server._isabelle_warm_cfg().enabled is True

    # Inject the stub session class.
    monkeypatch.setattr(isa_mod, "IsabelleSession", _LifecycleStubSession)

    session_dir = server._isabelle_session_dir()
    session_dir.mkdir(parents=True, exist_ok=True)
    return server, session_dir


def _seed_node(session_dir: Path, node: str, *, imports: Sequence[str] = ()) -> None:
    """Project an accepted node's ``Tablet_<Node>.thy`` into the synced dir.

    ``imports`` names the extra ``Tablet_*`` theories this node imports. The warm
    base is the in-flight node's IMPORT CONE (the `lake build Tablet.<Node>`
    analogue), so a sibling only becomes warm when some node actually imports it.
    """
    deps = " ".join([isabelle_scaffold.PREAMBLE_THEORY, *imports])
    (session_dir / f"Tablet_{node}.thy").write_text(
        f"theory Tablet_{node}\n  imports {deps}\n"
        f'begin\nlemma {node}: "True" by simp\nend\n',
        encoding="utf-8",
    )


def _seed_preamble(session_dir: Path) -> None:
    (session_dir / f"{isabelle_scaffold.PREAMBLE_THEORY}.thy").write_text(
        f"theory {isabelle_scaffold.PREAMBLE_THEORY}\n  imports Complex_Main\nbegin\nend\n",
        encoding="utf-8",
    )


def _check(server: CheckerServer, node: str, rid: int):
    req = CheckerRequest(
        op="isabelle_thm_deps", request_id=rid, node_name=node, timeout_secs=60.0
    )
    return server._handle_isabelle_node_op(req)


# ============================ 1. persistence across bursts ============================


def test_warm_session_persists_across_bursts(warm_server) -> None:
    """Two consecutive node ops (the unit-level analogue of two worker bursts to
    the persistent checker) reuse the SAME cached warm session — it is created
    once and held on the server, never re-spawned per op."""
    server, session_dir = warm_server
    _seed_preamble(session_dir)
    _seed_node(session_dir, "Dep")
    _seed_node(session_dir, "InFlight", imports=["Tablet_Dep"])

    # Burst N: check InFlight.
    resp1, _rc1, log1 = _check(server, "InFlight", 1)
    assert log1.get("isabelle_warm_gate") is True
    assert resp1["status"] == "ok"
    # Exactly one warm session was created (plus possibly nothing else).
    warm_sessions = [s for s in _SESSIONS if s.warm_prefix_enabled]
    assert len(warm_sessions) == 1
    first = warm_sessions[0]
    assert first.started is True and first.closed is False

    # Burst N+1: check InFlight again. The SAME warm session is reused.
    resp2, _rc2, _log2 = _check(server, "InFlight", 2)
    assert resp2["status"] == "ok"
    warm_sessions = [s for s in _SESSIONS if s.warm_prefix_enabled]
    assert len(warm_sessions) == 1, "the warm session must persist, not re-spawn per burst"
    assert warm_sessions[0] is first
    # It reconciled on BOTH checks (warm prefix kept in lockstep with the dir).
    assert len(first.reconcile_calls) == 2


def test_warm_session_is_the_cached_server_handle(warm_server) -> None:
    """The warm session the gate uses IS ``server._isabelle_session`` (the cached
    per-server handle), so its lifetime == the server's, across all bursts."""
    server, session_dir = warm_server
    _seed_preamble(session_dir)
    _seed_node(session_dir, "InFlight")
    _check(server, "InFlight", 1)
    assert server._isabelle_session is not None
    assert server._isabelle_session.warm_prefix_enabled is True
    assert server._isabelle_session.started is True


# ============================ 2. promote-on-acceptance ============================


def test_node_accepted_in_burst_n_is_warm_in_burst_n_plus_1(warm_server) -> None:
    """A node accepted (projected into the synced dir) BETWEEN two checks is
    reconciled into the warm prefix on the next check — i.e. a node accepted in
    burst N is warm in burst N+1. The reconcile's accepted set GROWS to include
    the newly-accepted sibling, and it is in the session's promoted set after."""
    server, session_dir = warm_server
    _seed_preamble(session_dir)
    _seed_node(session_dir, "InFlight")

    # Burst N: only Preamble is accepted (besides the in-flight node). Reconcile
    # sees just the Preamble (InFlight is excluded as the in-flight node).
    _check(server, "InFlight", 1)
    warm = server._isabelle_session
    assert warm.reconcile_calls[-1] == ("Tablet_Preamble",)
    assert "Tablet_NewlyAccepted" not in warm.promoted

    # Between bursts: a sibling gets ACCEPTED — its .thy is projected into the
    # synced isabelle/ dir (what the kernel's pre-batch sync does on acceptance).
    _seed_node(session_dir, "NewlyAccepted")

    # Burst N+1: check a DIFFERENT in-flight node; reconcile now promotes the
    # newly-accepted sibling (it is warm for this and every later check).
    _seed_node(session_dir, "Other", imports=["Tablet_NewlyAccepted"])
    _check(server, "Other", 2)
    warm = server._isabelle_session
    assert "Tablet_NewlyAccepted" in warm.promoted, (
        "a node accepted in the previous burst must be warm (promoted) in the next"
    )
    # The reconciled set is the IN-FLIGHT NODE'S IMPORT CONE: the Preamble plus
    # the newly-accepted sibling it imports. `Tablet_InFlight` is deliberately
    # ABSENT — `Other` does not import it, so under the `lake build Tablet.<Node>`
    # analogue it is not part of this node's build and must not be able to affect
    # its verdict.
    assert warm.reconcile_calls[-1] == (
        "Tablet_Preamble",
        "Tablet_NewlyAccepted",
    )


# ============================ 3. non-durable warmth → rebuild ============================


def test_first_check_after_restart_rebuilds_warm_prefix_from_synced_dir(warm_server) -> None:
    """On a checker restart / rewind the in-memory warm heap is gone
    (``_isabelle_session`` reset to None). The FIRST subsequent op transparently
    RE-CREATES the session and reconciles the warm prefix from the synced
    accepted dir from cold — no stale state, no manual relaunch."""
    server, session_dir = warm_server
    _seed_preamble(session_dir)
    _seed_node(session_dir, "Dep")
    _seed_node(session_dir, "InFlight", imports=["Tablet_Dep"])

    # Warm up: check once (creates + promotes Dep).
    _check(server, "InFlight", 1)
    first = server._isabelle_session
    assert first is not None
    assert "Tablet_Dep" in first.promoted

    # Simulate a checker restart / rewind: the warm session is reaped and its
    # in-memory heap (promoted set) is gone.
    server._close_isabelle_session()
    assert server._isabelle_session is None
    assert first.closed is True

    # Meanwhile more nodes were accepted before the restart's first check (the
    # synced dir is the durable source of truth across the restart).
    _seed_node(session_dir, "AcceptedWhileDown")
    # The in-flight node picks up the new dependency (a worker edit); only then is
    # it in InFlight's import cone and therefore part of its warm base.
    _seed_node(
        session_dir, "InFlight", imports=["Tablet_Dep", "Tablet_AcceptedWhileDown"]
    )

    # The FIRST op after the restart: a brand-new session is created and the warm
    # prefix is rebuilt from the synced dir from cold — Dep AND the node accepted
    # while the checker was down are both promoted, with no manual relaunch.
    _check(server, "InFlight", 2)
    rebuilt = server._isabelle_session
    assert rebuilt is not None and rebuilt is not first, "a fresh session is built"
    assert rebuilt.started is True
    assert "Tablet_Dep" in rebuilt.promoted
    assert "Tablet_AcceptedWhileDown" in rebuilt.promoted, (
        "the rebuild must reconcile from the synced accepted dir (durable), so a "
        "node accepted while the checker was down is warm after restart"
    )


def test_reap_then_advisory_rebuilds_session(warm_server) -> None:
    """The same rebuild-on-restart holds for the worker advisory op: after a reap
    the next advisory recreates the warm session and reconciles from the dir.

    The advisory projects the worker ``Tablet/`` into the session dir itself
    (``sync_session``), so seed the accepted siblings as worker sources — the
    faithful production shape (the worker authors ``Tablet/<Node>.thy``)."""
    server, session_dir = warm_server
    _seed_preamble(session_dir)
    tablet_dir = session_dir.parent / "Tablet"
    tablet_dir.mkdir(parents=True, exist_ok=True)
    # InFlight imports Dep, so Dep is in InFlight's import cone and therefore in
    # its warm base (the `lake build Tablet.<Node>` analogue). Written as WORKER
    # sources because the advisory re-projects `Tablet/` over the session dir.
    for node, deps in (("Dep", ()), ("InFlight", ("Tablet_Dep",))):
        imports = " ".join([isabelle_scaffold.PREAMBLE_THEORY, *deps])
        (tablet_dir / f"{node}.thy").write_text(
            f"theory Tablet_{node}\n  imports {imports}\n"
            f'begin\nlemma {node}: "True" by simp\nend\n',
            encoding="utf-8",
        )
    # The in-flight node is already projected into the session dir pre-dispatch
    # (the production ``sync_tablet_dir`` + sync) so the dispatcher's
    # "has the worker synced this node yet" guard passes; ``run_warm_advisory``
    # re-projects from the worker source on top.
    _seed_node(session_dir, "InFlight", imports=["Tablet_Dep"])

    req = CheckerRequest(
        op="isabelle_warm_advisory", request_id=1, node_name="InFlight", timeout_secs=60.0
    )
    resp1, _rc1, _log1 = server._handle_isabelle_warm_advisory(req)
    assert resp1["advisory_unavailable"] is False
    assert resp1["ok"] is True
    first = server._isabelle_session
    assert first is not None
    # Dep is an accepted sibling (projected from the worker source), warm in the
    # prefix; InFlight is the in-flight node (excluded from the prefix).
    assert "Tablet_Dep" in first.promoted

    server._close_isabelle_session()
    assert server._isabelle_session is None

    resp2, _rc2, _log2 = server._handle_isabelle_warm_advisory(req)
    assert resp2["ok"] is True
    rebuilt = server._isabelle_session
    assert rebuilt is not None and rebuilt is not first
    assert "Tablet_Dep" in rebuilt.promoted

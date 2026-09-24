"""Warm Isabelle node-check GATE wiring + cold-rebuild backstop (Phase 2).

This is the SENSITIVE seam: it routes the checker's Isabelle node-check through
the Phase-1 warm session (``IsabelleSession.reconcile_accepted_base`` →
``check_theory`` against the warm accepted-sibling prefix) so a node re-check
re-elaborates only the in-flight theory, while GUARANTEEING the warm verdict can
never differ from the cold verdict it would have produced. Flag-gated OFF by
default (``isabelle_warm_session.enabled`` / ``TRELLIS_ISABELLE_WARM_SESSION``);
when OFF this module is never entered and the cold node-check path in
``server.py`` is byte-for-byte unchanged.

Closure-correctness, three layers (design v2 §4.2):

1. **Reconciliation** makes the warm prefix == the synced accepted-node set on
   disk (content-hashed), so the warm base only ever holds accepted, oracle-clean
   theories — never an advisory-green or stale one.
2. **Anomaly → graceful cold fallback.** Any ``IsabelleSessionError``
   (spawn/connect/protocol/timeout) during reconcile or the warm check discards
   the warm session and re-runs the SAME op on a fresh COLD session for that
   check. A node is NEVER accepted on a broken warm session.
3. **Cold-recompute cross-check (the guarantee).** On a configurable cadence
   (every Nth closure check) AND on a forced trigger (the stateless checker's
   stand-in for "mandatory at tablet completion"), the gate ALSO recomputes the
   cert COLD — a fresh ``run_cert_probe`` for the SAME active node on a fresh,
   no-warm-prefix Isabelle session — and compares the warm cert to that cold
   cert. The cold recompute is NODE-SCOPED: it ``use_theories``-elaborates only
   the active node and its import cone (exactly what the warm ``check_node``
   elaborates), NOT a whole-session ``isabelle build``. Matching the warm
   verdict's scope is load-bearing: at e.g. the TheoremStating phase the proposed
   tablet legitimately holds sibling lemmas with ``sorry`` placeholders that are
   NOT in the active node's cone, so a whole-session cold build would go red on an
   unrelated ``sorry`` and the cross-check would spuriously HALT comparing a
   single-node warm verdict against a whole-session cold verdict (apples to
   oranges). The node-scoped cold recompute never touches those siblings, so a
   clean active node compares clean-to-clean. On a cert mismatch (the closed
   certificate the two scopes produce differs — oracles/deps/statement-hash/
   status) it writes the ``checker_disagreement_halt.json`` marker the supervisor
   loop + the bridge poll (mirroring the existing fail-loudly dual-check halt) and
   returns the COLD cert, so a warm false-green can neither ship nor be accepted.
   The halt is SEMANTICALLY GATED on a warm FALSE-ACCEPT: it fires only when the
   WARM verdict is a CLEAN CLOSURE (``isabelle_cert_gate_violation``-clean: ``ok``
   status, ``theorem_exists``, no oracle/shyps) that the cold verdict refutes. If
   the warm verdict is itself NON-closing (an oracle/non-``ok``/missing-theorem —
   the warm gate already rejects the node), a warm-vs-cold detail divergence is
   benign (both reject; no false-accept) — logged for visibility, never halted.
   (The kernel's separate per-cert-op SPINE ``isabelle_build_session`` whole-
   session build remains the phase-aware whole-tablet gate; it is NOT this
   cross-check.)

One Isabelle process at a time: the cold cross-check first REAPS the warm session
(so the cold probe is the only live ``isabelle`` process), then lets the warm
session lazily re-create + re-promote on the next op. All of this runs under the
dispatcher's ``_workspace_lock`` (the lake-serialization lock), so the warm
server and the cold recompute never overlap.
"""

from __future__ import annotations

import json
import logging
import re
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple

from trellis.atomic_actions import isabelle_observations
from trellis.atomic_actions.observations import _progress_emit
from trellis.checker import isabelle_scaffold
from trellis.checker.isabelle_session import (
    IsabelleReconcileBudgetExhausted,
    IsabelleSession,
    IsabelleSessionError,
)
from trellis.checker.isabelle_warm_config import (
    IsabelleWarmSessionConfig,
    env_force_cross_check,
)

_LOGGER = logging.getLogger("trellis.checker.isabelle_warm_gate")

# The halt marker filename the supervisor `Run` loop
# (`kernel/src/bin/runtime_cli.rs`) and the Python bridge
# (`trellis/runtime/bridge.py`) both poll at `<runtime_root>/`. Kept verbatim
# (not imported) so this module has no kernel/bridge import dependency; the
# constant is asserted equal to the bridge's in the gate tests.
CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME = "checker_disagreement_halt.json"

# H2: the NON-halting "the cold cross-check tier is degraded" marker. Written
# (and refreshed) when the cold channel has failed K consecutive cross-checks in
# a row — the cross-check cannot run, so a (hypothetical) warm heap-corruption
# would not be caught by the cross-check tier (the SPINE cold build the kernel
# runs per cert op still fails CLOSED, so this is visibility, not a hole). It is
# NOT polled by the supervisor loop (no halt); it is a forensic breadcrumb for
# the operator + the run-resource monitor. Cleared automatically the next time a
# cross-check succeeds.
COLD_CROSSCHECK_DEGRADED_MARKER_FILENAME = "checker_cold_crosscheck_degraded.json"

# The cert fields whose warm-vs-cold equality IS the closure-correctness
# invariant. A warm `thm`-value that produced these byte-identically to a cold
# build is, by Phase-0's GO, the same kernel theorem; any divergence is a
# heap-corruption alarm that must HALT. `statement_repr_long` is included so a
# qualified-name drift (the Correspondence axis input) is caught too, and
# `statement_type_hash` so a TYPE/SORT drift is caught — every other field here
# digests a pretty-print, and Isabelle's printer prunes type information by
# design, so warm and cold could agree on all of them while disagreeing about
# what the theorem actually says. It also proves the alias de-leak in
# `run_cert_probe` really fires: `Term.Const` names are always internal LONG
# names, so an un-rewritten warm digest would differ from every cold one.
# `statement_repr_typed` is deliberately NOT here — it is a diagnostic print,
# and a print has no business being able to HALT the run.
_CERT_FIELDS: Tuple[str, ...] = (
    "status",
    "oracles_used",
    "kernel_axioms",
    "extra_shyps",
    "statement_hash",
    "statement_repr",
    "statement_repr_long",
    "statement_type_hash",
    "theorem_exists",
)

# The node ops that carry the soundness CERT (and so drive the cadence
# cross-check). `isabelle_check_node` is the build-only envelope (no cert), so it
# warms + falls back on anomaly but does not itself trigger a cold cert compare.
_CERT_OPS = frozenset({"isabelle_thm_oracles", "isabelle_thm_deps"})

# The approved-oracle FLOOR for IsabelleHol — the allow-set the kernel's
# `isabelle_cert_gate_violation` filters `oracles_used` against. It mirrors the
# kernel's `ISABELLE_HOL_APPROVED_ORACLES_FLOOR` (`kernel/src/backend.rs`), which
# is EMPTY: no oracle is approvable (`skip_proof`=`sorry`/`<proof>` and every
# external-solver oracle `smt`/`z3`/… are rejected). Pinned here so the warm
# gate's "is this warm cert a clean CLOSURE" test reuses the SAME closure
# predicate the kernel accept-gate applies, rather than inventing one.
ISABELLE_APPROVED_ORACLES_FLOOR: frozenset = frozenset()


_IMPORTS_RE = re.compile(
    r"^\s*theory\s+\S+\s+imports\s+(?P<imports>.*?)\s+begin\b",
    re.DOTALL | re.MULTILINE,
)


def _declared_tablet_imports(session_dir: Path, theory: str) -> List[str]:
    """The ``Tablet_*`` theories named in ``theory``'s own ``imports`` clause.

    Parses the theory header (``theory T imports A B C begin``). Only sibling
    tablet theories that actually exist in ``session_dir`` are returned; library
    imports (``Main``, ``Complex_Main``, ``HOL-Probability.Probability``) are not
    our concern — Isabelle resolves those from the session ancestry.
    """
    path = session_dir / f"{theory}.thy"
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return []
    match = _IMPORTS_RE.search(text)
    if match is None:
        return []
    found: List[str] = []
    for token in match.group("imports").split():
        name = token.strip().strip('"').split(".")[-1]
        if not name.startswith(isabelle_scaffold.NODE_THEORY_PREFIX):
            continue
        if name.endswith("__Cert"):
            continue
        if (session_dir / f"{name}.thy").is_file():
            found.append(name)
    return found


def _import_cone_theories(session_dir: Path, *, theory: str) -> List[str]:
    """The transitive ``Tablet_*`` import cone of ``theory``, dependency-first.

    THE LEAN ANALOGUE. The Lean checker builds ``lake build Tablet.<Node>``: it
    names the node as the target and lets the build tool resolve the import cone
    (``materialize_tablet_oleans``). A sibling the node does not import is never
    named, so it cannot affect the node's verdict.

    This is the Isabelle counterpart: hold warm exactly the theories ``theory``
    transitively imports, so ``use_theories`` for the node elaborates against a
    base containing only its real dependencies.

    Previously this enumerated EVERY ``Tablet_<Node>.thy`` present in the session
    dir and called it the "accepted base". That was wrong twice over: the files
    on disk are whatever the worker most recently authored (accepted or not), and
    a node was made to depend on siblings it never imported. A worker submitting
    a batch of N new nodes therefore had every node's certificate poisoned by any
    ONE sibling that failed to elaborate — observed live, where two nodes that
    passed ``isabelle_check_node`` with rc=0 were still returned ``invalid_proof``
    by ``isabelle_thm_deps`` because an unrelated sibling in the same batch did
    not compile.

    ``theory`` itself is EXCLUDED (it is the in-flight node under check).
    ``Tablet_Preamble`` is always included when present: the scaffold has every
    node theory import it, so it is in every cone.
    """
    preamble = isabelle_scaffold.PREAMBLE_THEORY
    ordered: List[str] = []
    seen: set = {theory}

    def visit(name: str) -> None:
        # Depth-first, dependencies before dependents. `seen` also breaks any
        # import cycle the worker may have authored (Isabelle would reject it,
        # but this must not hang the checker).
        if name in seen:
            return
        seen.add(name)
        for dep in _declared_tablet_imports(session_dir, name):
            visit(dep)
        ordered.append(name)

    for dep in _declared_tablet_imports(session_dir, theory):
        visit(dep)

    if (session_dir / f"{preamble}.thy").is_file() and preamble not in ordered:
        ordered.insert(0, preamble)
    return ordered


@dataclass
class _ColdCertResult:
    """The NODE-SCOPED cold cross-check outcome for one node.

    ``cert`` is the cold cert recomputed by ``run_cert_probe`` on a fresh,
    no-warm-prefix session for the SAME active node (its theory + import cone
    only — matching the warm verdict's scope). There is no whole-session build
    component: a genuine cold proof failure of the active node surfaces INSIDE
    ``cert`` (``status == invalid_proof`` / non-zero ``returncode``) and is
    caught by the cert-field comparison, exactly like an oracle or statement-hash
    divergence.
    """

    cert: Dict[str, Any]
    # True iff the cold CHANNEL itself failed (cold session spawn/connect/
    # timeout, or the cold probe transport failed) — as opposed to a genuine cold
    # verdict. An infrastructure failure of the cold cross-check must NOT be read
    # as a warm-vs-cold DISAGREEMENT (that would halt the run on a flaky cold
    # spawn). The cross-check is skipped (warm verdict stands; the next cadence
    # tick re-attempts), never halted.
    cold_channel_failed: bool = False


class IsabelleWarmGate:
    """Routes a checker Isabelle node-check through the warm session + backstop.

    Construction takes callables rather than the server object so the gate is
    testable in isolation:

    * ``warm_session_factory()`` → the held warm :class:`IsabelleSession`
      (flag-ON), created lazily by the caller; the gate calls it on each op so a
      reaped session is transparently re-created.
    * ``reap_warm_session()`` → discard the held warm session (the caller nulls
      its handle so the next ``warm_session_factory()`` rebuilds it). Used on a
      warm anomaly and before a cold cross-check (one-process discipline).
    * ``cold_session_factory()`` → a FRESH, started, flag-OFF
      :class:`IsabelleSession` for the NODE-SCOPED cold recompute; the gate
      ``close()``s it. The cold cross-check recomputes the cert by
      ``run_cert_probe`` on this session (the active node + its import cone only,
      exactly matching the warm verdict's scope); it deliberately does NOT drive a
      whole-session ``isabelle build`` (that would compare a single-node warm
      verdict against a whole-tablet cold verdict and spuriously HALT on an
      unrelated sibling ``sorry`` at a mid-formalization phase).

    The gate holds a per-server closure-check counter (under its own lock) to
    drive the periodic cadence; it is in-memory + non-durable (a checker restart
    resets it, which is fine — the cross-check is defence-in-depth and a forced
    cross-check / the next cadence tick re-establishes ground truth).
    """

    def __init__(
        self,
        *,
        session_dir: Path,
        runtime_root: Path,
        config: IsabelleWarmSessionConfig,
        warm_session_factory: Callable[[], IsabelleSession],
        reap_warm_session: Callable[[], None],
        cold_session_factory: Callable[[], IsabelleSession],
    ) -> None:
        self.session_dir = Path(session_dir)
        self.runtime_root = Path(runtime_root)
        self.config = config
        self._warm_session_factory = warm_session_factory
        self._reap_warm_session = reap_warm_session
        self._cold_session_factory = cold_session_factory
        self._cert_check_count = 0
        # H2: consecutive cold-cross-checks skipped for a cold-CHANNEL failure.
        # Reset to 0 whenever a cross-check actually runs (channel OK). Guarded by
        # the same counter lock as ``_cert_check_count``.
        self._consecutive_cold_failures = 0
        self._counter_lock = threading.Lock()

    # ------------------------------ public API ------------------------------

    def run_node_op(
        self,
        *,
        op: str,
        node_name: str,
        theory: str,
        cert_theorem: str,
        qualified_thm: str,
        timeout_secs: float,
    ) -> Tuple[Dict[str, Any], bool]:
        """Run one Isabelle node op through the warm gate + backstop.

        Returns ``(payload, used_cold_fallback)``. ``payload`` is the SAME
        envelope shape the cold ``server.py`` path returns (``check_node``: an
        ``ExternalCommandObservation``; ``thm_oracles``: the oracle/dep stdout
        envelope; ``thm_deps``: the ``LocalClosureProbeOutput`` cert). The warm
        verdict is reconciled-then-checked; an anomaly degrades to a fresh cold
        session; a cadence/forced soundness check additionally cold cross-checks
        and HALTS (writes the marker) + returns the COLD cert on any mismatch.
        """
        anomaly: Optional[str] = None
        budget_exceeded: Optional[str] = None
        inflight_timeout: Optional[str] = None
        payload: Optional[Dict[str, Any]] = None
        try:
            session = self._warm_session_factory()
            try:
                self._reconcile(
                    session, exclude_theory=theory, timeout_secs=timeout_secs
                )
            except IsabelleReconcileBudgetExhausted as exc:
                # Case (a) — the shared budget ran out at the PRE-DISPATCH
                # check: no promote task is in flight, the warm socket is
                # idle, and the session is healthy with everything that
                # finished recorded in ``_promoted``. It must NOT be reaped
                # — handled distinctly below (serve cold, keep warm).
                budget_exceeded = f"{exc.kind}: {exc.message}"
            except IsabelleSessionError as exc:
                if exc.kind == "timed_out":
                    # Case (b) — a member promote itself timed out
                    # CLIENT-side: the client gave up on an async
                    # ``use_theories`` task the server may STILL be
                    # elaborating on that socket. The session must be
                    # REAPED before the cold serve (pre-tranche-4
                    # behaviour): keeping it risks two concurrent Isabelle
                    # processes on the shared heaps (the cold serve below
                    # spawns one while the warm server still elaborates)
                    # and a stale-reply socket desync (``_await_terminal``
                    # does not correlate task ids, so the next op could
                    # read THIS task's late reply). Handled distinctly
                    # below.
                    inflight_timeout = f"{exc.kind}: {exc.message}"
                else:
                    # Genuine transport/protocol anomaly: today's
                    # reap-and-fallback path, unchanged.
                    raise
            if budget_exceeded is None and inflight_timeout is None:
                payload = self._observe(
                    session,
                    op=op,
                    theory=theory,
                    cert_theorem=cert_theorem,
                    qualified_thm=qualified_thm,
                    timeout_secs=timeout_secs,
                )
        except IsabelleSessionError as exc:
            # A session anomaly that PROPAGATED (e.g. from reconcile, or from the
            # warm ``check_node`` path) — capture it for the cold fallback below.
            anomaly = f"{exc.kind}: {exc.message}"

        if inflight_timeout is not None:
            # Case (b) — reconcile member promote timed out IN-FLIGHT: the
            # warm server may still be elaborating that task on its socket.
            # REAP the session first (one-process discipline: the cold serve
            # below must be the only live ``isabelle`` process; a kept
            # session would also risk a stale-reply desync on its next op),
            # THEN serve this op cold. Logged distinctly from the
            # pre-dispatch budget case (a), which keeps the session.
            _LOGGER.warning(
                "isabelle warm gate: reconcile member promote timed out "
                "in-flight on %s for node=%s (%s) — the warm server may still "
                "be elaborating that task; reaping the warm session before "
                "the cold serve",
                op,
                node_name,
                inflight_timeout,
            )
            self._reap_warm_session()
            cold_started = time.time()
            _progress_emit(
                f"[acceptance]   isabelle {op} {node_name}: reconcile member "
                f"promote timed out in-flight ({inflight_timeout}); warm "
                f"session reaped (in-flight task may still be running); "
                f"serving this op cold"
            )
            cold_payload = self._observe_cold(
                op=op,
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=timeout_secs,
            )
            _progress_emit(
                f"[acceptance]   isabelle {op} {node_name}: cold serve after "
                f"in-flight promote timeout done in "
                f"{time.time() - cold_started:.1f}s"
            )
            return cold_payload, True

        if budget_exceeded is not None:
            # Case (a) — reconcile BUDGET EXCEEDED pre-dispatch: serve this
            # op via the existing cold path, but do NOT treat the warm
            # session as broken — no reap, no "anomaly" label (no task is in
            # flight, so the cold serve is the only live Isabelle process).
            # Its ``_promoted`` map keeps every member that finished, so the
            # next op resumes the reconcile where this one ran out instead
            # of rebuilding the prefix from scratch.
            _LOGGER.warning(
                "isabelle warm gate: reconcile budget exceeded on %s for "
                "node=%s (%s; op budget %.0fs shared across the cone) — "
                "serving this op cold; warm session left intact",
                op,
                node_name,
                budget_exceeded,
                timeout_secs,
            )
            cold_started = time.time()
            _progress_emit(
                f"[acceptance]   isabelle {op} {node_name}: reconcile budget "
                f"exceeded ({budget_exceeded}); serving this op cold (warm "
                f"session kept — finished promotes retained for the next op)"
            )
            cold_payload = self._observe_cold(
                op=op,
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=timeout_secs,
            )
            _progress_emit(
                f"[acceptance]   isabelle {op} {node_name}: cold serve after "
                f"reconcile budget exceeded done in "
                f"{time.time() - cold_started:.1f}s"
            )
            return cold_payload, True

        # The observation functions (``*_server_side``) catch
        # ``IsabelleSessionError`` THEMSELVES and encode a transport failure as a
        # fail-closed envelope with ``spawn_error`` set (a command envelope) /
        # ``status == internal_error`` + ``spawn_error`` (the cert). So a warm
        # anomaly during the check shows up as that envelope, NOT a raised
        # exception. Detect it and degrade to cold — distinct from a legitimate
        # proof failure (``invalid_proof`` / non-zero rc with NO ``spawn_error``),
        # which the cold path would reproduce and so must NOT trigger a fallback.
        if anomaly is None and payload is not None and _envelope_is_anomaly(payload):
            anomaly = str(payload.get("spawn_error") or "warm session transport failure")

        if anomaly is not None:
            # Warm-session anomaly: NEVER accept on a broken warm session.
            # Discard it (re-created + re-promoted on the next op) and serve the
            # verdict from a fresh COLD session for this check.
            _LOGGER.warning(
                "isabelle warm session anomaly on %s for node=%s (%s) — "
                "degrading to cold for this check",
                op,
                node_name,
                anomaly,
            )
            self._reap_warm_session()
            cold_started = time.time()
            _progress_emit(
                f"[acceptance]   isabelle {op} {node_name}: warm-session anomaly "
                f"({anomaly}); cold fallback starting (fresh cold session — "
                f"this can take ~15 min)"
            )
            cold_payload = self._observe_cold(
                op=op,
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=timeout_secs,
            )
            _progress_emit(
                f"[acceptance]   isabelle {op} {node_name}: cold fallback done "
                f"in {time.time() - cold_started:.1f}s"
            )
            return cold_payload, True

        assert payload is not None  # no anomaly ⇒ the warm observe produced one

        # Soundness ops drive the cold-rebuild cross-check cadence. The cert the
        # cross-check compares is the WARM payload just produced.
        if op in _CERT_OPS and self._should_cross_check():
            payload = self._cross_check_and_maybe_halt(
                op=op,
                node_name=node_name,
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=timeout_secs,
                warm_payload=payload,
            )
        return payload, False

    def run_warm_advisory(
        self,
        *,
        node_name: str,
        theory: str,
        timeout_secs: float,
    ) -> Dict[str, Any]:
        """The worker inner-loop ADVISORY: warm-elaborate ONLY the in-flight node.

        This is the Phase-3 fast pre-check the worker's ``incremental-check``
        drives — the Isabelle analogue of the Lean warm ``lean --server``
        advisory. It is NOT the gate: it does the warm node-level work (project
        the worker's in-flight ``Tablet/<node>.thy`` into the session dir →
        reconcile the warm accepted-sibling prefix → ``IsabelleSession.check_node``
        of JUST the in-flight theory against that warm prefix) and returns
        ``{ok, seconds, errors, advisory_unavailable}`` — with NO ``thm_deps``
        cert, NO cold cross-check, NO halt. Green here is necessary but NOT
        sufficient: the deterministic node-check gate (``run_node_op`` →
        ``thm_deps``) remains the only sign-off, exactly as the Lean
        ``incremental-check`` is advisory to ``check_node``.

        Failure-open like the Lean broker: any session anomaly (spawn / connect /
        protocol / timeout) reaps the warm session (re-created lazily on the next
        op) and returns ``advisory_unavailable=True`` so the worker falls back to
        ``isabelle build`` — never a wrong green. A genuine proof failure is
        ``ok=False`` with the ``*** …`` error lines in ``errors`` (NOT
        ``advisory_unavailable``), so the worker sees what to fix.

        Runs under the dispatcher's ``_workspace_lock`` (the caller holds it), so
        the one warm ``isabelle`` process is used serially.
        """
        try:
            # Make the worker's in-flight edit visible: project the synced
            # `Tablet/<node>.thy` into `<session_dir>/Tablet_<node>.thy` (the
            # cheap pure file copy `isabelle-sync-session` does pre-batch — the
            # worker calls the advisory ad-hoc, so it must project itself) and
            # refresh the ROOT. Best-effort: a projection error leaves the prior
            # state and the warm check below reports staleness rather than wedging.
            try:
                isabelle_scaffold.sync_session(self.session_dir)
            except Exception:  # noqa: BLE001 — projection is best-effort
                _LOGGER.exception(
                    "isabelle warm advisory: session projection failed for "
                    "node=%s (checking against the prior projected state)",
                    node_name,
                )
            session = self._warm_session_factory()
            self._reconcile(session, exclude_theory=theory)
            # Purge the in-flight node AFTER the check (not before — see
            # `_observe` for the timing rationale) so repeated advisory checks on
            # the same node + later promotes do not collide with its residue. The
            # advisory is failure-OPEN and NON-soundness-bearing.
            warm = session.check_node(
                master_dir=str(self.session_dir),
                theory=theory,
                cert_theorem=node_name,
                timeout_secs=timeout_secs,
            )
        except IsabelleSessionError as exc:
            # Transport/session anomaly: discard the warm session (re-created on
            # the next op) and tell the worker the advisory is unavailable so it
            # falls back to `isabelle build`. NEVER a green.
            _LOGGER.warning(
                "isabelle warm advisory anomaly for node=%s (%s: %s) — advising "
                "isabelle build fallback",
                node_name,
                exc.kind,
                exc.message,
            )
            self._reap_warm_session()
            return {
                "ok": False,
                "seconds": 0.0,
                "errors": [f"{exc.kind}: {exc.message}"],
                "advisory_unavailable": True,
            }
        return {
            "ok": bool(warm.ok),
            "seconds": float(warm.seconds),
            "errors": list(warm.errors),
            "advisory_unavailable": False,
        }

    # --------------------------- warm-path helpers ---------------------------

    def _reconcile(
        self,
        session: IsabelleSession,
        *,
        exclude_theory: str,
        timeout_secs: Optional[float] = None,
    ) -> None:
        """Hold the in-flight node's IMPORT CONE warm (content-hashed).

        The Lean analogue of ``lake build Tablet.<Node>``: only what the node
        imports is named, so an unrelated sibling — broken, unaccepted, or
        mid-edit — cannot reach this node's verdict. This also makes the warm
        path agree with the cold cross-check, which this module's own docstring
        already defines as node-scoped ("only the active node and its import
        cone, NOT a whole-session build") for exactly this reason.

        ``timeout_secs`` is the CALLER'S OP BUDGET, threaded down as the
        reconcile's TOTAL budget (shared across the cone's member promotes —
        never multiplied per member). Before this the member promotes each ran
        under ``DEFAULT_SESSION_START_TIMEOUT_SECS`` (240 s) — a session-START
        constant misused as a per-member elaboration budget: one genuinely slow
        in-flight cone member (~20 min elaborations are real) timed out at
        240 s, was misread as a warm-session anomaly, and cost a reaped session
        plus a ~25-minute cold fallback. ``None`` keeps the legacy per-promote
        default (the failure-open advisory path, which prefers its own
        fixed-budget behaviour).
        """
        cone = _import_cone_theories(self.session_dir, theory=exclude_theory)
        if timeout_secs is None:
            session.reconcile_accepted_base(
                master_dir=str(self.session_dir), accepted=cone
            )
            return
        session.reconcile_accepted_base(
            master_dir=str(self.session_dir),
            accepted=cone,
            total_budget_secs=float(timeout_secs),
        )

    def _observe(
        self,
        session: IsabelleSession,
        *,
        op: str,
        theory: str,
        cert_theorem: str,
        qualified_thm: str,
        timeout_secs: float,
    ) -> Dict[str, Any]:
        """Drive the op's observation function against ``session``.

        The same ``isabelle_observations.*_server_side`` functions the cold path
        uses — they call ``session.check_theory`` (byte-identical wire on a warm
        session; only the held-open prefix differs), so the cert is produced
        identically and only the in-flight node re-elaborates. A worker proof
        failure or a transport failure is surfaced INSIDE the envelope (these
        functions catch ``IsabelleSessionError`` themselves for the cert ops);
        ``check_node`` re-raises a session anomaly, which the caller maps to the
        cold fallback.

        H1 (in-flight freshness) — purge AFTER, not before. Live
        characterization of the Option-A held-open document settled the timing:
        purging the in-flight theory BEFORE the check and re-``use_theories``-ing
        it corrupts the document ("Illegal theory header"); leaving the prior
        in-flight theory RESIDENT corrupts the NEXT op when the same node is
        re-checked (same-node loop) OR promoted as a now-accepted sibling
        (cross-node) — that residue collides with the promote. Purging the
        in-flight node AFTER its check clears that residue, so both the same-node
        re-check and the cross-node promote re-elaborate cleanly (verified live),
        while the check itself runs against the fresh-loaded node. A
        proof-REPLACING edit (clean→``sorry``) can still read residency-stale on
        the warm channel; SOUNDNESS does not rest on the warm verdict — the
        kernel runs a fresh COLD whole-session ``isabelle build``
        (``quick_and_dirty=false``) as a hard precondition of every ``thm_deps``
        cert op (the audit's "spine") plus the 1-in-N warm/cold cross-check, so a
        stale-clean warm cert on an actually-broken node is caught and HALTS. A
        per-cert UNIQUE probe name keeps each cert read fresh; the warm session's
        ``quick_and_dirty=false`` rejects a ``sorry`` at load. See the READY
        notes for the residual proof-replace limitation.
        """
        if op == "isabelle_check_node":
            return isabelle_observations.check_node_server_side(
                session,
                master_dir=str(self.session_dir),
                theory=theory,
                cert_theorem=cert_theorem,
                timeout_secs=timeout_secs,
            )
        if op == "isabelle_thm_oracles":
            return isabelle_observations.thm_oracles_server_side(
                session,
                master_dir=str(self.session_dir),
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=timeout_secs,
            )
        if op == "isabelle_thm_deps":
            return isabelle_observations.thm_deps_server_side(
                session,
                master_dir=str(self.session_dir),
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=timeout_secs,
            )
        raise IsabelleSessionError(
            "protocol_error", f"warm gate: unknown node op {op!r}"
        )
        # NO post-check purge. The in-flight node is deliberately LEFT resident.
        #
        # This used to call `purge_theories` on the real node name to keep the
        # held-open document residue-free. On Isabelle 2025-2 that is actively
        # destructive, and it is what made a batch of correct nodes unacceptable:
        #
        #   * `Resources.State.purge_theories` computes both the state removal
        #     and the document deletion edits, but the PUBLIC wrapper commits only
        #     the state removal and DISCARDS the edits
        #     (`src/Pure/PIDE/headless.scala`; the internal `clean_theories` is
        #     the path that applies them). So the resource state says the theory
        #     is gone while the PIDE document still holds its full text.
        #   * The next load of that same name therefore sees `old_theory = None`
        #     and emits an INSERTION at offset 0 instead of a replacement,
        #     appending a SECOND copy of the theory into the populated node.
        #   * The second `theory … begin` hits `illegal_init`
        #     (`src/Pure/PIDE/document.ML`) → `Illegal theory header`, after which
        #     `definition`/`end` parse with no theory context and report
        #     "missing theory context for command …".
        #
        # Reproduced deterministically: checking the eight-node batch in the live
        # order yields rc 0,0,0,0,1,0,1,0 and the next round fails exactly the six
        # nodes whose cone touches a previously purged name, while `EdgeSet` and
        # `VertexSet` survive as retained predecessors
        # (`headless.scala` keeps predecessors of non-purged nodes).
        #
        # Freshness — the reason the purge existed — is supplied instead by the
        # content-keyed `__In_<sha>` alias (`fresh_inflight_theory`): changed
        # bytes yield a NEW theory name, so there is no same-name reload to go
        # wrong, and unchanged bytes reuse the resident copy. Residue is bounded
        # by recycling the session, not by a purge that cannot work.

    # --------------------------- cold-path helpers ---------------------------

    def _observe_cold(
        self,
        *,
        op: str,
        theory: str,
        cert_theorem: str,
        qualified_thm: str,
        timeout_secs: float,
    ) -> Dict[str, Any]:
        """Serve the op verdict from a FRESH, flag-OFF cold session.

        Used on a warm anomaly (the graceful fallback). The cold session is a
        plain ``IsabelleSession`` (no warm prefix): it re-elaborates the node
        graph from source — exactly the pre-Phase-2 behavior — so the verdict is
        the trustworthy cold one. The session is reaped BY NAME in ``finally``.
        A cold-session anomaly here is itself surfaced inside the envelope (the
        observation functions' own fail-closed paths), so a node is never
        accepted on a failure of BOTH channels.
        """
        cold = self._cold_session_factory()
        try:
            return self._observe(
                cold,
                op=op,
                theory=theory,
                cert_theorem=cert_theorem,
                qualified_thm=qualified_thm,
                timeout_secs=timeout_secs,
            )
        except IsabelleSessionError as exc:
            # Both channels failed — fail closed with the cold cause (never an
            # accept). Match the op's envelope shape.
            if op == "isabelle_thm_deps":
                return isabelle_observations._internal_error_cert(
                    f"isabelle cold-fallback failed: {exc.message}",
                    timed_out=(exc.kind == "timed_out"),
                )
            return isabelle_observations.external_command_envelope(
                returncode=None,
                stderr=exc.message,
                timed_out=(exc.kind == "timed_out"),
                spawn_error=exc.message,
            )
        finally:
            try:
                cold.close()
            except Exception:  # noqa: BLE001 — best-effort reap
                _LOGGER.exception("error closing cold fallback session")

    def _cold_cert(
        self,
        *,
        theory: str,
        cert_theorem: str,
        qualified_thm: str,
        timeout_secs: float,
    ) -> _ColdCertResult:
        """The authoritative NODE-SCOPED cold cert for one node.

        Reaps the warm session FIRST (one-process discipline), then runs a fresh
        cold ``run_cert_probe`` for the active node on a no-warm-prefix session,
        mapped through ``local_closure_cert_envelope`` to the SAME cert dict the
        warm ``thm_deps`` payload carries.

        The cold recompute is NODE-SCOPED — ``run_cert_probe`` ``use_theories``-
        elaborates the worker node theory (resolving its import cone from source)
        then the checker-owned probe, exactly the scope the warm ``check_node``
        elaborates. It deliberately does NOT run a whole-session ``isabelle
        build``: an unrelated sibling ``sorry`` (legitimate at a
        mid-formalization phase, e.g. TheoremStating) is not in the active node's
        cone, so it never poisons the comparison. A genuine cold proof FAILURE of
        the active node is a real verdict (``status == invalid_proof`` in the
        cert), caught by the cert-field comparison — not a channel failure.

        The warm session is left reaped; the caller's next op re-creates + re-
        promotes it. Runs under the dispatcher's ``_workspace_lock``, so this is
        the only live ``isabelle`` process for its duration.
        """
        self._reap_warm_session()
        cold_started = time.time()
        _progress_emit(
            f"[acceptance]   isabelle cold cross-check {theory}: starting "
            f"(fresh cold session — this can take ~15 min)"
        )
        cold_channel_failed = False
        cold = None
        try:
            cold = self._cold_session_factory()
        except IsabelleSessionError as exc:
            # The cold session could not even start — cannot cross-check.
            cert = isabelle_observations._internal_error_cert(
                f"isabelle cold cross-check session start failed: {exc.message}",
                timed_out=(exc.kind == "timed_out"),
            )
            _progress_emit(
                f"[acceptance]   isabelle cold cross-check {theory}: failed "
                f"(cold session start: {exc.kind}) in {time.time() - cold_started:.1f}s"
            )
            return _ColdCertResult(cert=cert, cold_channel_failed=True)
        try:
            try:
                probe = isabelle_observations.run_cert_probe(
                    cold,
                    master_dir=str(self.session_dir),
                    theory=theory,
                    cert_theorem=cert_theorem,
                    qualified_thm=qualified_thm,
                    timeout_secs=timeout_secs,
                )
                cert = isabelle_observations.local_closure_cert_envelope(probe)
            except isabelle_observations._ProbeWorkerFailed as failed:
                # The WORKER node proof did not build clean cold — a genuine cold
                # verdict (the active node is not closed), NOT a channel failure.
                cert = isabelle_observations.local_closure_cert_envelope(
                    failed.worker_outcome
                )
            except IsabelleSessionError as exc:
                # The cold session/probe transport failed mid-check — channel
                # failure, not a verdict.
                cold_channel_failed = True
                cert = isabelle_observations._internal_error_cert(
                    f"isabelle cold cross-check probe failed: {exc.message}",
                    timed_out=(exc.kind == "timed_out"),
                )
        finally:
            try:
                cold.close()
            except Exception:  # noqa: BLE001 — best-effort reap
                _LOGGER.exception("error closing cold cross-check session")

        # A cold cert of `internal_error` is the CHANNEL failing, not a verdict.
        # `use_theories` can fail during import/dependency resolution and never
        # produce a snapshot; that reply carries no theorem, statement or
        # dependencies. Treating it as a refutation compared a clean warm
        # certificate against a fabricated one and HALTED a live run on a closure
        # "disagreement" that did not exist — the warm verdict there was correct
        # (the node's defining fact resolved oracle-free).
        if str(cert.get("status", "")) == isabelle_observations.STATUS_INTERNAL_ERROR:
            cold_channel_failed = True
        _progress_emit(
            f"[acceptance]   isabelle cold cross-check {theory}: done "
            f"status={cert.get('status')} in {time.time() - cold_started:.1f}s"
        )
        return _ColdCertResult(cert=cert, cold_channel_failed=cold_channel_failed)

    # ----------------------------- the backstop -----------------------------

    def _should_cross_check(self) -> bool:
        """True iff this soundness check should drive a cold cross-check.

        A forced cross-check (the env switch — the completion stand-in) ALWAYS
        triggers; otherwise the periodic cadence (every Nth soundness check)
        triggers. Cadence ``0`` disables the periodic path (forced still works).
        The counter is incremented under the lock so concurrent ops can't skip a
        tick (Isabelle ops serialize under ``_workspace_lock`` anyway, but the
        lock keeps the counter correct if that ever relaxes).
        """
        if env_force_cross_check():
            return True
        cadence = self.config.effective_cross_check_cadence()
        if cadence <= 0:
            return False
        with self._counter_lock:
            self._cert_check_count += 1
            return self._cert_check_count % cadence == 0

    def _cross_check_and_maybe_halt(
        self,
        *,
        op: str,
        node_name: str,
        theory: str,
        cert_theorem: str,
        qualified_thm: str,
        timeout_secs: float,
        warm_payload: Dict[str, Any],
    ) -> Dict[str, Any]:
        """Compare the warm cert to a cold cert; HALT + prefer cold on mismatch.

        The warm payload may be a ``thm_deps`` cert dict directly, or a
        ``thm_oracles`` stdout envelope (no structured cert) — in the latter
        case we recompute the warm cert structurally for the comparison would be
        apples-to-oranges, so we cross-check on the cert dict the cold probe
        produces vs the warm cert dict produced by a parallel warm cert read.
        To keep the comparison honest and the wire shape intact, the cross-check
        always compares CERT DICTS: for ``thm_deps`` the warm payload IS the cert
        dict; for ``thm_oracles`` we compare the cold cert's oracle/dep surface
        against the warm stdout surface.
        """
        cold = self._cold_cert(
            theory=theory,
            cert_theorem=cert_theorem,
            qualified_thm=qualified_thm,
            timeout_secs=timeout_secs,
        )

        # Cold-CHANNEL failure (the cold session could not spawn / the cold probe
        # transport failed / timed out) is NOT a warm-vs-cold disagreement — it
        # means the cross-check could not be performed this time. Do NOT halt on
        # infrastructure flakiness; the warm verdict (already validated warm)
        # stands and the next cadence tick re-attempts the cross-check. (A genuine
        # cold proof FAILURE — the active node not building cold — is NOT a
        # channel failure and DOES surface as a cert mismatch below, because that
        # is a real disagreement.)
        if cold.cold_channel_failed:
            with self._counter_lock:
                self._consecutive_cold_failures += 1
                consecutive = self._consecutive_cold_failures
            reason = str(cold.cert.get("spawn_error") or "")[:400]
            _LOGGER.warning(
                "isabelle cold cross-check could not run for node=%s (cold channel "
                "failed: %s) — skipping the cross-check, warm verdict stands "
                "(consecutive cold-channel failures: %d)",
                node_name,
                reason[:200],
                consecutive,
            )
            self._maybe_alarm_cold_degraded(
                node_name=node_name,
                consecutive=consecutive,
                reason=reason,
            )
            return warm_payload

        # The cold channel is healthy this time → reset the consecutive-failure
        # counter and clear any degraded marker (H2): the cross-check tier is
        # operational again.
        self._reset_cold_failures()

        if op == "isabelle_thm_deps":
            diffs = _cert_field_diffs(warm_payload, cold.cert)
            on_mismatch_payload = cold.cert
        else:  # isabelle_thm_oracles — compare the oracle/dep SURFACE
            diffs = _oracle_surface_diffs(warm_payload, cold.cert)
            # The cold oracle/dep surface, rendered into the same stdout envelope
            # the op returns (so a mismatch returns the cold surface).
            on_mismatch_payload = _oracle_envelope_from_cert(cold.cert)

        # A cold recompute that disagrees on the ACTIVE node surfaces inside the
        # node-scoped cert itself: a proof that fails to build cold → ``status ==
        # invalid_proof``; a ``sorry`` that builds (the cold session admits it)
        # → ``skip_proof`` in ``oracles_used``; a different elaborated theorem →
        # ``statement_hash``. All are compared above. So no separate whole-session
        # build check is needed — and a whole-session build would spuriously fire
        # on an UNRELATED sibling ``sorry`` outside the active node's cone at a
        # mid-formalization phase (the bug this fix removes). The kernel's separate
        # per-cert-op spine ``isabelle_build_session`` remains the phase-aware
        # whole-tablet gate.

        if diffs:
            # Semantic gate: the disagreement HALT exists to catch a warm
            # FALSE-ACCEPT — the warm gate reporting a node CLOSED (clean closure
            # cert) when cold contradicts it. So halt ONLY when the WARM verdict
            # is a clean closure (`isabelle_cert_gate_violation`-clean + `ok`):
            # an accepting verdict the cold one refutes is the genuine alarm.
            #
            # If the warm cert is NOT a clean closure (it carries `skip_proof`/
            # another oracle, a non-`ok` status, a missing theorem, or dangling
            # shyps), the warm gate is ALREADY REJECTING this node — both warm and
            # cold reject and merely disagree on the rejection detail (e.g. warm
            # found the theorem with a `sorry` oracle while cold's cone did not
            # build, a cone-level staleness). There is no false-accept, so the
            # divergence is benign: log it for visibility but do NOT write the
            # halt marker and do NOT treat the op as a disagreement.
            if not _warm_payload_is_clean_closure(op, warm_payload):
                _LOGGER.warning(
                    "isabelle warm/cold cert divergence on %s node=%s, but the "
                    "WARM verdict is NOT a clean closure (it already rejects this "
                    "node) — both sides reject, no false-accept, NOT halting. "
                    "diffs=%s",
                    op,
                    node_name,
                    diffs,
                )
                # Both reject; the warm verdict (already non-accepting) stands —
                # treat as effective agreement (both refuse the node).
                return warm_payload
            self._write_halt_marker(
                node_name=node_name,
                op=op,
                diffs=diffs,
                warm_payload=warm_payload,
                cold_cert=cold.cert,
            )
            _LOGGER.error(
                "ISABELLE WARM/COLD DISAGREEMENT on %s node=%s — supervisor will "
                "HALT. diffs=%s",
                op,
                node_name,
                diffs,
            )
            # Prefer COLD: never let a warm result that diverged from cold be the
            # verdict, even for this in-flight check.
            return on_mismatch_payload
        # Agreement (the Phase-0-proven normal case): the warm verdict stands.
        return warm_payload

    def _maybe_alarm_cold_degraded(
        self, *, node_name: str, consecutive: int, reason: str
    ) -> None:
        """H2: surface a loud, NON-halting alarm on a chronically-flaky cold channel.

        After ``cold_failure_alarm_after`` consecutive cross-checks skipped for a
        cold-CHANNEL failure, log at ERROR and write/refresh the degraded marker
        (a forensic breadcrumb, not a halt). The SPINE cold build the kernel runs
        per cert op still fails CLOSED while the cold channel is wedged, so this
        is visibility — a silently-eroded cross-check tier becomes observable.
        ``0`` threshold disables the alarm.
        """
        threshold = self.config.effective_cold_failure_alarm_after()
        if threshold <= 0 or consecutive < threshold:
            return
        _LOGGER.error(
            "ISABELLE COLD CROSS-CHECK TIER DEGRADED: the cold channel has failed "
            "%d consecutive cross-checks (latest node=%s: %s). The warm gate's "
            "1-in-N cold cross-check cannot run, so a hypothetical warm heap "
            "corruption would not be caught by THIS tier — the kernel's per-cert "
            "spine cold build still fails closed, so the run is not unsound, but "
            "the cross-check defence-in-depth is offline. Investigate the cold "
            "isabelle channel (server spawn / build).",
            consecutive,
            node_name,
            reason,
        )
        path = self.runtime_root / COLD_CROSSCHECK_DEGRADED_MARKER_FILENAME
        payload = {
            "kind": "cold_crosscheck_degraded",
            "schema_version": 1,
            "source": "isabelle_warm_cold_cross_check",
            "consecutive_cold_channel_failures": consecutive,
            "alarm_threshold": threshold,
            "latest_node": node_name,
            "latest_reason": reason,
            "unix_ts": int(time.time()),
            "note": (
                "NON-halting alarm: the Isabelle cold cross-check tier is "
                "degraded (the cold channel keeps failing). The run is NOT "
                "halted — the kernel's per-cert-op spine cold build still gates "
                "acceptance — but the warm gate's defence-in-depth cross-check "
                "is offline. This marker is cleared automatically the next time "
                "a cold cross-check succeeds."
            ),
        }
        body = json.dumps(payload, indent=2, sort_keys=True)
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body, encoding="utf-8")
        except OSError as exc:
            _LOGGER.error(
                "failed to write cold-crosscheck-degraded marker at %s: %s",
                path,
                exc,
            )

    def _reset_cold_failures(self) -> None:
        """Clear the consecutive-cold-failure counter + degraded marker (H2)."""
        with self._counter_lock:
            was = self._consecutive_cold_failures
            self._consecutive_cold_failures = 0
        if was:
            path = self.runtime_root / COLD_CROSSCHECK_DEGRADED_MARKER_FILENAME
            try:
                path.unlink()
            except FileNotFoundError:
                pass
            except OSError as exc:
                _LOGGER.warning(
                    "could not clear cold-crosscheck-degraded marker at %s: %s",
                    path,
                    exc,
                )

    def _write_halt_marker(
        self,
        *,
        node_name: str,
        op: str,
        diffs: Sequence[str],
        warm_payload: Mapping[str, Any],
        cold_cert: Mapping[str, Any],
    ) -> None:
        """Persist the ``checker_disagreement_halt.json`` marker (write-once).

        Same filename + ``kind: "checker_disagreement"`` + ``clear_instructions``
        schema the kernel's ``write_checker_disagreement_halt_marker`` writes, so
        the supervisor `Run` loop and the bridge both poll it and HALT, and the
        operator's existing ``ack-halt-marker --force`` clears it. Best-effort: a
        write failure is logged (the warm gate already returned the cold verdict,
        so even without the marker this single check is sound; the marker is the
        run-level halt + forensics layer).
        """
        path = self.runtime_root / CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME
        if path.exists():
            _LOGGER.warning(
                "isabelle warm/cold halt marker already present at %s; preserving "
                "the original diagnostic (current node=%s)",
                path,
                node_name,
            )
            return
        ts = int(time.time())
        payload = {
            "kind": "checker_disagreement",
            "schema_version": 1,
            "source": "isabelle_warm_cold_cross_check",
            "active_node": node_name,
            "op": op,
            "unix_ts": ts,
            "diffs": list(diffs),
            "warm_oracles_used": list(warm_payload.get("oracles_used", []))
            if isinstance(warm_payload, Mapping)
            else [],
            "warm_statement_hash": warm_payload.get("statement_hash", "")
            if isinstance(warm_payload, Mapping)
            else "",
            "warm_status": warm_payload.get("status", "")
            if isinstance(warm_payload, Mapping)
            else "",
            "cold_oracles_used": list(cold_cert.get("oracles_used", [])),
            "cold_statement_hash": cold_cert.get("statement_hash", ""),
            "cold_status": cold_cert.get("status", ""),
            # The cold channel's own diagnostics. Without these a divergence is
            # a bare "warm ok, cold invalid_proof, everything else empty" and the
            # cause has to be re-derived from scratch — which is exactly what
            # happened on `GnpWeight`, where the cold cert carried no statement,
            # no theorem and no axioms, and the marker recorded no reason.
            # The whole point of the marker is that the NEXT occurrence is
            # diagnosable without reproducing it.
            "cold_errors": list(cold_cert.get("errors", [])),
            "cold_message": cold_cert.get("message", ""),
            "cold_raw_tail": cold_cert.get("raw_tail", ""),
            "cold_theorem_exists": cold_cert.get("theorem_exists", None),
            "cold_root_kind": cold_cert.get("root_kind", ""),
            "clear_instructions": (
                "The trellis supervisor is HALTED because the Isabelle warm "
                "node-check gate produced a verdict that DIVERGED from the cold "
                f"rebuild on node `{node_name}`. A warm-vs-cold disagreement is a "
                "potential closure hole (the warm heap may have shipped a "
                "non-reproducible accept); it is a STRUCTURAL alarm, not a "
                "transient. Investigate the `diffs` above (the warm heap is the "
                "suspect — Phase 0 proved warm==cold, so a divergence means heap "
                "corruption or a reconciliation bug). After review, DELETE this "
                f"file to resume: `rm {path}`. The supervisor will refuse to "
                "dispatch new bursts until then."
            ),
        }
        body = json.dumps(payload, indent=2, sort_keys=True)
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body, encoding="utf-8")
            _LOGGER.error(
                "isabelle warm/cold halt marker persisted at %s (node=%s)",
                path,
                node_name,
            )
        except OSError as exc:
            _LOGGER.error(
                "failed to write isabelle warm/cold halt marker at %s: %s — the "
                "single check already returned the cold verdict, but the "
                "run-level halt has nothing to poll",
                path,
                exc,
            )


# --------------------------- anomaly detection (pure) ---------------------------


def _envelope_is_anomaly(payload: Mapping[str, Any]) -> bool:
    """True iff an observation envelope reflects a TRANSPORT/SESSION anomaly.

    The observation functions (``check_node_server_side`` /
    ``thm_oracles_server_side`` / ``thm_deps_server_side`` →
    ``_internal_error_cert``) catch ``IsabelleSessionError`` and set
    ``spawn_error`` (the spawn/connect/protocol/timeout cause) on the returned
    envelope. A non-empty ``spawn_error`` is therefore the unambiguous
    "the warm session itself failed" signal — distinct from a legitimate proof
    failure (a real ``sorry``/bad proof yields ``invalid_proof`` / non-zero
    returncode with an EMPTY ``spawn_error``, which the cold path would
    reproduce). Only the former degrades to the cold channel.
    """
    spawn_error = payload.get("spawn_error")
    return bool(spawn_error)


# --------------------------- cert comparison (pure) ---------------------------


def _norm_field(name: str, value: Any) -> Any:
    """Normalize a cert field for order-insensitive equality.

    ``oracles_used`` / ``kernel_axioms`` / ``extra_shyps`` are kernel SETS — the
    observation layer de-dups but preserves observation order, which is not
    load-order-stable, so compare them as sorted sets. Scalars compare directly.
    """
    if name in ("oracles_used", "kernel_axioms", "extra_shyps"):
        if isinstance(value, (list, tuple)):
            return sorted(str(v) for v in value)
        return value
    return value


def _cert_field_diffs(
    warm: Mapping[str, Any], cold: Mapping[str, Any]
) -> List[str]:
    """The list of cert fields where the warm cert diverges from the cold cert.

    Empty ⇒ byte-equal verdict (the closure-correctness invariant holds). Each
    diff names the field + both values so the halt marker is self-explanatory.
    This is the load-bearing comparison: a non-empty result MUST halt.
    """
    diffs: List[str] = []
    for field in _CERT_FIELDS:
        w = _norm_field(field, warm.get(field))
        c = _norm_field(field, cold.get(field))
        if w != c:
            diffs.append(f"cert field {field!r} differs: warm={w!r} cold={c!r}")
    return diffs


def _cert_is_clean_closure(cert: Mapping[str, Any]) -> bool:
    """True iff a cert DICT represents a clean CLOSURE (an ACCEPTING verdict).

    The Python port of the kernel's accept predicate: a ``thm_deps`` cert is a
    clean closure iff the kernel would NOT reject it on the closure axes —
    i.e. ``status == 'ok'`` AND ``isabelle_cert_gate_violation`` returns ``None``
    (``kernel/src/runtime_cli_observations.rs``). That cert-gate is, exactly:

    * ``oracles_used`` ⊆ :data:`ISABELLE_APPROVED_ORACLES_FLOOR` (= ∅, so
      ``oracles_used`` must be EMPTY — any oracle, ``skip_proof``/``smt``/…, is a
      violation),
    * ``extra_shyps`` EMPTY (no dangling/inconsistent-class sort hypotheses),
    * ``theorem_exists`` TRUE.

    Reusing this kernel predicate is the whole point of the disagreement-halt
    gate: the cross-check HALTS only when the warm verdict is a clean closure the
    cold verdict contradicts (a warm false-accept). If the warm cert is NOT a
    clean closure (an oracle, non-``ok`` status, missing theorem, dangling
    shyps), the warm gate is ALREADY rejecting the node, so a warm-vs-cold detail
    divergence is benign (both reject) and must not halt.
    """
    if str(cert.get("status", "")) != "ok":
        return False
    if not bool(cert.get("theorem_exists", False)):
        return False
    oracles = cert.get("oracles_used", []) or []
    if any(str(o) not in ISABELLE_APPROVED_ORACLES_FLOOR for o in oracles):
        return False
    shyps = cert.get("extra_shyps", []) or []
    if list(shyps):
        return False
    return True


def _oracle_envelope_is_clean_closure(envelope: Mapping[str, Any]) -> bool:
    """True iff a ``thm_oracles`` STDOUT envelope represents a clean CLOSURE.

    The ``thm_oracles`` op returns a string envelope (not a cert dict): the probe
    renders ``oracles: …`` / ``depends on axioms: …`` / ``extra_shyps: …`` into
    ``stdout`` with ``returncode == 0`` iff BOTH the worker proof and the probe
    checked clean. The clean-closure surface is therefore: ``returncode == 0``
    (the theorem checked — the envelope's stand-in for ``status == ok`` +
    ``theorem_exists``), NO oracles outside the floor (= ∅), and NO ``extra_shyps``
    — the same closure predicate as :func:`_cert_is_clean_closure`, read off the
    rendered surface.
    """
    if envelope.get("returncode") != 0:
        return False
    if str(envelope.get("spawn_error") or ""):
        return False
    surface = _parse_oracle_stdout(str(envelope.get("stdout", "")))
    oracles = surface.get("oracles", [])
    if any(str(o) not in ISABELLE_APPROVED_ORACLES_FLOOR for o in oracles):
        return False
    if surface.get("extra_shyps", []):
        return False
    return True


def _warm_payload_is_clean_closure(
    op: str, warm_payload: Mapping[str, Any]
) -> bool:
    """True iff the WARM verdict for ``op`` is a clean CLOSURE (an accept).

    Dispatches on the op's wire shape: ``isabelle_thm_deps`` carries a cert DICT
    (:func:`_cert_is_clean_closure`); ``isabelle_thm_oracles`` carries a stdout
    ENVELOPE (:func:`_oracle_envelope_is_clean_closure`). The disagreement halt
    fires only when this is TRUE and the cold verdict contradicts it.
    """
    if op == "isabelle_thm_deps":
        return _cert_is_clean_closure(warm_payload)
    return _oracle_envelope_is_clean_closure(warm_payload)


def _oracle_surface_diffs(
    warm_envelope: Mapping[str, Any], cold_cert: Mapping[str, Any]
) -> List[str]:
    """Compare a ``thm_oracles`` warm stdout envelope to the cold cert surface.

    ``thm_oracles_server_side`` renders ``oracles: …`` / ``depends on axioms: …``
    / ``extra_shyps: …`` into ``stdout``. We parse those back and compare them as
    sets to the cold cert's ``oracles_used`` / ``kernel_axioms`` / ``extra_shyps``
    — the soundness-load-bearing surface. A returncode mismatch is a diff too.
    """
    diffs: List[str] = []
    warm_surface = _parse_oracle_stdout(str(warm_envelope.get("stdout", "")))
    pairs = (
        ("oracles", "oracles_used"),
        ("depends on axioms", "kernel_axioms"),
        ("extra_shyps", "extra_shyps"),
    )
    for stdout_key, cert_key in pairs:
        w = sorted(warm_surface.get(stdout_key, []))
        c = sorted(str(v) for v in cold_cert.get(cert_key, []))
        if w != c:
            diffs.append(
                f"oracle surface {stdout_key!r} differs: warm={w!r} cold={c!r}"
            )
    warm_rc = warm_envelope.get("returncode")
    cold_rc = cold_cert.get("returncode")
    if warm_rc != cold_rc:
        diffs.append(f"returncode differs: warm={warm_rc!r} cold={cold_rc!r}")
    return diffs


def _parse_oracle_stdout(stdout: str) -> Dict[str, List[str]]:
    """Parse the ``thm_oracles`` stdout envelope back into name lists."""
    out: Dict[str, List[str]] = {}
    for line in stdout.splitlines():
        for key in ("oracles", "depends on axioms", "extra_shyps"):
            prefix = f"{key}:"
            if line.startswith(prefix):
                names = [tok for tok in line[len(prefix):].split() if tok]
                out[key] = names
    return out


def _oracle_envelope_from_cert(cert: Mapping[str, Any]) -> Dict[str, Any]:
    """Render a cold cert into the ``thm_oracles`` stdout envelope shape.

    So a ``thm_oracles`` cross-check mismatch returns the COLD surface in the
    same wire shape the op produces (the gate prefers cold on disagreement).
    """
    lines: List[str] = []
    lines.append(
        f"oracles: {' '.join(str(v) for v in cert.get('oracles_used', []))}".rstrip()
    )
    lines.append(
        f"depends on axioms: "
        f"{' '.join(str(v) for v in cert.get('kernel_axioms', []))}".rstrip()
    )
    shyps = cert.get("extra_shyps", [])
    if shyps:
        lines.append(f"extra_shyps: {' '.join(str(v) for v in shyps)}".rstrip())
    return isabelle_observations.external_command_envelope(
        returncode=cert.get("returncode"),
        stdout="\n".join(lines),
        stderr=str(cert.get("stderr", "")),
    )


__all__ = [
    "IsabelleWarmGate",
    "CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME",
    "COLD_CROSSCHECK_DEGRADED_MARKER_FILENAME",
    "ISABELLE_APPROVED_ORACLES_FLOOR",
]

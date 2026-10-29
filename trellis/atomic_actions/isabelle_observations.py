"""Isabelle/HOL checker observations (B2a).

The Isabelle analogue of :mod:`trellis.atomic_actions.observations`. These
helpers gather raw facts for the Rust kernel and synthesize the two
envelopes the kernel deserializes:

* the ``ExternalCommandObservation`` shape (``{returncode, stdout, stderr,
  timed_out, spawn_error}``) consumed by the ``isabelle-check-node`` (the
  ``lean-compile-node`` analogue) and ``isabelle-thm-oracles`` (the
  ``print-axioms`` analogue) ops; and
* the ``LocalClosureProbeOutput`` cert shape (``{status, kernel_axioms,
  oracles_used, boundary_theorems, strict_theorem_deps,
  strict_definition_deps, errors, …}``) consumed by the
  ``isabelle-thm-deps`` (the ``local-closure-axioms`` analogue) op.

They do NOT decide validity, classify failures, or interpret the cert —
the Rust kernel's accept-gate (B2c-gate) does that.

Routing
-------
Mirrors ``observations.py``: when ``server_side is False`` (the worker /
CLI side) the op is socket-mandatory and routes through the AF_UNIX
checker server via :mod:`trellis.atomic_actions.isabelle_checker_client`;
when ``server_side is True`` the op drives the held
:class:`trellis.checker.isabelle_session.IsabelleSession` directly (the
supervisor-side endpoint the dispatcher invokes). The raw Isabelle TCP
port + password never leave the session object — exactly the
``bwrap_role`` recursion-guard split ``observations.py`` uses for lake.

For B2a no IsabelleHol tablet is live, so none of these run in
production; the testable core is the protocol + the envelope synthesis.
"""

from __future__ import annotations

import time
import uuid
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

# The acceptance sub-progress channel, shared with the Lean observations
# module (one implementation, one env var: `TRELLIS_ACCEPTANCE_PROGRESS_LOG`).
# `_progress_emit` appends a line to the log file the worker-side
# `_run_kernel_cli_once` tail-and-forward thread streams to the calling
# agent's stderr; it is a no-op when the env var is unset (direct CLI use,
# unit tests, and — note — the long-lived checker-server process, which is
# launched without the per-call env var; the worker-visible heartbeat for
# these ops therefore comes from the check.py client side in `cli.py`, while
# the emissions in this module surface wherever these functions run in a
# process that carries the env var).
from trellis.atomic_actions.observations import _progress_emit
from trellis.checker.isabelle_session import (
    CheckOutcome,
    IsabelleSession,
    IsabelleSessionError,
    cert_probe_theory_name,
    statement_hash_of,
    write_cert_probe_theory,
)

ISABELLE_SUPPORT_TIMEOUT_SECS = 3600.0

# The ``status`` values the kernel's ``parse_local_closure_response`` reads
# (mirrors the Lean local-closure script contract). ``ok`` = the probe ran
# and the theorem checked; ``internal_error`` = the probe could not produce
# a trustworthy verdict (transport/spawn/parse failure); ``invalid_*`` is
# reserved for proof failures (the theorem did not check).
STATUS_OK = "ok"
STATUS_INTERNAL_ERROR = "internal_error"
STATUS_INVALID_PROOF = "invalid_proof"


# --------------------------- envelope synthesis ---------------------------


def external_command_envelope(
    *,
    returncode: Optional[int],
    stdout: str = "",
    stderr: str = "",
    timed_out: bool = False,
    spawn_error: str = "",
) -> Dict[str, Any]:
    """Build the ``ExternalCommandObservation``-shaped JSON the kernel reads.

    ``returncode`` is ``Option<i32>`` kernel-side (``null`` only on a
    transport failure with no observed exit). For a node check the rule is
    ``0`` iff the build reported ``ok && failed == 0``.
    """
    return {
        "returncode": returncode,
        "stdout": stdout,
        "stderr": stderr,
        "timed_out": bool(timed_out),
        "spawn_error": spawn_error,
    }


def _pairs(names: Sequence[str], hash_field: str) -> List[Dict[str, str]]:
    """Render a name list as the kernel's ``{"name", "<hash_field>"}`` array.

    The Rust parser (``parse_local_closure_pairs``) expects each
    boundary/dep entry as an object carrying ``name`` plus a per-list hash
    field (``statement_hash`` / ``value_hash`` / ``semantic_hash``). For
    B2a the Isabelle cert does not yet compute per-dep content hashes, so
    the hash is the empty string (a stable placeholder the gate tolerates);
    B2d/B2-fingerprint can populate real hashes later without changing the
    wire shape.
    """
    return [{"name": n, hash_field: ""} for n in names]


def local_closure_cert_envelope(outcome: CheckOutcome) -> Dict[str, Any]:
    """Map a :class:`CheckOutcome` onto the ``LocalClosureProbeOutput`` cert.

    Field mapping (B2a):

    * ``oracles_used`` ← the ``thm_oracles`` writeln names. ``skip_proof``
      (the ``sorry`` oracle, Isabelle's ``Pure.skip_proof``) surfaces here;
      the B2c-gate rejects it. A clean proof yields an empty set.
    * ``kernel_axioms`` ← the ``thm_deps`` writeln names (the
      ``#print axioms`` analogue the approved-axiom audit bounds).
    * ``boundary_theorems`` / ``strict_theorem_deps`` /
      ``strict_definition_deps`` — empty for B2a (per-node Tablet
      dependency partitioning + content hashes is B2d/B2-fingerprint work;
      the wire shape is present so the gate parses cleanly).
    * ``status`` ← ``ok`` iff the theorem checked and exists; else a
      proof-failure / internal-error status the gate treats as not-passing.
    * ``theorem_exists`` ← the cert proves the named theorem is present
      (catches ``oops``/elision, which emit no ``theorem`` writeln).
    """
    theorem_exists = bool(outcome.theorem_exists)
    proof_ok = outcome.ok and outcome.failed == 0
    if proof_ok and theorem_exists:
        status = STATUS_OK
    elif getattr(outcome, "pre_node_failure", False):
        # `use_theories` never produced a snapshot for this theory (it failed
        # during import/dependency resolution). That is a TRANSPORT failure, not
        # a statement about the proof. Reporting it as `invalid_proof` presents a
        # fabricated refutation to the gate: the warm/cold cross-check compared a
        # clean warm certificate against it and halted a live run on a closure
        # "disagreement" that did not exist. `internal_error` still fails closed —
        # the gate never accepts on it — while naming the real cause.
        status = STATUS_INTERNAL_ERROR
    elif not proof_ok:
        status = STATUS_INVALID_PROOF
    else:
        # Built ok but the theorem the cert targeted is absent — treat as
        # an internal/elision error so the gate does not accept it.
        status = STATUS_INTERNAL_ERROR

    errors: List[str] = []
    if not proof_ok:
        errors.extend(outcome.error_lines)
        if not errors:
            errors.append(
                "isabelle use_theories reported the proof did not check "
                f"(ok={outcome.ok}, failed={outcome.failed})"
            )
    if proof_ok and not theorem_exists:
        errors.append(
            "certificate theorem absent from the checked theory "
            "(possible oops/elision)"
        )

    # Report the node's REAL principal kind rather than assuming "theorem". A
    # definition node's certificate is resolved against Isabelle's generated
    # `<node>_def` fact (see `run_cert_probe`), so labelling it "theorem" would
    # misdescribe what was certified to every downstream consumer.
    root_kind = (
        "definition"
        if outcome.cert_principal.endswith(_DEF_FACT_SUFFIX)
        else "theorem"
    )

    return {
        "status": status,
        "root_kind": root_kind,
        "kernel_axioms": list(outcome.dependencies),
        "oracles_used": list(outcome.oracles),
        "boundary_theorems": _pairs([], "statement_hash"),
        "strict_theorem_deps": _pairs([], "value_hash"),
        "strict_definition_deps": _pairs([], "semantic_hash"),
        "theorem_exists": theorem_exists,
        # B2c-gate Slice 1 (S3): the residual dangling sort hypotheses the
        # server extracted from the probe's ``Thm.extra_shyps``. NON-EMPTY ⇒
        # an empty/inconsistent-class vacuous-``False`` (oracle-blind); the
        # Slice-2 Rust gate rejects on it. ``statement_hash`` is the
        # SERVER-recomputed hash of the elaborated statement (gate #2 /
        # record-to-record stability; Correspondence I1 reuses
        # ``statement_repr``).
        "extra_shyps": list(outcome.extra_shyps),
        # "Closed relative to its children": `residual_oracles_used` are the
        # oracles this node's OWN proof introduces (the declared-child subproofs
        # cut away), and `boundary_theorems` are the declared dependencies whose
        # principal theorem the proof actually reached. `oracles_used` above stays
        # the TRANSITIVE union, so the two can be compared and nothing loosens
        # implicitly. Acceptance still reads the transitive set today; the gate
        # moves to the residual set only once fixtures confirm attribution.
        "residual_oracles_used": list(outcome.residual_oracles),
        "boundary_theorems_reached": list(outcome.boundary_theorems),
        "statement_hash": outcome.statement_hash,
        "statement_repr": outcome.statement_repr,
        # The `names_long` (qualified) statement print — the Correspondence
        # Tier-2 closure axis (I2) reshapes THIS field (cross-node constants
        # carry their `Tablet_<Dep>.` qualifier) into the corr payload. Additive
        # alongside the short `statement_repr` the SOUNDNESS lane reads; option
        # (A) reuses the landed `isabelle-thm-deps` cert as the corr producer's
        # input, so the field rides this same cert dict.
        "statement_repr_long": outcome.statement_repr_long,
        # The TYPE-SENSITIVE axis: `sha256` of the canonical
        # `ML_Syntax.print_term` serialization of `Thm.full_prop_of` tupled
        # with `Thm.hyps_of` / `Thm.extra_shyps`. ADDITIVE alongside
        # `statement_hash` / `statement_repr_long`, both of which stay
        # byte-unchanged. Every hash above digests a PRINT, and Isabelle's
        # printer prunes types by design, so a `nat` -> `'a` generalization
        # can leave all of them fixed; this one moves. Consumed by the corr
        # fingerprint's `statement_type_hash` component and by the warm-vs-cold
        # cross-check.
        "statement_type_hash": outcome.statement_type_hash,
        # DIAGNOSTIC ONLY (never hashed, never gated): the
        # `show_types`/`show_sorts` print, so a reviewer can SEE which type or
        # sort moved. The structural payload itself is not repeated here — it
        # is already on the wire verbatim in `stdout` (the `TRELLIS_STMT_STRUCT`
        # line), and `ML_Syntax.print_term` repeats the full type at every leaf
        # with no sharing, so duplicating it would be the single largest field
        # in the envelope for no new information.
        "statement_repr_typed": outcome.statement_repr_typed,
        "errors": errors,
        # Transport envelope (mirrors the Lean local-closure handler).
        "returncode": outcome.returncode,
        "stdout": outcome.stdout,
        "stderr": outcome.stderr,
        "timed_out": False,
        "spawn_error": "",
    }


def corr_statement_payload_envelope(outcome: CheckOutcome) -> Dict[str, Any]:
    """Map a :class:`CheckOutcome` onto the corr ``{ok, payload, error}`` shape.

    The Correspondence Tier-2 closure axis (I1/I2) reads the node's
    elaborated STATEMENT — ``Thm.prop_of`` printed with ``Name_Space.names_long``
    (so cross-node constants carry their ``Tablet_<Dep>.`` qualifier) and
    normalized (``normalize_statement_repr``, already applied in
    ``CheckOutcome.statement_repr_long``) — NOT the ``thm_deps`` proof closure.
    The read-only R1 experiment confirmed ``thm_deps`` shifts under a
    proof-only edit (the proof reaches different Pure inference rules) while
    ``Thm.prop_of`` is byte-stable across proof edits and moves only on a
    statement edit; a follow-on probe confirmed the DEFAULT (short) print hides
    cross-node constants, so the corr axis reads the LONG form.

    Field mapping — the ``LeanSemanticPayloadObservation`` shape the Rust corr
    producer deserializes (``runtime_cli_observations.rs`` ``{ok, payload,
    error}``):

    * ``payload`` ← the already-normalized ``statement_repr_long`` (the I1
      REUSE of the B2c-gate Slice-1 normalizer; NO new canonicalization here).
    * ``ok`` ← ``probe.ok && failed == 0 && theorem_exists && bool(payload)``.
      A missing/empty statement, an absent theorem (``oops``/elision), or a
      proof failure yields ``ok == False`` so the producer treats the payload
      as unavailable (its own ``ok || empty`` guard then declines to cache it,
      exactly as for a failed Lean elaboration).
    * ``error`` ← the first ``error``-kind line (the most actionable cause),
      empty on success.
    """
    proof_ok = outcome.ok and outcome.failed == 0
    payload = outcome.statement_repr_long
    ok = bool(proof_ok and outcome.theorem_exists and payload)
    error = outcome.error_lines[0] if outcome.error_lines else ""
    if not ok and not error:
        # Surface a deterministic cause when nothing failed loudly but the
        # payload is still unusable (e.g. a clean build whose cert theorem was
        # elided, so no statement was elaborated).
        if proof_ok and not outcome.theorem_exists:
            error = (
                "certificate theorem absent from the checked theory "
                "(possible oops/elision)"
            )
        elif proof_ok and not payload:
            error = "no elaborated statement (TRELLIS_STMT) was emitted"
    return {"ok": ok, "payload": payload, "error": error}


def _internal_error_corr_payload(message: str) -> Dict[str, Any]:
    """A fail-closed corr ``{ok, payload, error}`` for a transport failure.

    ``ok == False`` with an empty ``payload`` so the producer never feeds an
    unproducible statement into the closure axis (mirrors
    :func:`_internal_error_cert`).
    """
    return {"ok": False, "payload": "", "error": message}


def _internal_error_cert(message: str, *, timed_out: bool = False) -> Dict[str, Any]:
    """A fail-closed ``LocalClosureProbeOutput`` cert for a transport failure.

    ``status == internal_error`` with empty axiom/oracle sets so the gate
    never accepts a node whose cert could not be produced. ``returncode``
    is ``None`` (no observed proof outcome).
    """
    return {
        "status": STATUS_INTERNAL_ERROR,
        "root_kind": "other",
        "kernel_axioms": [],
        "oracles_used": [],
        "boundary_theorems": [],
        "strict_theorem_deps": [],
        "strict_definition_deps": [],
        "theorem_exists": False,
        # Fail-closed: empty shyps + empty statement hash so the gate cannot
        # mistake an unproducible cert for a clean/stable statement.
        "extra_shyps": [],
        "statement_hash": "",
        "statement_repr": "",
        "statement_repr_long": "",
        "statement_type_hash": "",
        "statement_repr_typed": "",
        "errors": [message],
        "returncode": None,
        "stdout": "",
        "stderr": message,
        "timed_out": bool(timed_out),
        "spawn_error": message,
    }


# --------------------------- server-side drivers ---------------------------
# These run on the supervisor side holding the session; the dispatcher in
# server.py calls them. They never touch the AF_UNIX socket (they ARE the
# authoritative side) and never expose the Isabelle port/password.


def check_node_server_side(
    session: IsabelleSession,
    *,
    master_dir: str,
    theory: str,
    cert_theorem: Optional[str] = None,
    timeout_secs: float = ISABELLE_SUPPORT_TIMEOUT_SECS,
) -> Dict[str, Any]:
    """``isabelle-check-node``: check one node theory, return the command shape.

    The ``ExternalCommandObservation`` envelope: ``returncode`` is 0 iff
    the proof checked (``ok && failed == 0``), else 1; a transport failure
    yields ``returncode: null`` + ``spawn_error``.
    """
    started = time.time()
    _progress_emit(f"[acceptance]   isabelle-check-node {theory}: starting")
    try:
        outcome = session.check_theory(
            master_dir=master_dir,
            theory=theory,
            cert_theorem=cert_theorem,
            timeout_secs=timeout_secs,
        )
    except IsabelleSessionError as exc:
        _progress_emit(
            f"[acceptance]   isabelle-check-node {theory}: failed "
            f"({exc.kind}) in {time.time() - started:.1f}s"
        )
        return external_command_envelope(
            returncode=None,
            stderr=exc.message,
            timed_out=(exc.kind == "timed_out"),
            spawn_error=exc.message,
        )
    _progress_emit(
        f"[acceptance]   isabelle-check-node {theory}: done "
        f"returncode={outcome.returncode} in {time.time() - started:.1f}s"
    )
    return external_command_envelope(
        returncode=outcome.returncode,
        stdout=outcome.stdout,
        stderr=outcome.stderr,
    )


class _ProbeWorkerFailed(Exception):
    """The worker node theory did not build clean (RC != 0).

    Carries the worker :class:`CheckOutcome` so the caller can surface the
    proof-failure cert directly (the probe is not run — its import of the
    failed worker theory would fail anyway, and the proof did not check).
    """

    def __init__(self, worker_outcome: CheckOutcome) -> None:
        super().__init__("worker node theory did not build clean")
        self.worker_outcome = worker_outcome


# Isabelle's generated name for a `definition`'s defining fact.
_DEF_FACT_SUFFIX = "_def"


def _principal_fact_undefined(outcome: CheckOutcome, qualified_thm: str) -> bool:
    """True iff the probe failed ONLY because `qualified_thm` does not exist.

    Isabelle reports `Undefined fact: "<name>"` once per reference; the probe
    names the principal four times (two outer commands + two ML antiquotations).
    We require EVERY error to be that one cause, so a node that additionally has
    a real proof failure is never silently retried under a different name — its
    original failure must stand.
    """
    errors = [line for line in (outcome.error_lines or []) if line.strip()]
    if not errors:
        return False
    return all(
        "Undefined fact:" in line and qualified_thm in line for line in errors
    )


def _tablet_boundary_candidates(master_dir: str, self_theory: str) -> List[str]:
    """Principal-theorem names of every OTHER tablet node, for the cut walk.

    The cut stops at the principal theorem of any registered tablet node — the
    same rule the Lean local-closure probe uses — so the candidate set is every
    sibling node theory present in the checker-owned session dir, not merely the
    node's direct imports (a proof may reach a grandchild's lemma directly).

    Both principal spellings are offered because a node may be theorem-like
    (`Tablet_X.X`) or a definition (`Tablet_X.X_def`); the walk matches whichever
    the proof actually references. Checker scratch (`__Cert` probes, `__In`
    aliases) and the node itself are excluded.

    This list is CHECKER-OWNED: it is derived from the socket-trusted session
    directory, never from worker text. A recorded boundary is only meaningful
    after the kernel validates it against the node's declared dependencies.
    """
    names: List[str] = []
    try:
        entries = sorted(Path(master_dir).glob("Tablet_*.thy"))
    except OSError:
        return names
    for path in entries:
        stem = path.stem
        if stem == self_theory or "__Cert" in stem or "__In" in stem:
            continue
        if stem == "Tablet_Preamble":
            continue
        node = stem[len("Tablet_"):]
        names.append(f"{stem}.{node}")
        names.append(f"{stem}.{node}{_DEF_FACT_SUFFIX}")
    return names


def run_cert_probe(
    session: IsabelleSession,
    *,
    master_dir: str,
    theory: str,
    cert_theorem: str,
    qualified_thm: Optional[str] = None,
    timeout_secs: float = ISABELLE_SUPPORT_TIMEOUT_SECS,
) -> CheckOutcome:
    """The S1 two-step: build the worker theory, then the CHECKER-OWNED probe.

    1. ``use_theories`` the WORKER node theory ``theory`` (``Tablet_<Node>``)
       — this is the authoritative build-RC under ``quick_and_dirty=false``
       (gate #1, the robust ``sorry``/``\\<proof>`` defense). If it did not
       build clean, raise :class:`_ProbeWorkerFailed` so the caller surfaces
       the proof-failure cert (the probe import would fail regardless).
    2. Construct the SERVER-side fully-qualified principal name
       ``Tablet_<Node>.<node>`` (``theory`` ``.`` ``cert_theorem``), WRITE
       the checker-owned probe theory ``Tablet_<Node>__Cert.thy``, and
       ``use_theories`` IT. The probe references the theorem by qualified
       name in CHECKER text and emits ``thm_oracles``/``thm_deps``/
       ``TRELLIS_SHYPS``/``TRELLIS_STMT`` — the certificate the worker can
       neither author, omit, nor redirect.

    Returns the PROBE's :class:`CheckOutcome`, with ``theorem_exists``
    strengthened: a clean probe build that emitted a non-empty
    ``TRELLIS_STMT`` is definitive existence proof (the ``@{thm <qualified>}``
    antiquotation only compiles if the qualified fact resolves; a shadow /
    wrong-name worker makes the probe FAIL to build).
    """
    # H1 (warm-residency-staleness): on a held-open WARM session elaborate the
    # worker node under a fresh CONTENT-KEYED alias theory (``<node>__In_<sha>``)
    # so a worker proof-REPLACING edit between bursts (clean→``sorry``) is
    # re-elaborated FRESH rather than read residency-stale (the live ``Subcritical``
    # halt: a stale clean step-1 build let the probe certify a now-``sorry`` node
    # clean). A ``purge_theories``+re-load of the same node name CORRUPTS the
    # held-open document ("Illegal theory header", live-confirmed 2025-2), so a
    # fresh name — the same defence the probe uses — is the viable mechanism.
    # Unchanged bytes map to the same alias (reused warm; no re-elaboration). On a
    # cold/flag-OFF session this returns ``theory`` unchanged (byte-identical).
    # The probe then imports the ALIAS and references the principal by the
    # alias-qualified name so BOTH the build-RC gate and the certified ``thm`` see
    # the current content. Defensive ``getattr``: a session-like object without
    # the capability (older mock) degrades to the node name (pre-H1 behavior).
    _fresh = getattr(session, "fresh_inflight_theory", None)
    build_theory = (
        _fresh(master_dir=master_dir, theory=theory) if callable(_fresh) else theory
    )
    worker_started = time.time()
    _progress_emit(
        f"[acceptance]     cert-probe {theory}: worker theory build starting"
    )
    worker = session.check_theory(
        master_dir=master_dir,
        theory=build_theory,
        cert_theorem=cert_theorem,
        timeout_secs=timeout_secs,
    )
    _progress_emit(
        f"[acceptance]     cert-probe {theory}: worker theory build done "
        f"returncode={worker.returncode} in {time.time() - worker_started:.1f}s"
    )
    if not (worker.ok and worker.failed == 0):
        # Relabel the alias back to the worker node so the surfaced
        # proof-failure cert names the real node, not the internal alias.
        if build_theory != theory:
            worker.theory_name = theory
        raise _ProbeWorkerFailed(worker)

    # SERVER-constructed qualified name. When an explicit ``qualified_thm`` was
    # passed for the real node name it must be re-targeted at the alias the node
    # actually elaborated under (so ``@{thm <qualified>}`` resolves); otherwise
    # derive ``<build_theory>.<node>``. The shape gate makes the principal
    # ``<node>`` unique within its theory so it is unambiguous. Either way it is
    # server text, never worker input.
    if qualified_thm is None or build_theory != theory:
        qualified_thm = f"{build_theory}.{cert_theorem}"
    # On a held-open WARM session, give the probe a UNIQUE theory name per call:
    # PIDE retains stale document state for a REUSED theory name across
    # ``use_theories`` calls, so re-probing the same node a second time on the
    # warm session with the fixed ``__Cert`` name fails to re-elaborate
    # ("Illegal theory header"; live-confirmed). A fresh name sidesteps it. The
    # cold one-shot path (session discarded) keeps the stable fixed name.
    nonce: Optional[str] = None
    if getattr(session, "warm_prefix_enabled", False):
        nonce = uuid.uuid4().hex[:12]
    # The probe IMPORTS the (possibly aliased) ``build_theory`` so the certified
    # ``thm`` is the freshly-elaborated one. Its name is derived from
    # ``build_theory`` so a content change (new alias) also gets a distinct probe.
    probe_theory = cert_probe_theory_name(build_theory, nonce=nonce)
    probe_path = write_cert_probe_theory(
        Path(master_dir),
        build_theory,
        qualified_thm,
        probe_theory=probe_theory,
        boundary_thms=_tablet_boundary_candidates(master_dir, theory),
    )
    probe_started = time.time()
    _progress_emit(
        f"[acceptance]     cert-probe {theory}: checker probe build starting "
        f"({probe_theory})"
    )
    try:
        probe = session.check_theory(
            master_dir=master_dir,
            theory=probe_theory,
            cert_theorem=cert_theorem,
            timeout_secs=timeout_secs,
        )
        _progress_emit(
            f"[acceptance]     cert-probe {theory}: checker probe build done "
            f"returncode={probe.returncode} in {time.time() - probe_started:.1f}s"
        )
    finally:
        # H3: delete the probe ``.thy`` once its cert is read so probe theories
        # never accumulate in ``session_dir`` (a probe can never outlive into a
        # later ROOT enumeration / the cold build's session / the warm prefix —
        # the scaffold sweep + the ``*__Cert*`` enumeration guard are the on-disk
        # backstops).
        #
        # We deliberately do NOT ``purge_theories`` the probe off a held-open
        # WARM document: live-confirmed, purging a cert probe (which ``imports``
        # the node) ALSO invalidates the node in the document, so the NEXT
        # ``use_theories`` of the node fails "Illegal theory header". With the
        # per-call UNIQUE probe name above there is no name-reuse collision, so
        # leaving the (tiny) probe theory resident is harmless and correct; the
        # deleted file means it can never re-enter any build. (A very long run
        # accrues one small resident probe theory per cert check; the warm
        # session is rebuilt from cold on any checker restart/rewind, which
        # bounds it.)
        try:
            probe_path.unlink()
        except OSError:
            pass
    # Strengthen existence: a clean probe build + a real elaborated statement
    # proves the qualified fact resolved (robust to the R2 print-form
    # question of whether the ``theorem`` line is qualified or bare).
    probe.theorem_exists = bool(
        probe.ok and probe.failed == 0
        and (probe.theorem_exists or probe.statement_repr)
    )
    probe.cert_principal = qualified_thm

    # DEFINITION nodes: resolve the principal fact Isabelle actually declared.
    #
    # The probe names the principal `Tablet_<Node>.<node>`, which is right for a
    # THEOREM node. A `definition` declares a CONSTANT; Isabelle names its
    # defining fact `<node>_def` and declares NO theorem of the bare name, so
    # the probe asks for a fact that cannot exist and the cert fails closed as
    # `invalid_proof` — for every definition node, permanently. Live-confirmed:
    # `thm_oracles Tablet_GnpProbability.GnpProbability` → `Undefined fact`,
    # while `..._def` resolves with an empty oracle set.
    #
    # We do NOT guess the node's kind from its source (that would duplicate the
    # kernel's Isabelle tokenizer, and a `.tex`-derived kind is not authoritative
    # about what the `.thy` declared). Instead we let ISABELLE answer: only when
    # it reports the named fact undefined do we retry the SERVER-CONSTRUCTED
    # `_def` name. This mirrors Lean, whose `#print axioms <node>` resolves
    # whatever declaration the node introduced without consulting a kind.
    #
    # Trust: both names are built server-side from the socket-derived theory and
    # node; no worker text selects the certified fact. If `_def` is also absent
    # the original failure stands, so the gate still fails closed.
    if (
        qualified_thm
        and not probe.theorem_exists
        and not cert_theorem.endswith(_DEF_FACT_SUFFIX)
        and _principal_fact_undefined(probe, qualified_thm)
    ):
        _progress_emit(
            f"[acceptance]     cert-probe {theory}: principal fact undefined; "
            f"retrying certificate as {cert_theorem}{_DEF_FACT_SUFFIX}"
        )
        return run_cert_probe(
            session,
            master_dir=master_dir,
            theory=theory,
            cert_theorem=f"{cert_theorem}{_DEF_FACT_SUFFIX}",
            qualified_thm=f"{qualified_thm}{_DEF_FACT_SUFFIX}",
            timeout_secs=timeout_secs,
        )
    # H1 alias de-leak: when the node was elaborated under a content-keyed alias
    # (``<node>__In_<sha>``), the ``Name_Space.names_long`` print qualifies the
    # node's OWN locally-defined constants with the ALIAS theory name
    # (``Tablet_Node__In_<sha>.cg`` vs the cold build's ``Tablet_Node.cg``). That
    # qualifier feeds ``statement_repr_long``, which the warm-vs-cold cross-check
    # compares — so an un-normalized alias would itself trip a SPURIOUS halt.
    # Rewrite the alias qualifier back to the real node name so the warm cert is
    # name-identical to a cold build's (the SHORT ``statement_repr`` is already
    # alias-free — constants print unqualified — so the ``statement_hash`` is
    # unaffected; ``oracles``/``dependencies`` carry no node qualifier). A no-op
    # on the cold path (``build_theory == theory``).
    #
    # The STRUCTURAL payload has the same exposure and worse: `Term.Const`
    # names in `Term.term` are ALWAYS the internal LONG names (`term.ML:230`),
    # so the alias qualifier leaks into `statement_type_repr`
    # UNCONDITIONALLY — there is no short-name escape hatch the way there is
    # for `statement_repr`. Without this rewrite every warm probe's type
    # digest would differ from a cold build's and the warm-vs-cold cross-check
    # (which now compares `statement_type_hash`) would HALT on every node. The
    # hash is RECOMPUTED from the rewritten repr, not carried over.
    if build_theory != theory:
        if probe.statement_repr_long:
            probe.statement_repr_long = probe.statement_repr_long.replace(
                build_theory, theory
            )
        if probe.statement_type_repr:
            probe.statement_type_repr = probe.statement_type_repr.replace(
                build_theory, theory
            )
            probe.statement_type_hash = statement_hash_of(probe.statement_type_repr)
        if probe.statement_repr_typed:
            probe.statement_repr_typed = probe.statement_repr_typed.replace(
                build_theory, theory
            )
        if probe.dependencies:
            probe.dependencies = [
                dep.replace(build_theory, theory) for dep in probe.dependencies
            ]
    return probe


def thm_deps_server_side(
    session: IsabelleSession,
    *,
    master_dir: str,
    theory: str,
    cert_theorem: str,
    qualified_thm: Optional[str] = None,
    timeout_secs: float = ISABELLE_SUPPORT_TIMEOUT_SECS,
) -> Dict[str, Any]:
    """``isabelle-thm-deps``: build worker + probe, cert FROM THE PROBE (S1).

    The certificate (oracles / extra_shyps / statement / theorem_exists /
    deps) is built from the CHECKER-OWNED probe outcome, never from
    worker-authored cert lines. A worker proof failure surfaces as an
    ``invalid_proof`` cert; a probe/transport failure fails closed.
    """
    started = time.time()
    _progress_emit(
        f"[acceptance]   isabelle-thm-deps {theory}: cert probe starting"
    )
    try:
        probe = run_cert_probe(
            session,
            master_dir=master_dir,
            theory=theory,
            cert_theorem=cert_theorem,
            qualified_thm=qualified_thm,
            timeout_secs=timeout_secs,
        )
    except _ProbeWorkerFailed as failed:
        # The worker proof did not check — surface the proof-failure cert.
        cert = local_closure_cert_envelope(failed.worker_outcome)
        _progress_emit(
            f"[acceptance]   isabelle-thm-deps {theory}: done "
            f"status={cert.get('status')} (worker proof failed) "
            f"in {time.time() - started:.1f}s"
        )
        return cert
    except IsabelleSessionError as exc:
        _progress_emit(
            f"[acceptance]   isabelle-thm-deps {theory}: failed "
            f"({exc.kind}) in {time.time() - started:.1f}s"
        )
        return _internal_error_cert(
            f"isabelle thm-deps probe failed: {exc.message}",
            timed_out=(exc.kind == "timed_out"),
        )
    cert = local_closure_cert_envelope(probe)
    _progress_emit(
        f"[acceptance]   isabelle-thm-deps {theory}: done "
        f"status={cert.get('status')} in {time.time() - started:.1f}s"
    )
    return cert


def corr_statement_payload_server_side(
    session: IsabelleSession,
    *,
    master_dir: str,
    theory: str,
    cert_theorem: str,
    qualified_thm: Optional[str] = None,
    timeout_secs: float = ISABELLE_SUPPORT_TIMEOUT_SECS,
) -> Dict[str, Any]:
    """The Correspondence Tier-2 statement payload, FROM THE PROBE (I1).

    Builds the worker + the CHECKER-OWNED probe (the same S1 two-step
    :func:`thm_deps_server_side` uses, so the statement is the
    server-extracted ``TRELLIS_STMT``, never a worker-authored line) and
    renders the ``{ok, payload, error}`` corr shape. A worker proof failure
    surfaces as ``ok == False`` with the proof-failure cause; a probe/transport
    failure fails closed.
    """
    started = time.time()
    _progress_emit(
        f"[acceptance]   isabelle-corr-payload {theory}: cert probe starting"
    )
    try:
        probe = run_cert_probe(
            session,
            master_dir=master_dir,
            theory=theory,
            cert_theorem=cert_theorem,
            qualified_thm=qualified_thm,
            timeout_secs=timeout_secs,
        )
    except _ProbeWorkerFailed as failed:
        payload = corr_statement_payload_envelope(failed.worker_outcome)
        _progress_emit(
            f"[acceptance]   isabelle-corr-payload {theory}: done "
            f"ok={payload.get('ok')} (worker proof failed) "
            f"in {time.time() - started:.1f}s"
        )
        return payload
    except IsabelleSessionError as exc:
        _progress_emit(
            f"[acceptance]   isabelle-corr-payload {theory}: failed "
            f"({exc.kind}) in {time.time() - started:.1f}s"
        )
        return _internal_error_corr_payload(
            f"isabelle corr-payload probe failed: {exc.message}"
        )
    payload = corr_statement_payload_envelope(probe)
    _progress_emit(
        f"[acceptance]   isabelle-corr-payload {theory}: done "
        f"ok={payload.get('ok')} in {time.time() - started:.1f}s"
    )
    return payload


def thm_oracles_server_side(
    session: IsabelleSession,
    *,
    master_dir: str,
    theory: str,
    cert_theorem: str,
    qualified_thm: Optional[str] = None,
    timeout_secs: float = ISABELLE_SUPPORT_TIMEOUT_SECS,
) -> Dict[str, Any]:
    """``isabelle-thm-oracles``: the ``print-axioms`` analogue, FROM THE PROBE.

    The kernel reads this op as an ``ExternalCommandObservation`` whose
    ``stdout`` it scans for the oracle/axiom surface. We build the worker +
    the CHECKER-OWNED probe (S1) and render the PROBE's oracle + dependency
    lines into ``stdout`` so the existing string-scanning consumer sees a
    stable, server-authoritative surface. ``returncode`` is 0 iff BOTH the
    worker proof and the probe checked clean.
    """
    started = time.time()
    _progress_emit(
        f"[acceptance]   isabelle-thm-oracles {theory}: cert probe starting"
    )
    try:
        probe = run_cert_probe(
            session,
            master_dir=master_dir,
            theory=theory,
            cert_theorem=cert_theorem,
            qualified_thm=qualified_thm,
            timeout_secs=timeout_secs,
        )
    except _ProbeWorkerFailed as failed:
        worker = failed.worker_outcome
        _progress_emit(
            f"[acceptance]   isabelle-thm-oracles {theory}: done "
            f"returncode={worker.returncode} (worker proof failed) "
            f"in {time.time() - started:.1f}s"
        )
        return external_command_envelope(
            returncode=worker.returncode,
            stdout="",
            stderr=worker.stderr,
        )
    except IsabelleSessionError as exc:
        _progress_emit(
            f"[acceptance]   isabelle-thm-oracles {theory}: failed "
            f"({exc.kind}) in {time.time() - started:.1f}s"
        )
        return external_command_envelope(
            returncode=None,
            stderr=exc.message,
            timed_out=(exc.kind == "timed_out"),
            spawn_error=exc.message,
        )
    _progress_emit(
        f"[acceptance]   isabelle-thm-oracles {theory}: done "
        f"returncode={probe.returncode} in {time.time() - started:.1f}s"
    )
    stdout_lines: List[str] = []
    stdout_lines.append(f"oracles: {' '.join(probe.oracles)}".rstrip())
    stdout_lines.append(f"depends on axioms: {' '.join(probe.dependencies)}".rstrip())
    if probe.extra_shyps:
        stdout_lines.append(f"extra_shyps: {' '.join(probe.extra_shyps)}".rstrip())
    return external_command_envelope(
        returncode=probe.returncode,
        stdout="\n".join(stdout_lines),
        stderr=probe.stderr,
    )


__all__ = [
    "ISABELLE_SUPPORT_TIMEOUT_SECS",
    "STATUS_OK",
    "STATUS_INTERNAL_ERROR",
    "STATUS_INVALID_PROOF",
    "external_command_envelope",
    "local_closure_cert_envelope",
    "corr_statement_payload_envelope",
    "check_node_server_side",
    "run_cert_probe",
    "thm_deps_server_side",
    "corr_statement_payload_server_side",
    "thm_oracles_server_side",
]

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

from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence

from trellis.checker.isabelle_session import (
    CheckOutcome,
    IsabelleSession,
    IsabelleSessionError,
    cert_probe_theory_name,
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

    return {
        "status": status,
        "root_kind": "theorem",
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
        "statement_hash": outcome.statement_hash,
        "statement_repr": outcome.statement_repr,
        "errors": errors,
        # Transport envelope (mirrors the Lean local-closure handler).
        "returncode": outcome.returncode,
        "stdout": outcome.stdout,
        "stderr": outcome.stderr,
        "timed_out": False,
        "spawn_error": "",
    }


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
    try:
        outcome = session.check_theory(
            master_dir=master_dir,
            theory=theory,
            cert_theorem=cert_theorem,
            timeout_secs=timeout_secs,
        )
    except IsabelleSessionError as exc:
        return external_command_envelope(
            returncode=None,
            stderr=exc.message,
            timed_out=(exc.kind == "timed_out"),
            spawn_error=exc.message,
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
    worker = session.check_theory(
        master_dir=master_dir,
        theory=theory,
        cert_theorem=cert_theorem,
        timeout_secs=timeout_secs,
    )
    if not (worker.ok and worker.failed == 0):
        raise _ProbeWorkerFailed(worker)

    # SERVER-constructed qualified name (the caller may pass it explicitly;
    # otherwise derive ``Tablet_<Node>.<node>``). The shape gate makes the
    # principal ``<node>`` unique within ``Tablet_<Node>`` so it is
    # unambiguous. Either way it is server text, never worker input.
    if qualified_thm is None:
        qualified_thm = f"{theory}.{cert_theorem}"
    write_cert_probe_theory(Path(master_dir), theory, qualified_thm)
    probe_theory = cert_probe_theory_name(theory)
    probe = session.check_theory(
        master_dir=master_dir,
        theory=probe_theory,
        cert_theorem=cert_theorem,
        timeout_secs=timeout_secs,
    )
    # Strengthen existence: a clean probe build + a real elaborated statement
    # proves the qualified fact resolved (robust to the R2 print-form
    # question of whether the ``theorem`` line is qualified or bare).
    probe.theorem_exists = bool(
        probe.ok and probe.failed == 0
        and (probe.theorem_exists or probe.statement_repr)
    )
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
        return local_closure_cert_envelope(failed.worker_outcome)
    except IsabelleSessionError as exc:
        return _internal_error_cert(
            f"isabelle thm-deps probe failed: {exc.message}",
            timed_out=(exc.kind == "timed_out"),
        )
    return local_closure_cert_envelope(probe)


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
        return external_command_envelope(
            returncode=worker.returncode,
            stdout="",
            stderr=worker.stderr,
        )
    except IsabelleSessionError as exc:
        return external_command_envelope(
            returncode=None,
            stderr=exc.message,
            timed_out=(exc.kind == "timed_out"),
            spawn_error=exc.message,
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
    "check_node_server_side",
    "run_cert_probe",
    "thm_deps_server_side",
    "thm_oracles_server_side",
]

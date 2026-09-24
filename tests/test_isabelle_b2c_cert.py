"""B2c-gate Slice 1 — ADVERSARIAL soundness tests (fake server; NO live Isabelle).

The proof author is a possibly-adversarial worker. These tests assert that
the SERVER-side certificate injection (the S1/S3/S7 must-fix) produces an
envelope the Slice-2 Rust accept-gate will REJECT for every cheat, and that
the certificate comes from a CHECKER-OWNED probe theory the worker can
neither author, omit, nor redirect.

Everything here runs against the in-process ``_FakeIsabelleServer`` (the
wire-verified framing) — no real ``isabelle`` is spawned. The opt-in
``@pytest.mark.isabelle_live`` cross-theory-probe confirmation lives in
``test_isabelle_session.py`` / the live harness.

What the Rust gate (Slice 2) will reject on, asserted at the cert envelope:
  * ``oracles_used`` non-empty (PRIMARY — catches ``smt``/``z3`` the build
    misses; and ``skip_proof`` belt-and-suspenders);
  * ``extra_shyps`` non-empty (S3 — empty/inconsistent-class vacuous-False);
  * ``status != ok`` (build-RC0 / theorem existence — wrong-name shadow);
  * ``returncode != 0`` (build-RC, the robust ``sorry`` defense).
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Dict, List, Optional

import pytest

from trellis.atomic_actions import isabelle_observations as iso
from trellis.checker import isabelle_session as isa
from trellis.checker.isabelle_session import (
    IsabelleSession,
    IsabelleSessionError,
)

# Reuse the wire-framing fake-server harness + the fake servers.db fixture.
from tests.test_isabelle_session import (  # noqa: F401  (fake_db is a fixture)
    _FakeIsabelleServer,
    _make_handler,
    _node,
    _session_against_fake,
    fake_db,
)


# --------------------------- scripted-server helpers ---------------------------


def _is_probe(theory: str) -> bool:
    """The checker-owned probe theory is ``Tablet_<Node>__Cert``."""
    return theory.endswith("__Cert")


def _two_phase_handler(
    *,
    worker_node: dict,
    probe_node: Optional[dict],
):
    """A ``use_theories`` handler that answers the WORKER and PROBE phases.

    ``run_cert_probe`` issues two ``use_theories`` calls: first the worker
    node theory, then (only if the worker built clean) the ``__Cert`` probe
    theory. This dispatches on the requested theory name so each phase gets
    its own scripted node, faithfully modelling the server's two-step.
    """

    from tests.test_isabelle_session import _frame_block

    def use_theories(arg):
        theories = (arg or {}).get("theories", [])
        theory = theories[0] if theories else ""
        if _is_probe(theory):
            node = probe_node if probe_node is not None else worker_node
        else:
            node = worker_node
        ok = bool(node.get("status", {}).get("ok", False)) and (
            int(node.get("status", {}).get("failed", 0)) == 0
        )
        return [_frame_block("FINISHED", {"ok": ok, "nodes": [node]})]

    return _make_handler(use_theories_reply=use_theories)


def _writeln(*lines: str) -> List[dict]:
    return [{"kind": "writeln", "message": m} for m in lines]


def _run_thm_deps(
    fake_db,
    *,
    name: str,
    worker_node: dict,
    probe_node: Optional[dict],
    node: str = "Foo",
):
    """Drive ``thm_deps_server_side`` (the cert op) end-to-end against the fake.

    Returns the cert dict. The session is the real ``IsabelleSession`` with
    spawn/reap/version-pin neutered (``_session_against_fake``) so the
    protocol + the two-step probe logic run unchanged against the fake wire.
    """
    server = _FakeIsabelleServer(
        _two_phase_handler(worker_node=worker_node, probe_node=probe_node)
    )
    server.start()
    sess = _session_against_fake(name, server, fake_db)
    # The reap shell-out (subprocess.run) must not hit a real isabelle.
    import subprocess

    orig_run = isa.subprocess.run
    isa.subprocess.run = lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    theory = f"Tablet_{node}"
    try:
        sess.start()
        # master_dir is a throwaway scratch UNDER the fake-db tmp dir so the
        # probe theory write never lands in /tmp.
        master = Path(isa.servers_db_path()).parent / "isa-master"
        master.mkdir(parents=True, exist_ok=True)
        return iso.thm_deps_server_side(
            sess,
            master_dir=str(master),
            theory=theory,
            cert_theorem=node,
            qualified_thm=f"{theory}.{node}",
            timeout_secs=10.0,
        )
    finally:
        isa.subprocess.run = orig_run
        sess.close()
        server.stop()


# =============================== 1. sorry-of-False ===============================


def test_sorry_of_false_build_fails_returncode_nonzero(fake_db) -> None:
    """Defense #1 (build-RC, the robust ``sorry`` defense): a ``sorry``-of-
    ``False`` worker FAILS the build under ``quick_and_dirty=false`` (B2d
    audit S2) ⇒ ``ok:false``/``failed:1`` ⇒ the cert returncode is non-zero
    and the status is not ``ok``. The probe is never reached (its import of
    the failed worker theory would fail anyway)."""
    worker = _node(
        theory_name="Draft.Tablet_Foo",
        failed=1,
        finished=3,
        ok=False,
        messages=[{"kind": "error", "message": "Failed to finish proof:\n 1. False"}],
    )
    cert = _run_thm_deps(fake_db, name="sorry-build", worker_node=worker, probe_node=None)
    assert cert["returncode"] != 0
    assert cert["status"] != iso.STATUS_OK
    assert cert["oracles_used"] == []  # worker theory has no cert lines


def test_sorry_skip_proof_oracle_caught_by_probe(fake_db) -> None:
    """Defense #2 (oracle gate, belt-and-suspenders): even in the
    hypothetical where a ``sorry`` did NOT fail the build (a quick_and_dirty
    bypass / the 2025-1 bug the version pin forbids), the SERVER-injected
    probe surfaces ``skip_proof`` ⇒ ``oracles_used == {"skip_proof"}`` and
    the gate rejects on the non-empty oracle set."""
    worker = _node(theory_name="Draft.Tablet_Foo", failed=0, ok=True)
    probe = _node(
        theory_name="Draft.Tablet_Foo__Cert",
        failed=0,
        ok=True,
        messages=_writeln(
            "theorem Tablet_Foo.Foo: False",
            "oracles:\n    skip_proof",
            "dependencies: 1\n    refl",
            "TRELLIS_SHYPS ",
            "TRELLIS_STMT Foo.Foo \\<equiv> Trueprop False",
        ),
    )
    cert = _run_thm_deps(fake_db, name="sorry-oracle", worker_node=worker, probe_node=probe)
    assert set(cert["oracles_used"]) == {"skip_proof"}


# =============================== 2. smt / z3 ===============================


def test_smt_oracle_caught_even_when_build_ok(fake_db) -> None:
    """The oracle gate's IRREPLACEABLE job: an ``smt``/``z3`` proof BUILDS
    CLEAN (the build does NOT catch it) but the SERVER-injected probe
    surfaces the solver oracle ⇒ ``oracles_used`` is non-empty ⇒ reject.
    Build-RC alone would have accepted this."""
    worker = _node(theory_name="Draft.Tablet_Foo", failed=0, ok=True)
    probe = _node(
        theory_name="Draft.Tablet_Foo__Cert",
        failed=0,
        ok=True,
        messages=_writeln(
            "theorem Tablet_Foo.Foo: P",
            "oracles:\n    z3\n    smt",
            "dependencies: 2\n    refl\n    conjI",
            "TRELLIS_SHYPS ",
            "TRELLIS_STMT Foo.Foo \\<equiv> Trueprop P",
        ),
    )
    cert = _run_thm_deps(fake_db, name="smt", worker_node=worker, probe_node=probe)
    assert cert["status"] == iso.STATUS_OK  # the proof DID 'check' under build
    assert cert["oracles_used"]  # non-empty ⇒ the gate rejects
    assert "z3" in cert["oracles_used"] and "smt" in cert["oracles_used"]


# =============================== 3. extra_shyps-False ===============================


def test_extra_shyps_nonempty_caught(fake_db) -> None:
    """S3 (oracle-blind): an empty/inconsistent type class lets a worker
    prove ``False`` oracle-free; it is visible ONLY via ``Thm.extra_shyps``.
    The probe's ``TRELLIS_SHYPS`` carries a non-empty sort payload ⇒
    ``extra_shyps != []`` ⇒ reject — even though ``oracles_used`` is empty."""
    worker = _node(theory_name="Draft.Tablet_Foo", failed=0, ok=True)
    probe = _node(
        theory_name="Draft.Tablet_Foo__Cert",
        failed=0,
        ok=True,
        messages=_writeln(
            "theorem Tablet_Foo.Foo: False",
            "oracles:",  # EMPTY oracle set — the oracle gate is blind here
            "dependencies: 1\n    refl",
            "TRELLIS_SHYPS {empty}",
            "TRELLIS_STMT Foo.Foo \\<equiv> Trueprop False",
        ),
    )
    cert = _run_thm_deps(fake_db, name="shyps", worker_node=worker, probe_node=probe)
    assert cert["oracles_used"] == []  # the oracle gate alone would MISS this
    assert cert["extra_shyps"] != []  # but extra_shyps catches it ⇒ reject
    assert "{empty}" in cert["extra_shyps"]


# =============================== 4. shadow / wrong-theorem ===============================


def test_wrong_theorem_name_probe_fails_status_not_ok(fake_db) -> None:
    """The cert is pinned to the SERVER name ``Tablet_<Node>.<node>``. A
    worker that names its fact ``<other>`` makes the probe's
    ``@{thm Tablet_Foo.Foo}`` antiquotation fail to resolve ⇒ the probe
    theory FAILS to build (no ``TRELLIS_STMT``) ⇒ ``theorem_exists`` False
    and ``status != ok``. The worker cannot redirect the cert at a trivial
    shadow theorem."""
    worker = _node(theory_name="Draft.Tablet_Foo", failed=0, ok=True)
    # Probe FAILS to build: the qualified name didn't resolve (worker named
    # the lemma something else), so the ML antiquotation errored.
    probe = _node(
        theory_name="Draft.Tablet_Foo__Cert",
        failed=1,
        finished=0,
        ok=False,
        messages=[{"kind": "error",
                   "message": "Undefined fact: \"Tablet_Foo.Foo\""}],
    )
    cert = _run_thm_deps(fake_db, name="shadow", worker_node=worker, probe_node=probe)
    assert cert["status"] != iso.STATUS_OK
    assert cert["theorem_exists"] is False
    assert cert["statement_hash"] == ""  # no statement extracted


# =============================== 5. cert-omission (core S1) ===============================


def test_cert_omission_server_injects_probe(fake_db, tmp_path: Path) -> None:
    """CORE S1: the worker NODE theory carries NO cert commands (the S6 shape
    gate forbids ``thm_oracles``/``ML`` in node theories). The SERVER STILL
    injects the certificate — it WRITES a checker-owned ``__Cert`` probe
    theory and reads oracles/shyps/statement from IT. The certificate is
    authoritative regardless of worker text.

    Asserted two ways: (a) the probe theory file is physically written by the
    server; (b) the probe's clean ``TRELLIS_STMT`` + empty oracles/shyps make
    a CLEAN accept-eligible cert — sourced entirely from the probe."""
    worker = _node(theory_name="Draft.Tablet_Foo", failed=0, ok=True)
    probe = _node(
        theory_name="Draft.Tablet_Foo__Cert",
        failed=0,
        ok=True,
        messages=_writeln(
            "theorem Tablet_Foo.Foo: 1 + 1 = 2",
            "oracles:",
            "dependencies: 2\n    One_nat_def\n    add.commute",
            "TRELLIS_SHYPS ",
            "TRELLIS_STMT Foo.Foo \\<equiv> Trueprop (1 + 1 = 2)",
        ),
    )

    # Drive against a known master dir so we can assert the probe FILE exists.
    server = _FakeIsabelleServer(
        _two_phase_handler(worker_node=worker, probe_node=probe)
    )
    server.start()
    sess = _session_against_fake("cert-omit", server, fake_db)
    import subprocess

    orig_run = isa.subprocess.run
    isa.subprocess.run = lambda *a, **k: subprocess.CompletedProcess(a, 0, "", "")
    master = tmp_path / "isa"
    master.mkdir()
    # H3 deletes the probe ``.thy`` in a ``finally`` after the cert is read (so
    # probe theories never accumulate), so capture the probe text AT WRITE TIME
    # to still assert the server authored the correct checker-owned probe.
    captured_probe: Dict[str, str] = {}
    orig_write = iso.write_cert_probe_theory

    def _spy_write(
        master_dir, node_theory, qualified_thm, *, probe_theory=None, boundary_thms=None
    ):
        p = orig_write(
            master_dir,
            node_theory,
            qualified_thm,
            probe_theory=probe_theory,
            boundary_thms=boundary_thms,
        )
        captured_probe["path"] = str(p)
        captured_probe["text"] = p.read_text(encoding="utf-8")
        return p

    iso.write_cert_probe_theory = _spy_write
    try:
        sess.start()
        cert = iso.thm_deps_server_side(
            sess,
            master_dir=str(master),
            theory="Tablet_Foo",
            cert_theorem="Foo",
            qualified_thm="Tablet_Foo.Foo",
            timeout_secs=10.0,
        )
    finally:
        isa.subprocess.run = orig_run
        iso.write_cert_probe_theory = orig_write
        sess.close()
        server.stop()

    # (a) The SERVER physically wrote the checker-owned probe theory (captured at
    # write time; H3 then deleted it so it leaves no residue on disk).
    assert captured_probe["path"] == str(master / "Tablet_Foo__Cert.thy")
    assert not (master / "Tablet_Foo__Cert.thy").exists()  # H3: cleaned up
    probe_text = captured_probe["text"]
    assert "imports Tablet_Foo" in probe_text
    assert "thm_oracles Tablet_Foo.Foo" in probe_text
    assert "Thm.extra_shyps" in probe_text and "Thm.prop_of" in probe_text
    # (b) The cert is clean + accept-eligible, sourced from the probe.
    assert cert["status"] == iso.STATUS_OK
    assert cert["oracles_used"] == []
    assert cert["extra_shyps"] == []
    assert cert["theorem_exists"] is True
    assert cert["statement_hash"]  # a real statement hash from TRELLIS_STMT
    assert cert["kernel_axioms"] == ["One_nat_def", "add.commute"]


# =============================== 6. version-pin (S7) ===============================


def test_start_rejects_old_isabelle_version(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    """S7: ``start()`` runs ``isabelle version`` first and HARD-PINS ≥ 2025-2
    (2025-1 accepted ``sorry`` — a real soundness bug). Monkeypatch the
    version banner to ``Isabelle2025-1`` ⇒ ``start()`` raises
    ``IsabelleSessionError("spawn_failed", …)`` BEFORE any server spawn."""
    import subprocess

    monkeypatch.setenv("ISABELLE_HOME_USER", str(tmp_path))
    sess = IsabelleSession(name="ver-pin-test")

    spawned = {"n": 0}

    def _no_spawn() -> None:
        spawned["n"] += 1

    sess._spawn_server = _no_spawn  # type: ignore[assignment]

    def _fake_run(cmd, *a, **k):
        # The version probe is ``[bin, "version"]``.
        if isinstance(cmd, (list, tuple)) and len(cmd) >= 2 and cmd[1] == "version":
            return subprocess.CompletedProcess(cmd, 0, "Isabelle2025-1\n", "")
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(isa.subprocess, "run", _fake_run)

    with pytest.raises(IsabelleSessionError) as exc:
        sess.start()
    assert exc.value.kind == "spawn_failed"
    assert "2025" in exc.value.message
    assert spawned["n"] == 0  # never reached the spawn


def test_version_pin_accepts_2025_2(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    """The current install (``Isabelle2025-2``) passes the pin: the assertion
    is silent and the spawn proceeds (here neutered so no real server)."""
    import subprocess

    monkeypatch.setenv("ISABELLE_HOME_USER", str(tmp_path))
    sess = IsabelleSession(name="ver-ok-test")

    def _fake_run(cmd, *a, **k):
        if isinstance(cmd, (list, tuple)) and len(cmd) >= 2 and cmd[1] == "version":
            return subprocess.CompletedProcess(cmd, 0, "Isabelle2025-2\n", "")
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(isa.subprocess, "run", _fake_run)
    # Should not raise.
    sess._assert_version_pin()


def test_parse_isabelle_version_forms() -> None:
    assert isa.parse_isabelle_version("Isabelle2025-2") == (2025, 2)
    assert isa.parse_isabelle_version("Isabelle2025-1") == (2025, 1)
    assert isa.parse_isabelle_version("Isabelle2025") == (2025, 0)
    assert isa.parse_isabelle_version("Isabelle2024") == (2024, 0)
    assert isa.parse_isabelle_version("garbage") is None
    # Ordering against the floor.
    assert isa.parse_isabelle_version("Isabelle2025-1") < isa.MIN_ISABELLE_VERSION
    assert isa.parse_isabelle_version("Isabelle2025-2") >= isa.MIN_ISABELLE_VERSION
    assert isa.parse_isabelle_version("Isabelle2026") >= isa.MIN_ISABELLE_VERSION


# =============================== statement normalization ===============================


def test_statement_normalization_symbol_unicode_equiv() -> None:
    """The statement hash is spelling-stable: the Unicode glyph and the
    ``\\<name>`` symbol spelling of the SAME connective normalize to one
    canonical form ⇒ identical hash (R1; Correspondence I1 reuses this)."""
    uni = "∀x. P x ⟹ Q x"          # ∀x. P x ⟹ Q x
    sym = r"\<forall>x. P x \<Longrightarrow> Q x"
    assert isa.normalize_statement_repr(uni) == isa.normalize_statement_repr(sym)
    assert isa.statement_hash_of(uni) == isa.statement_hash_of(sym)
    assert isa.statement_hash_of(uni)  # non-empty


def test_statement_normalization_whitespace_and_cartouche() -> None:
    a = "Foo.Foo   \\<equiv>\tTrueprop   False"
    b = "Foo.Foo \\<equiv> Trueprop False"
    assert isa.normalize_statement_repr(a) == isa.normalize_statement_repr(b)
    # Cartouche delimiters are dropped.
    assert "‹" not in isa.normalize_statement_repr("‹Foo›")
    # Empty repr ⇒ empty hash (fail-closed).
    assert isa.statement_hash_of("") == ""
    assert isa.statement_hash_of("   ") == ""

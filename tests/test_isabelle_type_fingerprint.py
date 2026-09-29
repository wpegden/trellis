"""The TYPE-SENSITIVE statement fingerprint (``statement_type_hash``).

Everything here runs against synthetic probe payloads or the in-process
``_FakeIsabelleServer``. **No Isabelle is run.**

Why this axis exists. Every statement hash the cert carried before this
module digested a PRETTY-PRINT, and Isabelle's printer prunes type
information by design — the implementation manual says so in as many words
("the default configuration routinely looses information",
``Doc/Implementation/Syntax.thy:65``), the *uncheck* phase's documented job is
to "prune type-information before pretty printing" (``:198``), and constants
are never type-annotated in plain text at all because
``show_const_types = show_markup andalso show_consts_markup``
(``syntax_phases.ML:665``) and the probe strips markup.

The live consequence: a worker generalized ``fixes R :: "nat ⇒ nat ⇒ bool"``
to ``"'a ⇒ 'a ⇒ bool"``, the printed term came back byte-identical, the
fingerprint did not move, and a correspondence ``Fail`` became unclearable.
The same blindness runs the other way — NARROWING a protected statement would
equally not move it, so the approval freeze could silently cover a weaker
claim. ``test_live_defect_*`` below is that exact case.
"""

from __future__ import annotations

from pathlib import Path
from typing import List, Optional

from trellis.atomic_actions import isabelle_observations as iso
from trellis.checker.isabelle_session import (
    CheckOutcome,
    TRELLIS_MARKER_EOM,
    _parse_cert_from_writeln,
    normalize_statement_repr,
    statement_hash_of,
    write_cert_probe_theory,
)
from trellis.checker.isabelle_warm_gate import _CERT_FIELDS

from tests.test_isabelle_b2c_cert import _run_thm_deps, _writeln
from tests.test_isabelle_session import _node, fake_db  # noqa: F401  (fixture)


# --------------------------- synthetic probe payloads ---------------------------
#
# `ML_Syntax.print_term` output, verbatim in shape: `Term.Const`/`Term.Free`/
# `Term.Var` carry `(name, typ)`, `Term.Type`/`Term.TFree`/`Term.TVar` carry
# their argument lists and SORTS, `Term.$` is application (`ml_syntax.ML:124`).
# The struct payload is `<shyps>|<hyps>|<prop>`.

_NAT = 'Term.Type ("Nat.nat", [])'
_BOOL = 'Term.Type ("HOL.bool", [])'
_PROP = 'Term.Type ("prop", [])'


def _fun(dom: str, rng: str) -> str:
    return f'Term.Type ("fun", [{dom}, {rng}])'


def _tfree(name: str, sort: str = "HOL.type") -> str:
    return f'Term.TFree ("{name}", ["{sort}"])'


def _trueprop(body: str) -> str:
    return f'Term.$ (Term.Const ("HOL.Trueprop", {_fun(_BOOL, _PROP)}), {body})'


def _relation_prop(elem_typ: str) -> str:
    """The live case's ``?R ?x ?y`` with ``?R :: elem ⇒ elem ⇒ bool``."""
    rel = _fun(elem_typ, _fun(elem_typ, _BOOL))
    return _trueprop(
        f'Term.$ (Term.$ (Term.Var (("R", 0), {rel}), '
        f'Term.Var (("x", 0), {elem_typ})), '
        f'Term.Var (("y", 0), {elem_typ}))'
    )


def _struct(prop: str, *, hyps: str = "[]", shyps: str = "[]") -> str:
    return f"{shyps}|{hyps}|{prop}"


# The PRINTED statement for both live-case variants. Byte-identical, because
# `uncheck` prunes exactly the type that differs. This string is the whole
# problem: it is what `statement_hash` and `lean_semantic_closure` digest.
_PRINTED = r"Trueprop (?R ?x ?y)"


def _probe_message_bodies(
    *,
    struct: Optional[str],
    printed: str = _PRINTED,
    typed: str = "",
    shyps_line: str = "",
) -> List[str]:
    """A full checker-probe writeln message list, sentinel-terminated.

    One element per ``writeln`` MESSAGE — which is what the probe emits and
    what `_parse_cert_from_writeln` consumes.
    """
    eom = TRELLIS_MARKER_EOM
    lines = [
        "theorem Tablet_Foo.Foo: P",
        "oracles:",
        "dependencies: 1\n    refl",
        f"TRELLIS_SHYPS {shyps_line} {eom}",
        f"TRELLIS_STMT {printed} {eom}",
        f"TRELLIS_STMT_LONG {printed} {eom}",
    ]
    if struct is not None:
        lines.append(f"TRELLIS_STMT_STRUCT {struct} {eom}")
    if typed:
        lines.append(f"TRELLIS_STMT_TYPED {typed} {eom}")
    return lines


def _type_hash_of(struct: str) -> str:
    """The digest the checker computes for a ``TRELLIS_STMT_STRUCT`` payload."""
    (
        _oracles,
        _deps,
        _exists,
        _shyps,
        _stmt,
        _long,
        _res,
        _bnd,
        parsed_struct,
        _typed,
    ) = _parse_cert_from_writeln(_probe_message_bodies(struct=struct), "Foo")
    assert parsed_struct, "the probe payload must round-trip through the parser"
    return statement_hash_of(parsed_struct)


def _print_hash_of(printed: str) -> str:
    return statement_hash_of(printed)


# ================================ 1. type sensitivity ================================


def test_live_defect_type_generalization_moves_the_type_hash() -> None:
    """THE live case, end to end at the digest level.

    ``fixes R :: "nat ⇒ nat ⇒ bool"`` generalized to ``"'a ⇒ 'a ⇒ bool"``.
    The printed term is byte-identical — ``uncheck`` deleted precisely the
    ``typ`` inside the ``Var`` — so every print-derived hash is unmoved. The
    structural digest moves, because in ``Term.term`` every leaf carries its
    full ``typ`` (``term.ML:217-234``) and nothing prunes it.
    """
    concrete = _struct(_relation_prop(_NAT))
    generic = _struct(_relation_prop(_tfree("'a")))

    # The premise of the whole defect: the PRINT does not distinguish them.
    assert _print_hash_of(_PRINTED) == _print_hash_of(_PRINTED)

    # The fix: the TYPE axis does.
    assert _type_hash_of(concrete) != _type_hash_of(generic)


def test_type_narrowing_moves_the_type_hash_too() -> None:
    """The direction that matters for the approval freeze.

    A generalization is loud (a reviewer notices an unclearable `Fail`); a
    NARROWING of an already-approved statement is silent and strictly weakens
    the claim the freeze is protecting. Same print, so the same blindness —
    and the same fix, symmetrically.
    """
    generic = _struct(_relation_prop(_tfree("'a")))
    narrowed = _struct(_relation_prop(_NAT))
    assert _type_hash_of(generic) != _type_hash_of(narrowed)


def test_type_argument_only_under_a_constant_moves_the_type_hash() -> None:
    """``card {} = 0`` at ``'a set`` vs at ``nat set``.

    The type variable occurs only under a ``Const``, so even a
    ``show_types``/``show_sorts`` print cannot distinguish these
    (``show_const_types`` requires ``show_markup``, which the probe strips).
    The structural digest is not printing, so it sees it.
    """

    def card_prop(elem: str) -> str:
        set_t = f'Term.Type ("Set.set", [{elem}])'
        return _trueprop(
            f'Term.$ (Term.$ (Term.Const ("HOL.eq", '
            f"{_fun(_NAT, _fun(_NAT, _BOOL))}), "
            f'Term.$ (Term.Const ("Finite_Set.card", {_fun(set_t, _NAT)}), '
            f'Term.Const ("Orderings.bot_class.bot", {set_t}))), '
            f'Term.Const ("Groups.zero_class.zero", {_NAT}))'
        )

    assert _type_hash_of(_struct(card_prop(_tfree("'a")))) != _type_hash_of(
        _struct(card_prop(_NAT))
    )


# ================================ 2. sort sensitivity ================================


def test_sort_constraint_moves_the_type_hash() -> None:
    """``'a`` vs ``'a::linorder``.

    Sorts live on ``TFree``/``TVar`` in the datatype, so the structural digest
    carries them. ``show_sorts`` would show this one, but only where the tyvar
    survives ``prune_types`` — and never under a ``Const``.
    """
    plain = _struct(_relation_prop(_tfree("'a")))
    ordered = _struct(_relation_prop(_tfree("'a", "Orderings.linorder")))
    assert _type_hash_of(plain) != _type_hash_of(ordered)


# ================================ 3. stability ================================


def test_whitespace_in_the_struct_payload_is_not_meaning() -> None:
    """Whitespace runs collapse: the type axis reuses the ONE normalizer.

    This is what makes the wrap fix (section 6) safe — a payload reassembled
    from wrapped physical lines carries whatever spacing the break inserted,
    and must still hash to the unwrapped value.
    """
    payload = _struct(_relation_prop(_NAT))
    for spacing in (",  ", ",   ", ",\t", ",\n  "):
        respaced = payload.replace(", ", spacing)
        assert respaced != payload
        assert statement_hash_of(respaced) == statement_hash_of(payload)
    # Leading/trailing whitespace is stripped, not digested.
    assert statement_hash_of(f"  {payload}  ") == statement_hash_of(payload)


def test_bound_variable_names_are_load_bearing_which_is_why_ml_erases_them() -> None:
    """Binder names are printing hints (bound vars are de Bruijn,
    ``term.ML:233``), so they must not move the digest — but
    ``ML_Syntax.print_term`` DOES print them (``ml_syntax.ML:128``). That is
    exactly why the probe's canonicalizer applies ``Term.map_abs_vars (K "")``
    before printing: without it, ``∀x.`` → ``∀y.`` would be a false reopen.

    This test pins both halves: the raw prints differ, the canonical ones do
    not. The canonicalization itself runs inside the probe's ML — see
    ``test_probe_ml_pins_the_canonicalization_stack``, which is what makes the
    claim structural rather than aspirational.
    """
    body = "Term.Bound 0"
    abs_x = f'Term.Abs ("x", {_NAT}, {body})'
    abs_y = f'Term.Abs ("y", {_NAT}, {body})'
    abs_canon = f'Term.Abs ("", {_NAT}, {body})'

    assert _type_hash_of(_struct(abs_x)) != _type_hash_of(_struct(abs_y))
    assert _type_hash_of(_struct(abs_canon)) == _type_hash_of(_struct(abs_canon))


def test_schematic_index_churn_is_load_bearing_which_is_why_ml_zeroes_it() -> None:
    """``?a1`` vs ``?a2``: same statement, and the probe's
    ``Term_Subst.zero_var_indexes_list`` makes both print at index 0."""
    at_1 = f'Term.Var (("R", 1), {_BOOL})'
    at_2 = f'Term.Var (("R", 2), {_BOOL})'
    zeroed = f'Term.Var (("R", 0), {_BOOL})'
    assert _type_hash_of(_struct(at_1)) != _type_hash_of(_struct(at_2))
    assert _type_hash_of(_struct(zeroed)) == _type_hash_of(_struct(zeroed))


def test_probe_ml_pins_the_canonicalization_stack(tmp_path: Path) -> None:
    """The stability guarantees above live in the probe's ML, which no test
    here can execute. Pin the emitted text so they cannot be silently dropped.

    Each line is load-bearing:
      * ``Thm.full_prop_of`` (not ``Thm.prop_of``) — covers flex-flex pairs,
        component 2 of ``Thm.thm_ord``;
      * ``Thm.hyps_of`` / ``Thm.extra_shyps`` — components 3 and 4; a fact
        carrying local hypotheses is a strictly WEAKER theorem;
      * ``Envir.beta_eta_contract`` — eta-noise is not meaning;
      * ``Term_Subst.zero_var_indexes_list`` — the LIST form, so hyps and prop
        are renumbered JOINTLY. With the singleton form a hypothesis's
        schematic variable would be zeroed independently of the prop's, and
        two different (hyps, prop) pairings could collide onto one digest;
      * ``Term.map_abs_vars (K "")`` — makes the digest decide ``aconv``
        exactly rather than approximate it.
    """
    path = write_cert_probe_theory(tmp_path, "Tablet_Foo", "Tablet_Foo.Foo")
    text = path.read_text(encoding="utf-8")
    for fragment in (
        "Thm.full_prop_of thm",
        "Thm.hyps_of thm",
        "Thm.extra_shyps thm",
        "Envir.beta_eta_contract",
        "Term_Subst.zero_var_indexes_list",
        'Term.map_abs_vars (K "")',
        "ML_Syntax.print_term",
        "ML_Syntax.print_sort",
        "TRELLIS_STMT_STRUCT",
    ):
        assert fragment in text, f"probe lost {fragment!r}:\n{text}"
    # `Thm.prop_of` alone would drop the tpairs component.
    assert "Thm.full_prop_of" in text


# ============================ 4. hyps / shyps ============================


def test_local_hypothesis_moves_the_type_hash() -> None:
    """A fact carrying ``hyps`` is strictly weaker than the same fact without
    them, and no hash before this one could see the difference:
    ``Thm.prop_of`` does not mention them at all."""
    prop = _relation_prop(_NAT)
    clean = _struct(prop)
    with_hyp = _struct(prop, hyps=f"[{_trueprop(f'Term.Free (\"h\", {_BOOL})')}]")
    assert _type_hash_of(clean) != _type_hash_of(with_hyp)


def test_dangling_sort_hypothesis_moves_the_type_hash() -> None:
    """``extra_shyps`` is the empty/inconsistent-type-class vacuous-``False``
    channel. It has its own hard gate, but it is also part of theorem identity
    (``Thm.thm_ord`` component 4) and so belongs in the digest."""
    prop = _relation_prop(_NAT)
    clean = _struct(prop)
    dangling = _struct(prop, shyps='[["Foo.empty"]]')
    assert _type_hash_of(clean) != _type_hash_of(dangling)


# ============================ 5. alias de-leak ============================


class _AliasStubSession:
    """A session that elaborates the node under a content-keyed WARM alias.

    Reproduces the H1 warm path: ``fresh_inflight_theory`` returns
    ``<node>__In_<sha>`` and every cert payload comes back qualified with THAT
    name, because ``Term.Const`` names in ``Term.term`` are always the internal
    LONG names (``term.ML:230``).
    """

    warm_prefix_enabled = False  # keep the probe theory name deterministic

    def __init__(self, alias: str, outcome: CheckOutcome) -> None:
        self.alias = alias
        self.outcome = outcome
        self.calls: List[str] = []

    def fresh_inflight_theory(self, *, master_dir: str, theory: str) -> str:
        return self.alias

    def check_theory(self, *, master_dir, theory, cert_theorem, timeout_secs):
        self.calls.append(theory)
        if theory.endswith("__Cert"):
            return self.outcome
        return CheckOutcome(
            ok=True, failed=0, finished=1, theory_name=theory, node_name=theory
        )


def test_alias_qualifier_is_rewritten_out_of_the_type_payload(tmp_path: Path) -> None:
    """Without this, EVERY warm probe's type digest differs from the cold one.

    The short ``statement_repr`` escapes the alias because the printer externs
    names; the structural payload cannot, since ``Term.Const`` carries the
    internal long name unconditionally. So the alias would leak into the digest
    on every warm probe, the warm-vs-cold cross-check would compare a warm
    ``Tablet_Foo__In_<sha>.cg`` against a cold ``Tablet_Foo.cg``, and the run
    would HALT on a heap-corruption alarm that is really a naming artefact.
    """
    node = "Tablet_Foo"
    alias = f"{node}__In_deadbeefcafe"
    aliased_const = f'Term.Const ("{alias}.cg", {_BOOL})'
    aliased_struct = _struct(_trueprop(aliased_const))
    clean_struct = _struct(_trueprop(f'Term.Const ("{node}.cg", {_BOOL})'))

    outcome = CheckOutcome(
        ok=True,
        failed=0,
        finished=1,
        theory_name=f"{alias}__Cert",
        node_name=f"{alias}__Cert",
        theorem_exists=True,
        statement_repr="cg n",
        statement_hash=statement_hash_of("cg n"),
        statement_repr_long=f"{alias}.cg n",
        statement_type_repr=normalize_statement_repr(aliased_struct),
        statement_type_hash=statement_hash_of(aliased_struct),
        statement_repr_typed=f"{alias}.cg (n::nat)",
    )
    session = _AliasStubSession(alias, outcome)

    probe = iso.run_cert_probe(
        session,
        master_dir=str(tmp_path),
        theory=node,
        cert_theorem="Foo",
        timeout_secs=10.0,
    )

    assert "__In_" not in probe.statement_type_repr, probe.statement_type_repr
    assert "__In_" not in probe.statement_repr_long
    assert "__In_" not in probe.statement_repr_typed
    # The de-leaked digest must equal what a COLD build of the same node
    # produces — that equality is the whole point.
    assert probe.statement_type_hash == statement_hash_of(clean_struct)


def test_warm_cold_cross_check_compares_the_type_hash() -> None:
    """The cross-check's field allowlist must carry the new axis, or a warm
    heap whose theorem differs from cold ONLY in a type would pass every
    compared field (they all digest prints).

    ``statement_repr_typed`` is deliberately absent: it is a diagnostic print,
    and a print has no business being able to HALT the run.
    """
    assert "statement_type_hash" in _CERT_FIELDS
    assert "statement_repr_typed" not in _CERT_FIELDS


# ============================ 6. wrap / truncation ============================


def _wrap_at(text: str, margin: int = 76) -> str:
    """Break ``text`` into physical lines at ``margin``, as batch-mode
    ``Pretty`` does (``pretty.ML:264,418-481``; default margin 76 from
    ``ml_pretty.ML:125``)."""
    out: List[str] = []
    line = ""
    for token in text.split(" "):
        if line and len(line) + 1 + len(token) > margin:
            out.append(line)
            line = token
        else:
            line = f"{line} {token}" if line else token
    if line:
        out.append(line)
    return "\n".join(out)


def test_wrapped_statement_payload_round_trips_intact() -> None:
    """A ``TRELLIS_STMT`` payload that batch-mode ``Pretty`` wrapped at the
    76-column margin must be reassembled, not truncated at its first line.

    The pre-fix parser matched the tag as a LINE prefix, took the rest of THAT
    line, and dropped every continuation line — so the fingerprint would have
    covered a PREFIX of the statement, silently. It never bit only because a
    PIDE session's ``symbolic_output_ops`` renders breaks as spaces rather
    than newlines (``pretty.ML:271,505``) — a print-mode side condition, not a
    property of the parser.
    """
    long_prop = (
        "Trueprop (bigrelation ?alpha ?beta ?gamma ?delta ?epsilon ?zeta "
        "?eta ?theta ?iota ?kappa ?lambda ?mu ?nu ?xi ?omicron ?pi ?rho "
        "?sigma ?tau ?upsilon ?phi ?chi ?psi ?omega)"
    )
    wrapped = _wrap_at(long_prop)
    assert "\n" in wrapped, "the fixture must actually wrap"

    # ONE writeln message whose body spans several physical lines.
    messages = [f"TRELLIS_STMT {wrapped} {TRELLIS_MARKER_EOM}"]
    (
        _oracles,
        _deps,
        _exists,
        _shyps,
        stmt,
        _long,
        _res,
        _bnd,
        _struct_payload,
        _typed,
    ) = _parse_cert_from_writeln(messages, None)

    assert normalize_statement_repr(stmt) == normalize_statement_repr(long_prop)
    assert statement_hash_of(stmt) == statement_hash_of(long_prop)


def test_wrapped_struct_payload_round_trips_intact() -> None:
    """Same, for the type axis. ``ML_Syntax.print_term`` repeats the full type
    at every leaf, so a real statement's structural payload is kilobytes — the
    payload most likely to wrap, and the one whose truncation would be a
    silently weaker gate."""
    struct = _struct(_relation_prop(_NAT))
    wrapped = _wrap_at(struct)
    assert "\n" in wrapped

    messages = [f"TRELLIS_STMT_STRUCT {wrapped} {TRELLIS_MARKER_EOM}"]
    parsed = _parse_cert_from_writeln(messages, None)[8]
    assert statement_hash_of(parsed) == statement_hash_of(struct)


def test_sentinel_terminates_a_payload_inside_a_merged_message() -> None:
    """Belt and braces: even if a transport merged markers into one message,
    the explicit sentinel stops one marker swallowing the next."""
    eom = TRELLIS_MARKER_EOM
    merged = "\n".join(
        [
            f"TRELLIS_STMT alpha beta {eom}",
            f"TRELLIS_STMT_LONG Tablet_Foo.alpha beta {eom}",
        ]
    )
    result = _parse_cert_from_writeln([merged], None)
    assert result[4] == "alpha beta"
    assert result[5] == "Tablet_Foo.alpha beta"


def test_marker_payload_stops_at_a_cert_section_header() -> None:
    """A ``thm_oracles``/``thm_deps`` structural line never continues a marker
    payload, even inside the same message and with no sentinel."""
    merged = "\n".join(["TRELLIS_STMT alpha", "oracles:", "    skip_proof"])
    result = _parse_cert_from_writeln([merged], None)
    assert result[4] == "alpha"
    assert result[0] == ["skip_proof"]


def test_longest_marker_tag_wins() -> None:
    """``TRELLIS_STMT`` textually prefixes three siblings. Tag matching is
    ordered by length, not by declaration order, so adding the next marker
    cannot silently reintroduce the shadowing hazard."""
    eom = TRELLIS_MARKER_EOM
    messages = [
        f"TRELLIS_STMT short {eom}",
        f"TRELLIS_STMT_LONG long {eom}",
        f"TRELLIS_STMT_STRUCT struct {eom}",
        f"TRELLIS_STMT_TYPED typed {eom}",
    ]
    result = _parse_cert_from_writeln(messages, None)
    assert result[4] == "short"
    assert result[5] == "long"
    assert result[8] == "struct"
    assert result[9] == "typed"


def test_pre_sentinel_single_line_payloads_still_parse() -> None:
    """Recorded traces and pre-sentinel probe output must keep parsing exactly
    as before: the message boundary alone is a sufficient delimiter."""
    messages = [
        "TRELLIS_SHYPS ",
        "TRELLIS_STMT Foo.Foo \\<equiv> Trueprop False",
        "TRELLIS_STMT_LONG Tablet_Foo.Foo \\<equiv> Trueprop False",
    ]
    result = _parse_cert_from_writeln(messages, None)
    assert result[3] == []
    assert result[4] == "Foo.Foo \\<equiv> Trueprop False"
    assert result[5] == "Tablet_Foo.Foo \\<equiv> Trueprop False"


# ============================ 7. cert envelope ============================


def test_cert_envelope_carries_the_type_axis(fake_db) -> None:  # noqa: F811
    """End-to-end through the real ``thm_deps_server_side`` against the fake
    wire: the axis reaches the ``LocalClosureProbeOutput`` envelope the kernel
    deserializes."""
    struct = _struct(_relation_prop(_NAT))
    worker = _node(theory_name="Draft.Tablet_Foo", failed=0, ok=True)
    probe = _node(
        theory_name="Draft.Tablet_Foo__Cert",
        failed=0,
        ok=True,
        messages=_writeln(
            *_probe_message_bodies(
                struct=struct, typed="?R (?x::nat) (?y::nat)"
            )
        ),
    )
    cert = _run_thm_deps(fake_db, name="typeaxis", worker_node=worker, probe_node=probe)

    assert cert["status"] == iso.STATUS_OK
    assert cert["statement_type_hash"] == statement_hash_of(struct)
    assert cert["statement_repr_typed"] == "?R (?x::nat) (?y::nat)"
    # Additive: the pre-existing axes are untouched.
    assert cert["statement_hash"] == statement_hash_of(_PRINTED)
    assert cert["statement_repr_long"] == normalize_statement_repr(_PRINTED)


def test_cert_envelope_two_variants_agree_on_print_and_differ_on_type(
    fake_db,  # noqa: F811
) -> None:
    """The live incident, asserted where the kernel actually reads it."""
    worker = _node(theory_name="Draft.Tablet_Foo", failed=0, ok=True)

    def cert_for(elem_typ: str, name: str):
        probe = _node(
            theory_name="Draft.Tablet_Foo__Cert",
            failed=0,
            ok=True,
            messages=_writeln(
                *_probe_message_bodies(struct=_struct(_relation_prop(elem_typ)))
            ),
        )
        return _run_thm_deps(
            fake_db, name=name, worker_node=worker, probe_node=probe
        )

    concrete = cert_for(_NAT, "concrete")
    generic = cert_for(_tfree("'a"), "generic")

    assert concrete["statement_hash"] == generic["statement_hash"]
    assert concrete["statement_repr_long"] == generic["statement_repr_long"]
    assert concrete["statement_type_hash"] != generic["statement_type_hash"]


def test_missing_struct_marker_yields_an_empty_type_hash(fake_db) -> None:  # noqa: F811
    """Fail-closed: no marker ⇒ no digest (never a digest of nothing). The
    kernel's ``reshape_isabelle_corr_payload`` then refuses the corr payload
    outright rather than producing a fingerprint blind to types."""
    worker = _node(theory_name="Draft.Tablet_Foo", failed=0, ok=True)
    probe = _node(
        theory_name="Draft.Tablet_Foo__Cert",
        failed=0,
        ok=True,
        messages=_writeln(*_probe_message_bodies(struct=None)),
    )
    cert = _run_thm_deps(fake_db, name="nostruct", worker_node=worker, probe_node=probe)
    assert cert["statement_type_hash"] == ""


def test_internal_error_cert_fails_closed_on_the_type_axis() -> None:
    """A transport failure must not present an empty digest as a stable one."""
    cert = iso._internal_error_cert("transport blew up")
    assert cert["statement_type_hash"] == ""
    assert cert["statement_repr_typed"] == ""

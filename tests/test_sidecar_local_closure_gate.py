"""The sidecar's LOCAL-closure gate, and the axiom parser behind it.

Both are pure string functions, so they are tested here rather than in
`test_sidecar_compile_loop.py`, which is toolchain-gated and skips in CI —
which is why neither had coverage.
"""

from __future__ import annotations

import pytest

from trellis.sidecar.compile_loop import _sorry_warning_names_node
from trellis.sidecar.prevalidate import run_axioms_probe  # noqa: F401  (import guard)


# The observed shape, from a real grunt attempt: `lake build Tablet.<Node>`
# replays the node's transitive import closure and re-emits every open
# dependency's warning. The target node itself is clean.
REAL_OUTPUT = """\
⚠ [1223/1236] Replayed Tablet.MinimallyImperfectDegreeTwoOddHole
warning: Tablet/MinimallyImperfectDegreeTwoOddHole.lean:10:8: declaration uses `sorry`
⚠ [1224/1236] Replayed Tablet.MinimallyImperfectTwoJoinOddHole
warning: Tablet/MinimallyImperfectTwoJoinOddHole.lean:13:8: declaration uses `sorry`
warning: Tablet/TwoJoinBlocksPerfect.lean:16:8: declaration uses `sorry`
Build completed successfully (1236 jobs).
"""


def test_dependency_warnings_do_not_condemn_the_node():
    """The regression this gate exists for: a clean proof was discarded
    because unrelated nodes in its import closure are open."""
    assert _sorry_warning_names_node(REAL_OUTPUT, "MinimumImperfectBergeNoTwoJoin") is False


def test_the_nodes_own_warning_still_condemns_it():
    own = REAL_OUTPUT + (
        "warning: Tablet/MinimumImperfectBergeNoTwoJoin.lean:14:2: declaration uses `sorry`\n"
    )
    assert _sorry_warning_names_node(own, "MinimumImperfectBergeNoTwoJoin") is True


@pytest.mark.parametrize("quote", ["`sorry`", "'sorry'", '"sorry"', "sorry"])
def test_every_quote_style_lean_has_used(quote):
    line = f"warning: Tablet/Foo.lean:1:1: declaration uses {quote}"
    assert _sorry_warning_names_node(line, "Foo") is True


def test_an_unattributed_warning_fails_closed():
    """A warning naming no file at all is treated as the node's own."""
    assert _sorry_warning_names_node("warning: declaration uses sorry", "Foo") is True


@pytest.mark.parametrize(
    "target,other",
    [("Foo", "FooBar"), ("Bar", "FooBar"), ("FooBar", "Foo")],
)
def test_node_names_that_are_substrings_do_not_collide(target, other):
    line = f"warning: Tablet/{other}.lean:1:1: declaration uses `sorry`"
    assert _sorry_warning_names_node(line, target) is False


def test_absolute_paths_still_match_the_node():
    line = (
        "warning: /home/x/runtime/sidecar/grunts/1/repo/Tablet/Foo.lean:1:1: "
        "declaration uses `sorry`"
    )
    assert _sorry_warning_names_node(line, "Foo") is True


def test_a_mathlib_warning_is_not_the_node():
    line = "warning: Mathlib/Order/Foo.lean:1:1: declaration uses `sorry`"
    assert _sorry_warning_names_node(line, "Foo") is False


def test_a_clean_build_is_clean():
    assert _sorry_warning_names_node("Build completed successfully (1236 jobs).", "Foo") is False


# --- the axiom parser -------------------------------------------------
#
# `run_axioms_probe` shells out, so the parse is exercised through a stub
# of its subprocess call.


def _probe(monkeypatch, tmp_path, stdout: str, rc: int = 0):
    from types import SimpleNamespace
    from pathlib import Path
    from trellis.sidecar import prevalidate as pv
    from trellis.sidecar.config import SidecarConfig

    monkeypatch.setattr(
        pv.subprocess, "run",
        lambda *a, **k: SimpleNamespace(returncode=rc, stdout=stdout, stderr=""),
    )
    return pv.run_axioms_probe(Path(tmp_path), "Foo", SidecarConfig(sandbox_role=""))


def test_wrapped_axiom_list_is_not_truncated(monkeypatch, tmp_path):
    """Lean wraps long axiom lists. Line-scoped parsing silently dropped
    everything after the wrap — including, in the worst case, `sorryAx`."""
    axioms, _ = _probe(monkeypatch, tmp_path, "'Foo' depends on axioms: [propext,\n Classical.choice, Quot.sound, sorryAx]")
    assert axioms is not None
    assert "sorryAx" in axioms
    assert len(axioms) == 4


def test_unrecognized_output_fails_closed(monkeypatch, tmp_path):
    """Neither marker present must mean UNKNOWN, not 'no axioms'."""
    axioms, _ = _probe(monkeypatch, tmp_path, "lake: something went sideways")
    assert axioms is None


def test_no_axioms_is_an_empty_set_not_a_failure(monkeypatch, tmp_path):
    axioms, _ = _probe(monkeypatch, tmp_path, "'Foo' does not depend on any axioms")
    assert axioms == []


def test_ordinary_axiom_list(monkeypatch, tmp_path):
    axioms, _ = _probe(monkeypatch, tmp_path, "'Foo' depends on axioms: [propext, Classical.choice, Quot.sound]")
    assert axioms == ["propext", "Classical.choice", "Quot.sound"]


def test_nonzero_return_code_fails_closed(monkeypatch, tmp_path):
    axioms, _ = _probe(monkeypatch, tmp_path, "boom", rc=1)
    assert axioms is None


# prevalidate is ADVISORY — the kernel is the sole authority — so a daemon
# check STRICTER than the kernel's discards good work before the kernel can
# rule on it. The axiom check probed `#print axioms` (transitive), so every
# node importing an open node reported `sorryAx` and died here; the kernel's
# gate 8 uses the LOCAL closure, where an open dep's sorryAx is absent.
# `perfect` node linegraph2_5, 2026-07-31: clean body, 4 open imports,
# discarded after 283s / 1.37M tokens.


def test_open_dependency_sorry_ax_does_not_reject(monkeypatch, tmp_path):
    from pathlib import Path
    from trellis.sidecar import prevalidate as pv
    from trellis.sidecar.config import SidecarConfig

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True, exist_ok=True)
    (repo / "Tablet" / "Foo.lean").write_text("theorem Foo : True := by\n-- BODY\n  trivial\n")
    monkeypatch.setattr(pv, "only_body_audit", lambda *a, **k: [])
    monkeypatch.setattr(pv, "run_local_closure_probe", lambda *a, **k:
                        {"status": "ok", "kernel_axioms": ["propext"]})
    monkeypatch.setattr(pv, "run_axioms_probe", lambda *a, **k:
                        (["propext", "sorryAx"], "log"))
    monkeypatch.setattr(pv, "load_approved_axioms_list", lambda *a, **k: ["propext"])
    monkeypatch.setattr(pv, "tablet_source_closure_hash", lambda *a, **k: "h")
    res = pv.prevalidate_success(
        repo=Path(repo), node="Foo", config=SidecarConfig(sandbox_role=""),
        pre_image="theorem Foo : True := by\n-- BODY\n  sorry\n", proof_body="  trivial",
    )
    assert res.ok, f"rejected a closure the kernel would accept: {res.reasons}"

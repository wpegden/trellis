

def test_isabelle_fragment_variant_is_resolved_structurally() -> None:
    """A fragment resolves to its `_isabelle` twin on an isabelle_hol run, with
    NO per-call-site conditional.

    REGRESSION: every `_isabelle` variant used to be routed by a hand-written
    branch at its own call site, so adding a fragment and forgetting the branch
    silently shipped Lean text to an Isabelle run — no error, because the Lean
    fragment exists and renders fine. That is how an Isabelle worker was told
    "Trellis formalizes ... into Lean", shown `Tablet/<Node>.lean`,
    `def`/`abbrev`/`structure`, the `-- BODY` marker and Mathlib hygiene, and
    reported back that "the target's Lean proof remains explicitly open".

    With resolution centralized, adding the variant FILE is sufficient.
    """
    from pathlib import Path

    from trellis.runtime.bridge_prompts import _resolve_prompt_fragment

    twinned = "canonical/SOUNDNESS.md"
    assert _resolve_prompt_fragment(twinned, backend="lean").name == "SOUNDNESS.md"
    assert (
        _resolve_prompt_fragment(twinned, backend="isabelle_hol").name
        == "SOUNDNESS_isabelle.md"
    )

    # The scheme document that leaked into the live run.
    scheme = "common/TRELLIS_FORMALIZATION_SCHEME.md"
    assert (
        _resolve_prompt_fragment(scheme, backend="isabelle_hol").name
        == "TRELLIS_FORMALIZATION_SCHEME_isabelle.md"
    )

    # No twin -> falls back to the shared text rather than failing. Discover an
    # untwinned fragment rather than naming one: variants are added over time, so
    # any hardcoded example eventually acquires a twin and inverts this check.
    from trellis.runtime.bridge_prompts import PROMPT_FRAGMENT_ROOT

    untwinned = next(
        (
            str(f.relative_to(PROMPT_FRAGMENT_ROOT))
            for f in sorted(PROMPT_FRAGMENT_ROOT.rglob("*.md"))
            if "_isabelle" not in f.name
            and not f.with_name(f.stem + "_isabelle" + f.suffix).exists()
        ),
        None,
    )
    if untwinned is not None:
        assert (
            _resolve_prompt_fragment(untwinned, backend="isabelle_hol").name
            == Path(untwinned).name
        )

    # The Lean path is untouched: no backend, or an explicit lean backend, both
    # resolve exactly as before.
    assert _resolve_prompt_fragment(scheme).name == "TRELLIS_FORMALIZATION_SCHEME.md"


def test_isabelle_scheme_fragments_carry_no_lean_vocabulary() -> None:
    """The Isabelle scheme documents must not mention Lean file conventions."""
    import re

    from trellis.runtime.bridge_prompts import PROMPT_FRAGMENT_ROOT

    banned = re.compile(r"\bLean\b|Mathlib|\.lean\b|\blake\b|-- BODY|olean")
    for name in (
        "TRELLIS_FORMALIZATION_SCHEME_isabelle.md",
        "TRELLIS_FORMALIZATION_SCHEME_verifier_isabelle.md",
        "00_trellis_scheme_brief_isabelle.md",
    ):
        text = (PROMPT_FRAGMENT_ROOT / "common" / name).read_text(encoding="utf-8")
        assert not banned.search(text), f"{name} still carries Lean vocabulary"


def test_filespec_resolves_to_the_canonical_name_every_fragment_cites(tmp_path) -> None:
    """Prompt prose names the spec `FILESPEC.md` on both backends.

    The run repo therefore holds one filespec, at that name, carrying the
    backend's content (`setup_repo.sh`). Shipping the Lean spec under the
    canonical name on an Isabelle run told workers to write `Tablet/<Node>.lean`
    while the substituted `filespec_path` pointed somewhere else.
    """
    import json

    from trellis.runtime.bridge_prompts import _filespec_path

    repo = tmp_path / "repo"
    repo.mkdir()
    (repo / "trellis.config.json").write_text(
        json.dumps({"workflow": {"default_target": "isabelle_hol"}}), encoding="utf-8"
    )
    (repo / "FILESPEC.md").write_text("# spec\n", encoding="utf-8")

    assert _filespec_path(repo) == (repo / "FILESPEC.md").resolve()


def test_isabelle_filespec_carries_no_lean_vocabulary() -> None:
    """The spec an Isabelle worker reads names only Isabelle mechanisms."""
    import re
    from pathlib import Path

    source = Path(__file__).resolve().parents[1] / "FILESPEC_isabelle.md"
    text = source.read_text(encoding="utf-8")
    hits = sorted(
        set(re.findall(r"\bLean\b|\bMathlib\b|\.lean\b|\blake\b|-- BODY|\bolean\b", text))
    )
    assert not hits, f"FILESPEC_isabelle.md carries Lean vocabulary: {hits}"


def test_runtime_snapshot_puts_one_backend_correct_filespec_at_the_repo_root(tmp_path) -> None:
    """The repo root carries this backend's spec, and only it.

    The snapshot mirrors the source tree (both names) into
    `.trellis/runtime/src/`. Mirroring both to the REPO ROOT as well put the
    other backend's spec where a worker browsing the project would find it.
    """
    import json

    from trellis.runtime_snapshot import materialize_project_runtime

    for target, expect_isabelle in (("isabelle_hol", True), ("lean", False)):
        repo = tmp_path / target
        (repo / ".trellis").mkdir(parents=True)
        (repo / "trellis.config.json").write_text(
            json.dumps({"workflow": {"default_target": target}}), encoding="utf-8"
        )

        materialize_project_runtime(repo, repo / ".trellis")

        roots = sorted(p.name for p in repo.glob("FILESPEC*.md"))
        assert roots == ["FILESPEC.md"], f"{target}: repo root filespecs = {roots}"

        body = (repo / "FILESPEC.md").read_text(encoding="utf-8")
        assert ("Isabelle/HOL backend" in body) is expect_isabelle, (
            f"{target}: repo root FILESPEC.md holds the wrong backend's spec"
        )


def test_cleanup_audit_tasks_render_pending_first_with_stable_task_index() -> None:
    """Rendered `cleanup_audit_tasks_json` survives prompt truncation.

    Tasks accumulate monotonically across audit rounds, so once the list
    passes the `_json_fence` truncation threshold the early slots are
    resolved history and the live Pending proposals fall past the
    truncation marker — degrading the audit's duplicate-avoidance view.
    The rendering must (a) stamp every entry's `task_index` with its
    zero-based position in the kernel's underlying array BEFORE any
    reordering (the kernel validates an inbound `task_modifications`
    index only for range and Pending status, so a positional read of a
    reordered list would dismiss the wrong task with no error), and
    (b) order Pending entries first so truncation drops resolved
    history instead.
    """
    import json

    from trellis.runtime.bridge_prompts import (
        _PROMPT_LIST_TRUNCATION_THRESHOLD,
        _json_fence,
        _render_cleanup_audit_tasks,
    )

    def task(position: int, status: dict) -> dict:
        # Mirror the kernel's `audit_contract_payload` task shape,
        # including the kernel-stamped positional `task_index`.
        return {
            "task_index": position,
            "target_node": f"Node{position:02d}",
            "rationale": "r",
            "confidence": "low",
            "kind": {"kind": "lint_fix", "warning_text": "w"},
            "status": status,
            "audit_origin_round": 1,
            "swept_parents": [],
            "region_block_lines": None,
        }

    total = _PROMPT_LIST_TRUNCATION_THRESHOLD + 5
    pending_positions = list(range(total - 4, total))
    tasks = [
        task(
            i,
            {"kind": "completed"} if i % 2 == 0 else {"kind": "dismissed", "reason": "x"},
        )
        for i in range(total - 4)
    ] + [task(i, {"kind": "pending"}) for i in pending_positions]

    fence = _json_fence(_render_cleanup_audit_tasks(tasks))
    assert fence.startswith("```json\n") and fence.endswith("\n```")
    rendered = json.loads(fence[len("```json\n") : -len("\n```")])

    entries = [e for e in rendered if isinstance(e, dict)]
    # The list really was truncated ...
    assert len(entries) == _PROMPT_LIST_TRUNCATION_THRESHOLD
    assert any(isinstance(e, str) and "truncated" in e for e in rendered)
    # ... yet every Pending task survived, ordered first, and only
    # resolved history was dropped.
    head = entries[: len(pending_positions)]
    assert [e["status"]["kind"] for e in head] == ["pending"] * len(pending_positions)
    assert [e["task_index"] for e in head] == pending_positions
    assert all(e["status"]["kind"] != "pending" for e in entries[len(pending_positions) :])
    # Every rendered entry's task_index points at the task it was
    # computed from in the ORIGINAL array — the index the kernel
    # validates `task_modifications` against.
    for entry in entries:
        assert tasks[entry["task_index"]]["target_node"] == entry["target_node"]

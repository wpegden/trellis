"""Behavioral tests for scripts/resolve_paper_targets.py.

The resolver mirrors the kernel's main-result scan in Python so the viewer's
target page can explain *why* a block is missing. Two things therefore matter:
the mirror must agree with the kernel on what a candidate is (mirror drift is
the whole risk), and every diagnostic must describe a rule the kernel actually
has. The first is tested against the real kernel CLI; the second against
fixture papers, one per reason code.
"""
from __future__ import annotations

import json
import os
import shlex
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "resolve_paper_targets.py"
FIXTURES = ROOT / "tests" / "fixtures" / "paper_targets"

# Every fixture the kernel can read (all of them; the non-UTF-8 case is built
# in a tmp dir because the kernel hard-errors on it by design).
FIXTURE_NAMES = sorted(path.name for path in FIXTURES.glob("*.tex"))


def kernel_cmd() -> list[str] | None:
    """A prebuilt kernel CLI, or None. Never builds: the kernel is not ours."""
    override = os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD", "").strip()
    if override:
        return shlex.split(override)
    for relative in (
        "kernel/target/debug/trellis_runtime_cli",
        "kernel/target/release/trellis_runtime_cli",
    ):
        candidate = ROOT / relative
        if candidate.is_file():
            return [str(candidate)]
    return None


def require_kernel_cmd() -> list[str]:
    """The kernel CLI, or a LOUD failure — never a silent skip.

    The mirror is only trustworthy while something checks it against the
    kernel. Skipping when no binary is present voids the drift guard
    entirely and reports a clean run having compared nothing, which is
    precisely the shape of failure the guard exists to prevent: the
    resolver's diagnostics would go on telling an author why their theorem
    was rejected with nothing confirming the resolver still agrees with the
    scanner that actually decides.

    `kernel/tests/local_closure_smoke.rs` takes the same stance for the same
    reason — it panics rather than skipping because "a vacuous pass would
    report safety that nothing checked". Set ``TRELLIS_ALLOW_FIXTURE_SKIP=1``
    to accept the gap deliberately.
    """
    cmd = kernel_cmd()
    if cmd is not None:
        return cmd
    if os.environ.get("TRELLIS_ALLOW_FIXTURE_SKIP", "").strip() == "1":
        pytest.skip("no prebuilt kernel CLI (TRELLIS_ALLOW_FIXTURE_SKIP=1)")
    pytest.fail(
        "no prebuilt kernel CLI, so the kernel-agreement guard cannot run — "
        "and passing here would report agreement that nothing checked. "
        "Build one ('cargo build --release' in kernel/), point "
        "TRELLIS_TRELLIS_KERNEL_CMD at a binary, or set "
        "TRELLIS_ALLOW_FIXTURE_SKIP=1 to accept the gap deliberately."
    )


def resolve(paper: Path, *args: str) -> dict:
    process = subprocess.run(
        [sys.executable, str(SCRIPT), str(paper), *args],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    assert process.returncode == 0, process.stderr
    return json.loads(process.stdout)


def fixture(name: str, *args: str) -> dict:
    return resolve(FIXTURES / name, *args)


def reasons(result: dict) -> set[str]:
    return {entry["reason_code"] for entry in result["rejected_blocks"]}


def rejected(result: dict, code: str) -> list[dict]:
    return [entry for entry in result["rejected_blocks"] if entry["reason_code"] == code]


def warnings(result: dict) -> set[str]:
    return {entry["code"] for entry in result["file_warnings"]}


def kernel_resolve(paper: Path, main_envs: list[str] | None = None) -> dict:
    cmd = kernel_cmd()
    assert cmd is not None
    payload = {
        "action": "resolve_main_result_targets",
        "paper_path": str(paper.resolve()),
        "raw_targets": None,
        "raw_labels": None,
    }
    if main_envs is not None:
        payload["main_result_envs"] = list(main_envs)
    process = subprocess.run(
        cmd, input=json.dumps(payload), capture_output=True, text=True, timeout=120
    )
    assert process.returncode == 0, process.stderr
    response = json.loads(process.stdout)
    assert response.get("status") == "resolve_main_result_targets_ok", response
    return response["output"]


# ---------------------------------------------------------------------------
# Kernel agreement — the mirror-drift guard
# ---------------------------------------------------------------------------

# The env sets the mirror is held to kernel agreement under: the default pair,
# a narrowed set, a widened set (the §1.4 swallow direction lives here), and
# the full canonical six — every membership a legal knob value can toggle.
ENV_SETS: list[list[str] | None] = [
    None,
    ["theorem"],
    ["theorem", "corollary", "proposition"],
    ["corollary", "definition", "helper", "lemma", "proposition", "theorem"],
]


def _env_set_id(env_set: list[str] | None) -> str:
    return "default" if env_set is None else "+".join(env_set)


_KNOB_SUPPORT: dict[str, bool] = {}


def kernel_supports_knob() -> bool:
    """Probe once per kernel binary: a knob-aware kernel rejects a bogus env.

    The request enum has no deny_unknown_fields, so a pre-knob kernel accepts
    `main_result_envs`, ignores it, and answers under the default set —
    comparing a widened mirror scan against that answer would blame the kernel
    for our own gap. A knob-aware kernel validates the list, so an impossible
    env name distinguishes the two.
    """
    cmd = kernel_cmd()
    assert cmd is not None
    key = shlex.join(cmd)
    if key not in _KNOB_SUPPORT:
        payload = {
            "action": "resolve_main_result_targets",
            "paper_path": str((FIXTURES / "basic.tex").resolve()),
            "raw_targets": None,
            "raw_labels": None,
            "main_result_envs": ["__trellis_env_knob_probe__"],
        }
        process = subprocess.run(
            cmd, input=json.dumps(payload), capture_output=True, text=True, timeout=120
        )
        if process.returncode != 0:
            _KNOB_SUPPORT[key] = True
        else:
            response = json.loads(process.stdout)
            _KNOB_SUPPORT[key] = (
                response.get("status") != "resolve_main_result_targets_ok"
            )
    return _KNOB_SUPPORT[key]


@pytest.mark.parametrize("env_set", ENV_SETS, ids=_env_set_id)
@pytest.mark.parametrize("name", FIXTURE_NAMES)
def test_candidates_agree_with_kernel(name: str, env_set: list[str] | None) -> None:
    """Whatever the kernel calls a target, the resolver calls a candidate —
    for every fixture, under every tested env set. If these two ever disagree,
    the diagnostics lie to the author; this is the test that must not narrow.
    """
    require_kernel_cmd()
    if env_set is not None and not kernel_supports_knob():
        pytest.skip("kernel binary predates the main_result_envs knob")
    paper = FIXTURES / name
    output = kernel_resolve(paper, env_set)
    args = [] if env_set is None else ["--main-result-envs", ",".join(env_set)]
    result = resolve(paper, *args)

    expected = [
        (target["start_line"], target["end_line"], target.get("tex_label"))
        for target in output["targets"]
    ]
    actual = [
        (entry["start_line"], entry["end_line"], entry["tex_label"])
        for entry in result["candidates"]
    ]
    assert actual == expected
    assert result["available_labels"] == sorted(output["available_labels"])
    # Candidate text is the kernel's block text verbatim, not a re-render.
    assert [entry["text"] for entry in result["candidates"]] == [
        item["text"] for item in output["preview"]
    ]


@pytest.mark.parametrize("name", FIXTURE_NAMES)
def test_self_check_against_kernel_reports_no_drift(name: str) -> None:
    """--kernel-cmd cross-checks the mirror and stays silent when it agrees."""
    cmd = require_kernel_cmd()
    result = fixture(name, "--kernel-cmd", shlex.join(cmd))
    assert "mirror_disagreement" not in warnings(result)
    assert "kernel_check_unavailable" not in warnings(result)


def test_widened_env_set_is_cross_checked_against_the_kernel() -> None:
    """The knob path needs the same drift guard the default path has."""
    cmd = require_kernel_cmd()
    result = fixture(
        "starred.tex",
        "--main-result-envs",
        "theorem,corollary,proposition",
        "--kernel-cmd",
        shlex.join(cmd),
    )
    if "kernel_knob_unsupported" in warnings(result):
        pytest.skip("kernel binary predates the main_result_envs knob")
    assert "mirror_disagreement" not in warnings(result)
    assert "prop:one" in [entry["key"] for entry in result["candidates"]]


def test_kernel_too_old_to_widen_is_not_reported_as_disagreement(tmp_path: Path) -> None:
    """A pre-knob kernel ignores main_result_envs; that is our gap, not drift.

    The request enum has no deny_unknown_fields, so an old binary answers under
    the default set and looks like it disagrees. Blaming the diagnostics for
    that would tell the user they are unreliable when they were right.
    """
    old_kernel = tmp_path / "old_kernel.py"
    old_kernel.write_text(
        "import json, sys\n"
        "sys.stdin.read()  # the field it does not know about is simply ignored\n"
        "print(json.dumps({'status': 'resolve_main_result_targets_ok',\n"
        "  'output': {'targets': [{'start_line': 13, 'end_line': 15,\n"
        "                          'tex_label': 'thm:infigure'}],\n"
        "             'available_labels': ['thm:infigure'], 'preview': []}}))\n",
        encoding="utf-8",
    )
    result = fixture(
        "starred.tex",
        "--main-result-envs",
        "theorem,corollary,proposition",
        "--kernel-cmd",
        f"{shlex.quote(sys.executable)} {old_kernel}",
    )
    assert "kernel_knob_unsupported" in warnings(result)
    assert "mirror_disagreement" not in warnings(result)
    message = next(
        entry["message"] for entry in result["file_warnings"]
        if entry["code"] == "kernel_knob_unsupported"
    )
    assert "predates" in message
    # The widened candidates still stand; only the cross-check was skipped.
    assert "prop:one" in [entry["key"] for entry in result["candidates"]]


def test_mirror_disagreement_is_reported_not_hidden(tmp_path: Path) -> None:
    """A kernel that disagrees produces a warning rather than silent divergence."""
    liar = tmp_path / "liar.py"
    liar.write_text(
        "import json, sys\n"
        "sys.stdin.read()\n"
        "print(json.dumps({'status': 'resolve_main_result_targets_ok',\n"
        "  'output': {'targets': [{'start_line': 1, 'end_line': 2}],\n"
        "             'available_labels': [], 'preview': []}}))\n",
        encoding="utf-8",
    )
    result = fixture("basic.tex", "--kernel-cmd", f"{shlex.quote(sys.executable)} {liar}")
    assert "mirror_disagreement" in warnings(result)


# ---------------------------------------------------------------------------
# Candidates and the advisory ranking (§5.4)
# ---------------------------------------------------------------------------

def test_ordinary_paper_ranks_the_referenced_main_theorem_first() -> None:
    result = fixture("basic.tex")
    keys = [entry["key"] for entry in result["candidates"]]
    assert keys == ["thm:main", "lines:30-32"]

    main = result["candidates"][0]
    assert main["rank"] == 1
    assert main["preselected"] is True
    assert main["first_class"] is True
    assert "label is \\ref'd before the first \\section" in main["rank_reasons"]
    assert any("main result" in reason for reason in main["rank_reasons"])

    trailing = result["candidates"][1]
    assert trailing["rank"] == 2
    assert trailing["preselected"] is False
    assert trailing["first_class"] is False
    assert "add-targets" in trailing["note"]


def test_ranking_never_changes_the_candidate_set() -> None:
    """Ranking is presentation: same candidates, whatever the scores say."""
    result = fixture("basic.tex")
    assert sorted(entry["rank"] for entry in result["candidates"]) == [1, 2]
    assert any(entry["preselected"] for entry in result["candidates"])


def test_unlabeled_candidate_is_keyed_by_lines() -> None:
    result = fixture("basic.tex")
    unlabeled = result["candidates"][1]
    assert unlabeled["key"] == f"lines:{unlabeled['start_line']}-{unlabeled['end_line']}"
    assert unlabeled["tex_label"] is None


# ---------------------------------------------------------------------------
# Reason codes
# ---------------------------------------------------------------------------

def test_lemma_is_rejected_as_not_a_main_result() -> None:
    result = fixture("basic.tex")
    entry = rejected(result, "env_not_main_result")[0]
    assert entry["env"] == "lemma"
    assert entry["label"] == "lem:key"
    assert (entry["start_line"], entry["end_line"]) == (18, 20)
    assert "theorem" in entry["message"] and "corollary" in entry["message"]


def test_input_directive_is_flagged_with_its_line() -> None:
    result = fixture("basic.tex")
    message = next(
        entry["message"] for entry in result["file_warnings"]
        if entry["code"] == "input_directives"
    )
    assert "\\input{intro}" in message
    assert "line 14" in message


def test_alias_environments_yield_no_candidates_and_a_mapping_fixit() -> None:
    result = fixture("aliases.tex")
    assert result["candidates"] == []
    codes = {entry["env"]: entry for entry in rejected(result, "alias_unnormalized")}
    assert set(codes) == {"thm", "conj", "lem"}
    assert "thm=theorem" in codes["thm"]["message"]
    assert "lem=lemma" in codes["lem"]["message"]
    # \newtheorem{conj}{Conjecture} maps onto nothing canonical: say so, and do
    # not invent a target for it.
    assert "conj=" not in codes["conj"]["message"]
    unmapped = [
        entry for entry in result["file_warnings"]
        if entry["code"] == "unmapped_newtheorem"
    ]
    assert len(unmapped) == 1 and "conj" in unmapped[0]["message"]


def test_newtheorem_declarations_shape_envs_present() -> None:
    result = fixture("aliases.tex")
    envs = {entry["env"]: entry for entry in result["envs_present"]}
    assert envs["thm"]["count"] == 1
    assert envs["thm"]["canonical"] == "theorem"
    assert envs["thm"]["declared_title"] == "Theorem"
    assert envs["conj"]["canonical"] is None
    assert envs["conj"]["declared_title"] == "Conjecture"
    assert envs["conj"]["main_result"] is False


def test_env_map_turns_an_alias_into_a_real_candidate() -> None:
    result = fixture("aliases.tex", "--env-map", "thm=theorem")
    assert [entry["key"] for entry in result["candidates"]] == ["t:alias"]
    assert result["normalization"]["explicit_env_map"] == {"thm": "theorem"}
    assert result["normalization"]["rewrites"]["begin:thm->theorem"] == 1
    assert "alias_unnormalized" not in {
        entry["reason_code"]
        for entry in result["rejected_blocks"]
        if entry["env"] == "thm"
    }


def test_widening_the_env_set_admits_a_proposition() -> None:
    result = fixture("starred.tex", "--main-result-envs", "theorem,corollary,proposition")
    assert "prop:one" in [entry["key"] for entry in result["candidates"]]
    assert result["main_result_envs"] == ["theorem", "corollary", "proposition"]
    assert "env_not_main_result" not in reasons(result)


def test_starred_environment_is_rejected_with_the_numbered_form_fixit() -> None:
    result = fixture("starred.tex")
    entry = rejected(result, "starred_env")[0]
    assert entry["env"] == "theorem*"
    assert "\\begin{theorem}" in entry["message"]


def test_theorem_inside_a_non_matched_env_is_still_a_candidate() -> None:
    """R8, the other direction: only *matched* envs make the scan jump."""
    result = fixture("starred.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:infigure"]


def test_theorem_nested_in_a_candidate_is_explained() -> None:
    result = fixture("nesting.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:outer"]
    entry = rejected(result, "nested_in_candidate")[0]
    assert entry["env"] == "corollary"
    assert entry["label"] == "cor:inner"
    assert "4-9" in entry["message"]


def test_missing_end_marker_is_explained() -> None:
    result = fixture("nesting.tex")
    entry = rejected(result, "unterminated_env")[0]
    assert entry["label"] == "thm:unterminated"
    assert "\\end{theorem}" in entry["message"]


def test_commented_and_out_of_window_blocks_are_distinguished() -> None:
    result = fixture("window.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:inside"]
    assert rejected(result, "commented_out")[0]["label"] == "thm:commented"
    outside = rejected(result, "outside_document_window")[0]
    assert outside["label"] == "thm:after"
    # The window closed at the commented-out \end{document} on line 12.
    assert "line 12" in outside["message"]


def test_label_rules_are_each_reported() -> None:
    result = fixture("labels.tex")
    second = rejected(result, "label_not_first")[0]
    assert second["label"] == "thm:second"
    assert "thm:first" in second["message"]

    duplicate = rejected(result, "duplicate_label")[0]
    assert duplicate["label"] == "thm:first"
    assert duplicate["env"] == "corollary"
    # The kernel's dedup keeps the FIRST block with a key, not the last.
    assert "4-6" in duplicate["message"]

    outside = rejected(result, "label_outside_env")[0]
    assert outside["label"] == "thm:outside"
    assert "Move the \\label inside" in outside["message"]


def test_capitalized_env_is_closed_by_its_own_end() -> None:
    """\\begin{Theorem} is matched case-insensitively and closed by \\end{Theorem}."""
    result = fixture("mixedcase.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:upper", "cor:plain"]
    assert "env_case_end_mismatch" not in reasons(result)
    upper = result["candidates"][0]
    assert (upper["start_line"], upper["end_line"]) == (4, 6)


def test_capitalized_env_does_not_swallow_the_block_after_it() -> None:
    """The block ends at its own \\end, not at the next lowercase one."""
    result = fixture("mixedcase_swallow.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:upper", "thm:lower"]
    assert result["available_labels"] == ["thm:lower", "thm:upper"]
    assert reasons(result) == set()


def test_braceless_end_in_verbatim_does_not_swallow_the_next_theorem() -> None:
    """`\\verb|\\end{|` is not an environment name: it must not eat a real close."""
    result = fixture("verb_end.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:verb", "thm:next"]
    assert reasons(result) == set()
    assert "thm:next" not in result["candidates"][0]["text"]


def test_differently_cased_nesting_resolves_the_outer_block() -> None:
    """Theorem and theorem are distinct envs: the inner \\end closes the inner block."""
    result = fixture("mixedcase_nested.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:outer"]
    assert "env_case_end_mismatch" not in reasons(result)
    outer = result["candidates"][0]
    assert (outer["start_line"], outer["end_line"]) == (5, 10)
    # The inner block is swallowed by the outer one, exactly as any statement
    # nested in a candidate is, and it says so.
    assert rejected(result, "nested_in_candidate")[0]["label"] == "thm:inner"


def test_mismatched_case_pair_is_rejected_without_swallowing() -> None:
    """\\begin{Theorem}...\\end{theorem} is invalid LaTeX: it costs its own block only."""
    result = fixture("mixedcase_mismatch.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:ok"]
    entry = rejected(result, "env_case_end_mismatch")[0]
    assert entry["label"] == "thm:mismatch"
    assert (entry["start_line"], entry["end_line"]) == (4, 6)
    assert "\\end{theorem}" in entry["message"]
    assert "case-sensitive" in entry["message"]
    # The block after it survives, with its own label, unabsorbed.
    assert "thm:mismatch" not in result["available_labels"]


def test_malformed_begin_reports_where_scanning_stopped_and_admits_the_gap() -> None:
    result = fixture("truncated.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:before"]
    assert result["scan_truncated"]["line"] == 14
    assert "cannot be diagnosed" in result["scan_truncated"]["message"]
    # The earlier malformed \begin{ swallowed a real theorem: say so rather than
    # attributing a reason to a block the scanner never examined.
    swallowed = next(
        entry for entry in result["file_warnings"] if entry["code"] == "malformed_begin"
    )
    assert "line 8" in swallowed["message"]
    assert "cannot be diagnosed" in swallowed["message"]
    assert not any(
        entry["label"] == "thm:swallowed" for entry in result["rejected_blocks"]
    )


def test_paper_with_no_recognizable_environments_says_so_honestly() -> None:
    result = fixture("no_envs.tex")
    assert result["candidates"] == []
    assert result["rejected_blocks"] == []
    message = next(
        entry["message"] for entry in result["file_warnings"]
        if entry["code"] == "no_recognizable_envs"
    )
    assert "cannot guess" in message


def test_ordinary_prose_environments_are_not_diagnosed() -> None:
    """No fix-it for \\begin{itemize}: a reason code there would be noise."""
    result = fixture("no_envs.tex")
    assert "itemize" in {entry["env"] for entry in result["envs_present"]}
    assert result["rejected_blocks"] == []


# ---------------------------------------------------------------------------
# Diagnostic quality: no fix-it that sends an author editing for nothing, and
# no theorem-like block that silently gets no reason at all.
# ---------------------------------------------------------------------------

def test_equation_label_inside_an_accepted_theorem_draws_no_fixit() -> None:
    """An \\label in a nested equation never competed to name the theorem."""
    result = fixture("eq_labels.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:a", "thm:b"]
    assert "eq:1" in result["available_labels"]
    assert not any(
        entry["label"] == "eq:1" or "eq:1" in entry["message"]
        for entry in result["rejected_blocks"]
    )


def test_second_label_of_the_theorem_itself_is_still_reported() -> None:
    """The real trap survives the F3 narrowing: a direct second label."""
    result = fixture("eq_labels.tex")
    entries = rejected(result, "label_not_first")
    assert [entry["label"] for entry in entries] == ["thm:b-alt"]
    entry = entries[0]
    assert entry["block_accepted"] is True  # the block itself was accepted
    assert "thm:b" in entry["message"]
    # A run configured by this label resolves without a line range and is then
    # dropped at init; say that rather than implying the block is missing.
    assert "dropped at init" in entry["message"]


def test_block_nested_in_a_deduped_block_still_gets_a_reason() -> None:
    result = fixture("nested_in_dup.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:a"]
    assert "cor:c" in result["available_labels"]
    entry = rejected(result, "nested_in_dropped_block")[0]
    assert entry["label"] == "cor:c"
    assert "dropped as a duplicate" in entry["message"]
    assert "8-13" in entry["message"]


def test_space_before_the_brace_hides_a_block_and_is_reported() -> None:
    """`\\begin {theorem}` compiles in LaTeX and is invisible to the scan."""
    result = fixture("spaced_begin.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["thm:seen"]
    entry = rejected(result, "spaced_begin_marker")[0]
    assert entry["label"] == "thm:spaced"
    assert entry["start_line"] == 4


def test_spaced_end_marker_explains_the_unterminated_block(tmp_path: Path) -> None:
    paper = tmp_path / "spaced_end.tex"
    paper.write_text(
        "\\documentclass{article}\n"
        "\\begin{document}\n"
        "\\begin{theorem}\\label{thm:x}\n"
        "Body.\n"
        "\\end {theorem}\n"
        "\\end{document}\n",
        encoding="utf-8",
    )
    result = resolve(paper)
    entry = rejected(result, "unterminated_env")[0]
    assert "space before its brace at line 5" in entry["message"]


def test_two_unlabeled_blocks_on_one_line_report_a_key_collision() -> None:
    """Not a duplicate *label*: there is no label, and repr(None) must not leak."""
    result = fixture("line_collision.tex")
    assert [entry["key"] for entry in result["candidates"]] == ["lines:3-3"]
    assert reasons(result) == {"duplicate_line_key"}
    entry = rejected(result, "duplicate_line_key")[0]
    assert entry["label"] is None
    assert entry["env"] == "corollary"
    assert "lines:3-3" in entry["message"]


@pytest.mark.parametrize("name", FIXTURE_NAMES)
def test_no_diagnostic_leaks_a_python_repr(name: str) -> None:
    result = fixture(name)
    for entry in result["rejected_blocks"] + result["file_warnings"]:
        assert "None" not in entry["message"], entry


@pytest.mark.parametrize("name", FIXTURE_NAMES)
def test_every_labeled_non_candidate_gets_a_reason(name: str) -> None:
    """The module's contract: a theorem-like block that missed is explained.

    available_labels covers exactly the blocks the scan matched, so any label in
    it that does not bind a candidate names a block the author expected to see.
    """
    result = fixture(name)
    if result["scan_truncated"] is not None:
        pytest.skip("post-truncation blocks are declared undiagnosable by design")
    explained = {entry["label"] for entry in result["rejected_blocks"]}
    for label in result["available_labels"]:
        inside_accepted = any(
            f"\\label{{{label}}}" in entry["text"] for entry in result["candidates"]
        )
        assert inside_accepted or label in explained, label


# ---------------------------------------------------------------------------
# Intake
# ---------------------------------------------------------------------------

def test_non_utf8_paper_is_transcoded_and_flagged(tmp_path: Path) -> None:
    paper = tmp_path / "latin1.tex"
    paper.write_bytes(
        b"\\documentclass{article}\n"
        b"\\begin{document}\n"
        b"\\begin{theorem}\\label{thm:accent}\n"
        b"Poincar\xe9 conjectured this.\n"
        b"\\end{theorem}\n"
        b"\\end{document}\n"
    )
    result = resolve(paper)
    assert [entry["key"] for entry in result["candidates"]] == ["thm:accent"]
    message = next(
        entry["message"] for entry in result["file_warnings"]
        if entry["code"] == "not_utf8"
    )
    assert "UTF-8" in message


def test_paper_sha256_and_name_are_recorded(tmp_path: Path) -> None:
    import hashlib

    paper = FIXTURES / "basic.tex"
    result = resolve(paper, "--paper-name", "uploaded.tex")
    assert result["paper_name"] == "uploaded.tex"
    assert result["paper_sha256"] == hashlib.sha256(paper.read_bytes()).hexdigest()


def test_out_writes_the_document_the_page_renders(tmp_path: Path) -> None:
    out = tmp_path / "targets_resolution.json"
    process = subprocess.run(
        [sys.executable, str(SCRIPT), str(FIXTURES / "basic.tex"), "--out", str(out)],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    assert process.returncode == 0, process.stderr
    assert process.stdout == ""
    assert json.loads(out.read_text(encoding="utf-8")) == fixture("basic.tex")

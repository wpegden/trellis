"""Sidecar driver primitives: splice property, ban scan, prompt
assembly, spool record schema (plan commit 9; test plan §6.2; amendment
A3).

The mocked-API loop this module also used to cover went with the HTTP
arm — ``ModelClient``, the tool schemas, context compaction and the
transport classifier are gone, and the generator is now
``codex_driver.run_attempt_codex`` (covered by
``tests/test_sidecar_codex_driver.py``). What is left here is the
arm-independent half of an attempt.
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

import pytest

from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.driver import (
    AXIOM_FLOOR,
    BANNED_TOKENS,
    SIDECAR_EXTRA_BANNED_TOKENS,
    AttemptResult,
    attempt_timings,
    body_ban_scan,
    build_attempt_record,
    build_system_prompt,
    build_system_prompt_v2,
    load_approved_axioms_list,
    splice_body,
    split_body_marker,
)

REPO_ROOT = Path(__file__).resolve().parents[1]

NODE_FILE = (
    "import Tablet.Preamble\nimport Tablet.Dep\n\n-- [TABLET NODE: Rung]\n"
    "theorem Rung : True := by\n-- BODY\n  sorry\n"
)


def _cfg(**overrides: Any) -> SidecarConfig:
    base = dict(
        enabled=True,
        attempt_wall_seconds=1800.0,
        max_iterations=12,
        attempt_tokens=500_000,
    )
    base.update(overrides)
    return SidecarConfig(**base)


# ---------------------------------------------------------------------------
# Marker split + splice property (adversarial)
# ---------------------------------------------------------------------------


def test_split_requires_exactly_one_marker() -> None:
    prefix, body = split_body_marker(NODE_FILE)
    assert prefix.endswith("-- BODY\n")
    assert body == "  sorry\n"
    with pytest.raises(ValueError):
        split_body_marker(NODE_FILE + "-- BODY\n")
    with pytest.raises(ValueError):
        split_body_marker("theorem X : True := by\n  trivial\n")


@pytest.mark.parametrize(
    "adversarial_body",
    [
        "  trivial\n-- BODY\n  sorry\n",  # smuggled marker
        NODE_FILE,  # whole-file emission
        "  -- ∀ ε > 0 ∃ δ 🎯 unicode\r\n  trivial\r\n",  # unicode/CRLF
        "```lean\n  trivial\n```\n",  # markdown fence residue
        "",  # empty
    ],
)
def test_splice_never_mutates_the_prefix(adversarial_body: str) -> None:
    prefix, _ = split_body_marker(NODE_FILE)
    spliced = splice_body(prefix, adversarial_body)
    assert spliced[: len(prefix)] == prefix, "prefix must be byte-identical"
    assert spliced[len(prefix) :] == adversarial_body


# ---------------------------------------------------------------------------
# Ban scan (A3)
# ---------------------------------------------------------------------------


def test_ban_list_mirrors_kernel_constants() -> None:
    """BANNED_TOKENS = FORBIDDEN_KEYWORDS_DEFAULT (Rust-mirrored, sorryAx
    first) + SIDECAR_EXTRA_BANNED_TOKENS (mirror of the Rust
    kernel/src/sidecar.rs constant, A3 tokens included)."""
    assert BANNED_TOKENS[0] == "sorryAx"
    for a3_token in ("attribute", "deriving", "export"):
        assert a3_token in SIDECAR_EXTRA_BANNED_TOKENS
    sidecar_rs = (REPO_ROOT / "kernel" / "src" / "sidecar.rs").read_text()
    match = re.search(
        r"pub const SIDECAR_EXTRA_BANNED_TOKENS: &\[&str\] = &\[(.*?)\];",
        sidecar_rs,
        re.DOTALL,
    )
    assert match is not None
    rust_extras = re.findall(r'"([^"]+)"', match.group(1))
    assert list(SIDECAR_EXTRA_BANNED_TOKENS) == rust_extras


def test_ban_scan_hits_every_token_and_respects_boundaries() -> None:
    for token in BANNED_TOKENS:
        assert body_ban_scan(f"  exact foo\n  {token} bar\n") == token
    # Comments included.
    assert body_ban_scan("  trivial -- attribute [simp] hidden\n") == "attribute"
    # Identifier containment does not trip.
    assert body_ban_scan("  exact attributeFoo my_export' derivingX sorryish\n") is None
    assert body_ban_scan("  intro h\n  simpa using h\n") is None


def test_ban_scan_rejects_exec_command_family() -> None:
    """Finding 1: the elaboration-time arbitrary-IO exec primitives.
    `run_tac` is a TACTIC reachable directly in a proof body; `run_elab`
    / `run_meta` are command-slot forms. Each executes IO under the
    tablet toolchain and must be rejected; `run_cmd`/`#eval` were already
    covered."""
    for token in ("run_tac", "run_elab", "run_meta"):
        assert token in BANNED_TOKENS
    assert body_ban_scan("  run_tac do pure ()\n  trivial\n") == "run_tac"
    assert body_ban_scan("  trivial\nrun_elab do pure ()\n") == "run_elab"
    assert body_ban_scan("  trivial\nrun_meta do pure ()\n") == "run_meta"
    # Prefix identifiers do not trip the token-boundary scan.
    assert body_ban_scan("  exact run_tactical\n") is None
    assert body_ban_scan("  exact run_elaborate\n") is None


# ---------------------------------------------------------------------------
# Prompt assembly
# ---------------------------------------------------------------------------


def _seed_repo(tmp_path: Path) -> Path:
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "Rung.lean").write_text(NODE_FILE)
    (repo / "Tablet" / "Dep.lean").write_text(
        "import Tablet.Preamble\n\n-- [TABLET NODE: Dep]\n"
        "theorem Dep : 1 = 1 := by\n-- BODY\n  rfl\n"
    )
    return repo


def test_system_prompt_templates_bans_axioms_and_deps(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    (repo / "APPROVED_AXIOMS.json").write_text(
        json.dumps({"global": ["Extra.global"], "nodes": {"Rung": ["Extra.rung"]}})
    )
    prompt = build_system_prompt(repo, "Rung", NODE_FILE)
    # Frozen prefix present verbatim; the sorry body absent.
    assert "-- [TABLET NODE: Rung]" in prompt
    assert "theorem Rung : True := by" in prompt
    assert "  sorry" not in prompt
    # Ban list templated from the (fixed) Python mirror + extras.
    assert "sorryAx" in prompt and "attribute" in prompt and "export" in prompt
    # Approved axioms: floor ∪ global ∪ per-node (E-D6).
    for axiom in AXIOM_FLOOR + ("Extra.global", "Extra.rung"):
        assert axiom in prompt
    # Dep statement block: Dep's signature region, without its body.
    assert "theorem Dep : 1 = 1 := by" in prompt
    assert "  rfl" not in prompt


def test_load_approved_axioms_floor_on_parse_error(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    (repo / "APPROVED_AXIOMS.json").write_text("{broken")
    assert load_approved_axioms_list(repo, "Rung") == list(AXIOM_FLOOR)


def test_toolless_prompt_keeps_the_maths_and_drops_the_tool_workflow(
    tmp_path: Path,
) -> None:
    """The shape the codex arm asks for — every tool flag off AND
    ``tools_available=False``, the one call site left. The mathematical
    content (frozen prefix, dependency statements, approved axioms)
    survives; the numbered workflow built on `read_file` /
    `search_tablet` / `lean_run_code` does not, since that agent has a
    shell and its caller states the real mechanism. The persistence
    directive — the only text telling the model not to stop — must
    still ride along."""
    repo = _seed_repo(tmp_path)
    prompt = build_system_prompt_v2(
        repo,
        "Rung",
        NODE_FILE,
        search_enabled=False,
        goals_enabled=False,
        mathlib_source_enabled=False,
        tools_available=False,
    )
    assert prompt.startswith("You are a Lean 4 proof engineer")
    assert "-- [TABLET NODE: Rung]" in prompt
    assert "theorem Dep : 1 = 1 := by" in prompt
    assert "TOOLS AND WORKFLOW" not in prompt
    assert "lean_run_code" not in prompt
    assert "get_goals" not in prompt
    assert "This is a LONG task: keep working." in prompt


# ---------------------------------------------------------------------------
# Record schema
# ---------------------------------------------------------------------------


def test_build_attempt_record_schema() -> None:
    result = AttemptResult(
        status="success",
        proof_body="  trivial\n",
        iterations=7,
        prompt_tokens=231044,
        completion_tokens=48211,
        wall_secs=811.4,
    )
    record = build_attempt_record(
        config=_cfg(),
        attempt_id="sc-20260722-213301-Rung",
        node="Rung",
        entry_seq=7,
        snapshot_sha="65bfc28",
        candidate={
            "node_file_sha256": "nf",
            "statement_prefix_sha256": "sp",
        },
        workspace_fingerprints={"source_closure_hash": "sch"},
        result=result,
        daemon_validation={"compiled": True},
    )
    assert record["schema"] == 2
    assert record["entry_seq"] == 7, "A3: records carry the queue generation"
    assert record["base"] == {
        "node_file_sha256": "nf",
        "statement_prefix_sha256": "sp",
    }
    assert record["artifact"]["proof_body"] == "  trivial\n"
    assert record["provenance"]["tokens"] == {"prompt": 231044, "completion": 48211}
    assert record["provenance"]["model"] == "gpt-5.6-luna"
    assert record["status"] == "success"
    # Telemetry is skip-when-default: a result with no phase data adds
    # no `timings` key (serde-compat: pre-telemetry shape unchanged).
    assert "timings" not in record["provenance"]


def test_attempt_record_timings_are_additive() -> None:
    """Grunt-bench fair-cost instrumentation: phase timings ride the
    provenance additively (api/check from the loop, refresh/server-open
    from the runner), and zero phases are omitted."""
    result = AttemptResult(
        status="success",
        proof_body="  trivial\n",
        iterations=2,
        wall_secs=41.0,
        api_secs=30.25,
        check_secs=6.5,
        info_tool_calls=3,
    )
    assert attempt_timings(AttemptResult(status="failed")) == {}
    record = build_attempt_record(
        config=_cfg(),
        attempt_id="sc-1",
        node="Rung",
        entry_seq=1,
        snapshot_sha="abc",
        candidate={},
        workspace_fingerprints={},
        result=result,
        daemon_validation={},
        timings={"refresh_secs": 0.21, "server_open_secs": 5.9, "zero_phase": 0.0},
    )
    assert record["provenance"]["timings"] == {
        "api_secs": 30.25,
        "check_secs": 6.5,
        "info_tool_calls": 3,
        "refresh_secs": 0.21,
        "server_open_secs": 5.9,
    }

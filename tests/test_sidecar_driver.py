"""Sidecar driver: mocked-API loop, splice property, ban scan, budgets,
key hygiene (plan commit 9; test plan §6.2; amendments A3 + A9)."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import pytest

from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.driver import (
    AXIOM_FLOOR,
    BANNED_TOKENS,
    READ_FILE_MAX_BYTES,
    READ_FILE_MAX_WINDOW_LINES,
    READ_FILE_TOOL,
    SEARCH_MATHLIB_MAX_CALLS,
    SEARCH_MATHLIB_TOOL,
    SEARCH_MAX_LINE_CHARS,
    SEARCH_RESULT_MAX_BYTES,
    SEARCH_TABLET_DEADLINE_SECS,
    SEARCH_TABLET_MAX_HITS,
    SEARCH_TABLET_TOOL,
    SIDECAR_EXTRA_BANNED_TOKENS,
    WALL_TIMEOUT_GRACE_SECONDS,
    AttemptResult,
    CompileVerdict,
    ModelClient,
    ModelTransportError,
    attempt_timings,
    body_ban_scan,
    build_attempt_record,
    build_system_prompt,
    build_system_prompt_v2,
    info_tool_specs,
    load_approved_axioms_list,
    make_info_tool_handlers,
    mathlib_source_root,
    run_attempt,
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


class FakeResponse:
    def __init__(self, status_code: int, payload: Dict[str, Any], headers=None):
        self.status_code = status_code
        self._payload = payload
        self.headers = headers or {}

    def json(self) -> Dict[str, Any]:
        return self._payload


def _tool_reply(body: str, tokens=(100, 20)) -> Dict[str, Any]:
    return {
        "usage": {"prompt_tokens": tokens[0], "completion_tokens": tokens[1]},
        "choices": [
            {
                "message": {
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "function": {
                                "name": "lean_run_code",
                                "arguments": json.dumps({"body": body}),
                            },
                        }
                    ],
                }
            }
        ],
    }


def _client_with_replies(replies: List[Dict[str, Any]], calls: List[Dict]) -> ModelClient:
    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"url": url, "payload": json})
        return FakeResponse(200, replies[min(len(calls) - 1, len(replies) - 1)])

    return ModelClient(_cfg(), "sk-secret-XYZ", post=post, sleep=lambda s: None)


def assert_assistant_messages_sendable(messages: List[Dict[str, Any]]) -> None:
    """Three request-shape invariants the endpoint enforces with a 400.

    An assistant message carrying neither text nor tool_calls is
    rejected outright ("Assistant message must have either content or
    tool_calls, but not none"). The echo and the tool messages that
    follow it must then correspond EXACTLY, in both directions: an
    echoed tool_call_id no tool message answers is rejected on the
    request after it, and so is a tool message answering an id no
    assistant message echoed. Any of the three ends the attempt
    transport-class, discarding the whole run of work.

    Both directions are checked because `_assistant_echo` decides the
    two ends separately — the caller's `with_tool_calls` promise governs
    the echo, the answering loop governs the tool messages — so a caller
    that drops the calls while still answering them satisfies the
    echoed-are-answered half vacuously."""
    answered = {
        m.get("tool_call_id") for m in messages if m.get("role") == "tool"
    }
    echoed = {
        call.get("id")
        for m in messages
        if m.get("role") == "assistant"
        for call in (m.get("tool_calls") or [])
    }
    for index, message in enumerate(messages):
        if message.get("role") != "assistant":
            continue
        calls = message.get("tool_calls") or []
        assert message.get("content") or calls, (
            f"messages[{index}] is an assistant message with neither "
            "content nor tool_calls"
        )
        for call in calls:
            assert call.get("id") in answered, (
                f"messages[{index}] echoes tool_call_id {call.get('id')!r} "
                "with no tool message answering it"
            )
    for index, message in enumerate(messages):
        if message.get("role") != "tool":
            continue
        assert message.get("tool_call_id") in echoed, (
            f"messages[{index}] answers tool_call_id "
            f"{message.get('tool_call_id')!r} that no assistant message "
            "echoes"
        )


@pytest.fixture(autouse=True)
def _assistant_shape_guard(monkeypatch: pytest.MonkeyPatch) -> None:
    """Hold the shape invariants over EVERY request any test in this
    module sends, not only the ones written to look for them."""
    unguarded = ModelClient.chat

    def guarded(self, messages, **kwargs):  # type: ignore[no-untyped-def]
        assert_assistant_messages_sendable(list(messages))
        return unguarded(self, messages, **kwargs)

    monkeypatch.setattr(ModelClient, "chat", guarded)


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


# ---------------------------------------------------------------------------
# Mocked-API loop
# ---------------------------------------------------------------------------


def test_loop_fail_then_success(tmp_path: Path) -> None:
    calls: List[Dict] = []
    client = _client_with_replies(
        [_tool_reply("  exact absurd\n"), _tool_reply("  trivial\n")], calls
    )
    compiles: List[str] = []

    def compile_body(body: str) -> CompileVerdict:
        compiles.append(body)
        return CompileVerdict(ok=body == "  trivial\n", log="error: absurd")

    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=compile_body,
    )
    assert result.status == "success"
    assert result.proof_body == "  trivial\n"
    assert result.iterations == 2
    assert compiles == ["  exact absurd\n", "  trivial\n"]
    assert result.prompt_tokens == 200 and result.completion_tokens == 40
    # The compiler feedback flowed back as a tool result.
    tool_messages = [
        m
        for call in calls
        for m in call["payload"]["messages"]
        if m.get("role") == "tool"
    ]
    assert any("error: absurd" in m["content"] for m in tool_messages)


def test_banned_body_feeds_back_without_burning_a_compile() -> None:
    calls: List[Dict] = []
    client = _client_with_replies(
        [_tool_reply("  admit\n"), _tool_reply("  trivial\n")], calls
    )
    compiles: List[str] = []

    def compile_body(body: str) -> CompileVerdict:
        compiles.append(body)
        return CompileVerdict(ok=True, log="")

    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=compile_body,
    )
    assert result.status == "success"
    assert compiles == ["  trivial\n"], "banned body must not reach the compiler"
    rejected = [
        m
        for call in calls
        for m in call["payload"]["messages"]
        if m.get("role") == "tool" and "banned" in str(m.get("content"))
    ]
    assert rejected, "ban feedback must be sent to the model"


def test_iteration_budget_trips() -> None:
    calls: List[Dict] = []
    client = _client_with_replies([_tool_reply("  exact nope\n")], calls)
    result = run_attempt(
        config=_cfg(max_iterations=3),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="no"),
    )
    assert result.status == "budget_exhausted"
    assert result.detail == "iteration budget"
    assert result.iterations == 3


def test_token_budget_trips() -> None:
    calls: List[Dict] = []
    client = _client_with_replies(
        [_tool_reply("  exact nope\n", tokens=(400_000, 200_000))], calls
    )
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="no"),
    )
    assert result.status == "budget_exhausted"
    assert result.detail == "token budget"


def test_wall_budget_trips() -> None:
    clock = {"t": 0.0}

    def now() -> float:
        clock["t"] += 1200.0
        return clock["t"]

    calls: List[Dict] = []
    client = _client_with_replies([_tool_reply("  exact nope\n")], calls)
    result = run_attempt(
        config=_cfg(attempt_wall_seconds=1800.0),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="no"),
        now=now,
    )
    assert result.status == "budget_exhausted"
    assert result.detail == "wall budget"


def test_fenced_block_fallback_mode() -> None:
    calls: List[Dict] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"payload": json})
        return FakeResponse(
            200,
            {
                "usage": {"prompt_tokens": 10, "completion_tokens": 5},
                "choices": [
                    {
                        "message": {
                            "content": "Here:\n```lean\n  trivial\n```\ndone"
                        }
                    }
                ],
            },
        )

    client = ModelClient(
        _cfg(tool_calls=False), "sk-secret", post=post, sleep=lambda s: None
    )
    result = run_attempt(
        config=_cfg(tool_calls=False),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "success"
    assert result.proof_body == "  trivial\n"
    # No tools advertised in fallback mode.
    assert "tools" not in calls[0]["payload"]


def test_prose_reply_nudged_once_then_counts_iterations() -> None:
    replies = [
        {
            "usage": {"prompt_tokens": 5, "completion_tokens": 5},
            "choices": [{"message": {"content": "let me think..."}}],
        },
        _tool_reply("  trivial\n"),
    ]
    calls: List[Dict] = []
    client = _client_with_replies(replies, calls)
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "success"
    assert result.iterations == 1, "the nudge turn does not count as an iteration"


def test_prose_replies_past_the_first_each_cost_an_iteration() -> None:
    """The other half of the nudge budget: `nudged` LATCHES, so only the
    first no-candidate turn is free and every one after it is charged.
    Without this the loop has no cap of its own — a model that answers
    with prose forever would be bounded by the wall alone, never by
    `max_iterations`."""
    replies = [
        {
            "usage": {"prompt_tokens": 5, "completion_tokens": 5},
            "choices": [{"message": {"content": "let me think..."}}],
        },
        {
            "usage": {"prompt_tokens": 5, "completion_tokens": 5},
            "choices": [{"message": {"content": "still thinking..."}}],
        },
        {
            "usage": {"prompt_tokens": 5, "completion_tokens": 5},
            "choices": [{"message": {"content": "nearly there..."}}],
        },
        _tool_reply("  trivial\n"),
    ]
    calls: List[Dict] = []
    client = _client_with_replies(replies, calls)
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "success"
    assert len(calls) == 4, "every nudge turn was actually sent"
    assert result.iterations == 3, (
        "one free nudge, then two charged ones, then the compile"
    )


def test_prose_replies_alone_exhaust_the_iteration_budget() -> None:
    """What the charge is FOR: a model that never submits a candidate
    ends the attempt on `max_iterations` rather than running the wall
    out. The cap counts turns after the free one, so it is reached on
    the turn after that.

    The clock is injected and the wall set well beyond where the cap
    falls, purely so this test cannot HANG if the charge ever stops
    being applied: `_client_with_replies` repeats its last reply
    forever, so an uncharged nudge loop would otherwise spin against a
    real 1800 s wall instead of failing."""
    calls: List[Dict] = []
    client = _client_with_replies(
        [
            {
                "usage": {"prompt_tokens": 5, "completion_tokens": 5},
                "choices": [{"message": {"content": "let me think..."}}],
            }
        ],
        calls,
    )
    ticks = iter(range(10_000))
    result = run_attempt(
        config=_cfg(max_iterations=5, attempt_wall_seconds=200.0),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="nope"),
        now=lambda: float(next(ticks)),
    )
    assert result.status == "budget_exhausted"
    assert result.detail == "iteration budget", (
        "the nudge loop ran to the wall instead of being charged"
    )
    assert result.iterations == 5
    assert len(calls) == 6, "the free first nudge is the extra turn"


# ---------------------------------------------------------------------------
# v2 info tools (grunt-bench): read_file + search_mathlib
# ---------------------------------------------------------------------------


def _info_reply(name: str, args: Dict[str, Any], call_id: str = "info-1", tokens=(50, 10)):
    return {
        "usage": {"prompt_tokens": tokens[0], "completion_tokens": tokens[1]},
        "choices": [
            {
                "message": {
                    "content": "",
                    "tool_calls": [
                        {
                            "id": call_id,
                            "function": {
                                "name": name,
                                "arguments": json.dumps(args),
                            },
                        }
                    ],
                }
            }
        ],
    }


def test_info_tool_turn_costs_no_compile_iteration(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    (repo / "Tablet" / "Rung.tex").write_text("prose proof of Rung\n")
    (repo / "Tablet" / "Dep.tex").write_text("prose proof of Dep\n")
    calls: List[Dict] = []
    client = _client_with_replies(
        [
            _info_reply("read_file", {"path": "Tablet/Rung.tex"}),
            _tool_reply("  trivial\n"),
        ],
        calls,
    )
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
        info_tools=make_info_tool_handlers(repo, "Rung"),
        extra_tool_specs=info_tool_specs(mathlib_source_enabled=False),
    )
    assert result.status == "success"
    assert result.iterations == 1, "info turns never count as compile iterations"
    assert result.info_tool_calls == 1
    # The prose came back as a tool message with the matching id.
    tool_messages = [
        m
        for call in calls
        for m in call["payload"]["messages"]
        if m.get("role") == "tool" and m.get("tool_call_id") == "info-1"
    ]
    assert any("prose proof of Rung" in m["content"] for m in tool_messages)
    # All tools advertised: lean_run_code + read_file + search_tablet (v4)
    # + search_mathlib + get_goals (v3). The mathlib grep is absent here:
    # the fixture repo carries no mathlib checkout.
    names = [
        t["function"]["name"] for t in calls[0]["payload"]["tools"]
    ]
    assert names == [
        "lean_run_code",
        "read_file",
        "search_tablet",
        "search_mathlib",
        "get_goals",
    ]


# ---------------------------------------------------------------------------
# v4 retrieval widening (SOL_VS_GRUNT_COMPARISON §4): read reaches every
# Tablet/*.{lean,tex} plus mathlib source, and two grep tools supply the
# mechanism the frontier control arm actually won with — SEARCH.
# ---------------------------------------------------------------------------


def test_read_file_allows_any_tablet_lean_or_tex(tmp_path: Path) -> None:
    """The widening: a sibling `.lean` OUTSIDE the import closure is
    readable — that is the exact file the sol arm cribbed its winning
    line from while the grunt could not open it."""
    repo = _seed_repo(tmp_path)
    (repo / "Tablet" / "Rung.tex").write_text("rung prose\n")
    (repo / "Tablet" / "Dep.tex").write_text("dep prose\n")
    # Not imported by Rung, and not even a dependency of one.
    (repo / "Tablet" / "Cousin.lean").write_text("cousin lean\n")
    (repo / "Tablet" / "Cousin.tex").write_text("cousin prose\n")
    (repo / "APPROVED_AXIOMS.json").write_text("{}")
    handlers = make_info_tool_handlers(repo, "Rung")
    read = handlers["read_file"]
    assert read({"path": "Tablet/Rung.tex"}) == "rung prose\n"
    assert read({"path": "Tablet/Dep.tex"}) == "dep prose\n"
    assert read({"path": "Tablet/Cousin.lean"}) == "cousin lean\n"
    assert read({"path": "Tablet/Cousin.tex"}) == "cousin prose\n"
    assert read({"path": "./Tablet/Cousin.lean"}) == "cousin lean\n"
    assert "theorem Rung" in read({"path": "Tablet/Rung.lean"})


def test_read_file_still_rejects_traversal_and_other_dirs(tmp_path: Path) -> None:
    """The path allowlist stays strict: Tablet/ and (when present)
    mathlib source only, no traversal, no absolute paths, no other
    extension, no nesting."""
    repo = _seed_repo(tmp_path)
    (repo / "APPROVED_AXIOMS.json").write_text("{}")
    (repo / "secrets.txt").write_text("no\n")
    (repo / "Tablet" / "sub").mkdir()
    (repo / "Tablet" / "sub" / "Nested.lean").write_text("nested\n")
    handlers = make_info_tool_handlers(repo, "Rung")
    read = handlers["read_file"]
    for path in (
        "APPROVED_AXIOMS.json",
        "secrets.txt",
        "../secrets",
        "Tablet/../APPROVED_AXIOMS.json",
        "Tablet/../../etc/passwd",
        "../../etc/passwd",
        "/etc/passwd",
        "~/.ssh/id_rsa",
        "Tablet/sub/Nested.lean",
        "Tablet/Rung.json",
        "Tablet",
        "",
        ".lake/packages/mathlib/Mathlib/Order/Basic.lean",
    ):
        assert "not readable" in read({"path": path}), path


def test_read_file_refuses_a_symlink_out_of_the_tablet(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    outside = tmp_path / "outside.lean"
    outside.write_text("secret\n")
    (repo / "Tablet" / "Escape.lean").symlink_to(outside)
    read = make_info_tool_handlers(repo, "Rung")["read_file"]
    assert "not readable" in read({"path": "Tablet/Escape.lean"})


def test_read_file_window_and_size_cap(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    body = "".join(f"line {n}\n" for n in range(1, 1001))
    (repo / "Tablet" / "Big.lean").write_text(body)
    read = make_info_tool_handlers(repo, "Rung")["read_file"]
    window = read({"path": "Tablet/Big.lean", "start_line": 500, "max_lines": 3})
    assert "lines 500-502 of 1000" in window
    assert "  500 | line 500" in window
    assert "line 503" not in window
    # The window length is capped even when the model asks for more.
    whole = read({"path": "Tablet/Big.lean", "start_line": 1, "max_lines": 100_000})
    assert f"lines 1-{READ_FILE_MAX_WINDOW_LINES} of 1000" in whole
    # Past the end reports the file length rather than an empty answer.
    assert "past the end" in read(
        {"path": "Tablet/Big.lean", "start_line": 5000, "max_lines": 5}
    )
    # Whole-file reads keep the byte cap.
    huge = "x" * (READ_FILE_MAX_BYTES + 4096) + "\n"
    (repo / "Tablet" / "Huge.tex").write_text(huge)
    out = read({"path": "Tablet/Huge.tex"})
    assert "[truncated" in out
    assert len(out) < READ_FILE_MAX_BYTES + 200


def test_search_tablet_finds_an_idiom_in_a_non_imported_sibling(
    tmp_path: Path,
) -> None:
    """The decisive case from the head-to-head, as a fixture: the answer
    exists ONLY in a sibling the node does not import."""
    repo = _seed_repo(tmp_path)
    (repo / "Tablet" / "LineGraphBasic.lean").write_text(
        "import Tablet.Preamble\n\n-- [TABLET NODE: LineGraphBasic]\n"
        "theorem LineGraphBasic : True := by\n-- BODY\n"
        "  let c := CompleteBipartiteGraph.bicoloring (Fin 3) (Fin 3)\n"
        "  trivial\n"
    )
    handlers = make_info_tool_handlers(repo, "Rung")
    assert "search_tablet" in handlers
    out = handlers["search_tablet"]({"query": "bicoloring"})
    assert "Tablet/LineGraphBasic.lean:6" in out
    assert "CompleteBipartiteGraph.bicoloring (Fin 3) (Fin 3)" in out
    # The hit line is marked and context lines carry their numbers.
    assert ">     6 |" in out
    assert "     5 |" in out
    # And the file it points at is then readable, closing the loop.
    assert "bicoloring" in handlers["read_file"](
        {"path": "Tablet/LineGraphBasic.lean"}
    )


def test_search_tablet_extension_case_and_no_hit(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    (repo / "Tablet" / "Rung.tex").write_text("the Perfect graph lemma\n")
    (repo / "Tablet" / "Note.lean").write_text("-- perfect graph tactic\n")
    search = make_info_tool_handlers(repo, "Rung")["search_tablet"]
    both = search({"query": "perfect"})
    assert "Tablet/Rung.tex" in both and "Tablet/Note.lean" in both
    lean_only = search({"query": "perfect", "extension": "lean"})
    assert "Tablet/Note.lean" in lean_only and "Tablet/Rung.tex" not in lean_only
    tex_only = search({"query": "perfect", "extension": "tex"})
    assert "Tablet/Rung.tex" in tex_only and "Tablet/Note.lean" not in tex_only
    # Smart case: an uppercase query is case-sensitive, so only the .tex
    # spelling matches; an explicit override widens it again.
    upper = search({"query": "Perfect"})
    assert "Tablet/Rung.tex" in upper and "Tablet/Note.lean" not in upper
    forced = search({"query": "Perfect", "case_sensitive": False})
    assert "Tablet/Note.lean" in forced
    assert "no match" in search({"query": "zzz_not_here"})
    # A pattern that does not parse as a regex falls back to a literal
    # match instead of erroring out.
    literal = search({"query": "graph ("})
    assert "no match" in literal and "matched literally" in literal


def test_search_tablet_caps_hits_and_truncates_long_lines(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    for index in range(30):
        (repo / "Tablet" / f"N{index}.lean").write_text(
            "".join(f"needle {n}\n" for n in range(10))
        )
    (repo / "Tablet" / "Long.lean").write_text("needle " + "y" * 4000 + "\n")
    search = make_info_tool_handlers(repo, "Rung")["search_tablet"]
    out = search({"query": "needle", "context": 0})
    header_count = sum(1 for line in out.splitlines() if line.startswith(">"))
    assert header_count <= SEARCH_TABLET_MAX_HITS
    assert "result cap reached" in out
    # Per-file cap keeps one crowded file from eating the budget.
    assert "more matches in Tablet/N0.lean" in out
    assert len(out) < SEARCH_RESULT_MAX_BYTES + 2000
    for line in out.splitlines():
        assert len(line) <= SEARCH_MAX_LINE_CHARS + 40
    # Context is clamped into range rather than rejected.
    wide = search({"query": "needle", "context": 999})
    assert wide  # no exception; the clamp is silent


def test_mathlib_tools_absent_without_a_checkout(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    handlers = make_info_tool_handlers(repo, "Rung")
    assert "search_mathlib_source" not in handlers
    assert mathlib_source_root(repo) is None
    specs = info_tool_specs(mathlib_source_enabled=False, goals_enabled=False)
    assert [s["function"]["name"] for s in specs] == [
        "read_file",
        "search_tablet",
        "search_mathlib",
    ]


def _seed_mathlib(repo: Path) -> Path:
    """A TINY fixture mathlib tree — the real one is never scanned by a
    unit test."""
    root = repo / ".lake" / "packages" / "mathlib" / "Mathlib"
    (root / "Combinatorics" / "SimpleGraph").mkdir(parents=True)
    (root / "Order").mkdir(parents=True)
    (root / "Combinatorics" / "SimpleGraph" / "Coloring.lean").write_text(
        "namespace SimpleGraph\n\n"
        "theorem chromaticNumber_le_card : True := by\n"
        "  trivial\n\n"
        "def bicoloring (V W : Type) : True := trivial\n"
    )
    (root / "Order" / "Basic.lean").write_text(
        "theorem le_of_eq_unrelated : True := trivial\n"
    )
    return root


def test_search_mathlib_source_is_bounded_and_readable(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    _seed_mathlib(repo)
    assert mathlib_source_root(repo) is not None
    handlers = make_info_tool_handlers(repo, "Rung")
    search = handlers["search_mathlib_source"]
    out = search({"query": "bicoloring"})
    assert ".lake/packages/mathlib/Mathlib/Combinatorics/SimpleGraph/" in out
    assert "Coloring.lean:6" in out
    # path_filter narrows the scan to a subtree.
    filtered = search({"query": "theorem", "path_filter": "Order"})
    assert "Order/Basic.lean" in filtered
    assert "Coloring.lean" not in filtered
    # A too-short query is refused before any scan (mathlib is ~8k files).
    assert "at least" in search({"query": "le"})
    assert "no match" in search({"query": "zzz_absent_zzz"})
    # The reported path is readable, and only under the package root.
    read = handlers["read_file"]
    path = ".lake/packages/mathlib/Mathlib/Combinatorics/SimpleGraph/Coloring.lean"
    assert "bicoloring" in read({"path": path})
    assert "not readable" in read({"path": ".lake/packages/mathlib/lakefile.lean"})
    assert "not readable" in read(
        {"path": ".lake/packages/mathlib/Mathlib/../../../secrets.txt"}
    )


def test_ripgrep_absent_is_a_tool_error_not_a_python_scan(
    tmp_path: Path, monkeypatch
) -> None:
    """U1: there is NO in-process regex fallback behind either grep tool.

    The retired fallback was the whole exploit: `_ripgrep_hits` returned
    None on ANY rg failure and the caller silently re-ran the query
    through Python's backtracking `re`, which the model could force at
    will (rg rejects lookaround). Both tools must now say ripgrep is
    missing rather than quietly matching in-process."""
    repo = _seed_repo(tmp_path)
    _seed_mathlib(repo)
    import shutil as shutil_mod

    monkeypatch.setattr(shutil_mod, "which", lambda name: None)
    handlers = make_info_tool_handlers(repo, "Rung")
    for tool, args in (
        ("search_mathlib_source", {"query": "bicoloring"}),
        ("search_tablet", {"query": "bicoloring"}),
    ):
        out = handlers[tool](args)
        assert out.startswith(f"{tool}: ")
        assert "ripgrep" in out and "no in-process fallback" in out
        # Emphatically NOT a result: no hit and no "no match" verdict,
        # because nothing was searched.
        assert "Coloring.lean" not in out
        assert "no match" not in out


def test_search_mathlib_source_call_cap_stops_a_grep_loop(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    _seed_mathlib(repo)
    search = make_info_tool_handlers(repo, "Rung")["search_mathlib_source"]
    for index in range(SEARCH_MATHLIB_MAX_CALLS):
        assert "used its" not in search({"query": f"theorem_{index}"})
    assert "used its" in search({"query": "theorem_over_the_cap"})


def test_repeat_cache_covers_the_new_search_tools(tmp_path: Path) -> None:
    """v2.1 hardening extends to v4: an identical re-query is answered
    from cache with an escalating warning, so a model cannot loop on
    greps."""
    repo = _seed_repo(tmp_path)
    _seed_mathlib(repo)
    (repo / "Tablet" / "Note.lean").write_text("-- bicoloring here\n")
    handlers = make_info_tool_handlers(repo, "Rung")
    for tool, args in (
        ("search_tablet", {"query": "bicoloring"}),
        ("search_mathlib_source", {"query": "bicoloring"}),
        ("read_file", {"path": "Tablet/Note.lean"}),
    ):
        first = handlers[tool](dict(args))
        second = handlers[tool](dict(args))
        third = handlers[tool](dict(args))
        assert not first.startswith("REPEAT CALL"), tool
        assert second.startswith("REPEAT CALL #2"), tool
        assert third.startswith("REPEAT CALL #3"), tool
        assert first.splitlines()[0] in second, tool
    # A DIFFERENT window of the same file is a different question.
    assert not handlers["read_file"](
        {"path": "Tablet/Note.lean", "start_line": 1, "max_lines": 1}
    ).startswith("REPEAT CALL")


def test_search_tool_results_are_never_ban_scanned(tmp_path: Path) -> None:
    """A sibling `.lean` body legitimately contains `sorry` or `admit`
    in a comment; search/read results are tool turns, so they flow back
    verbatim while a banned candidate BODY is still rejected."""
    repo = _seed_repo(tmp_path)
    (repo / "Tablet" / "Cousin.lean").write_text(
        "-- macro_rules and admit and sorry appear here\n"
    )
    calls: List[Dict] = []
    client = _client_with_replies(
        [
            _info_reply("search_tablet", {"query": "macro_rules"}),
            _tool_reply("  admit\n"),
            _tool_reply("  trivial\n"),
        ],
        calls,
    )
    compiles: List[str] = []

    def compile_body(body: str) -> CompileVerdict:
        compiles.append(body)
        return CompileVerdict(ok=True, log="")

    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=compile_body,
        info_tools=make_info_tool_handlers(repo, "Rung"),
        extra_tool_specs=info_tool_specs(),
    )
    assert result.status == "success"
    assert compiles == ["  trivial\n"]
    tool_contents = [
        m["content"]
        for call in calls
        for m in call["payload"]["messages"]
        if m.get("role") == "tool"
    ]
    assert any("macro_rules and admit and sorry appear here" in c for c in tool_contents)
    assert any("banned" in c for c in tool_contents)


def test_info_tool_counts_are_recorded_per_kind(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    (repo / "Tablet" / "Rung.tex").write_text("prose\n")
    calls: List[Dict] = []
    client = _client_with_replies(
        [
            _info_reply("read_file", {"path": "Tablet/Rung.tex"}),
            _info_reply("search_tablet", {"query": "theorem"}),
            _info_reply("search_tablet", {"query": "Preamble"}),
            _tool_reply("  trivial\n"),
        ],
        calls,
    )
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
        info_tools=make_info_tool_handlers(repo, "Rung"),
        extra_tool_specs=info_tool_specs(),
    )
    assert result.status == "success"
    assert result.info_tool_calls == 3
    assert result.info_tool_counts == {"read_file": 1, "search_tablet": 2}
    timings = attempt_timings(result)
    assert timings["info_tool_calls_by_kind"] == {"read_file": 1, "search_tablet": 2}


def test_v4_prompt_describes_the_widened_surface(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    prompt = build_system_prompt_v2(repo, "Rung", NODE_FILE)
    assert prompt.startswith(build_system_prompt(repo, "Rung", NODE_FILE))
    # Every advertised tool is described, in workflow order.
    for index, needle in enumerate(
        [
            "Tablet/Rung.tex",
            "search_tablet",
            "search_mathlib (Loogle)",
            "search_mathlib_source",
            "lean_run_code",
        ]
    ):
        assert needle in prompt, needle
    assert prompt.index("search_tablet") < prompt.index("search_mathlib_source")
    assert "whether or not this node imports it" in prompt
    assert "1. Read the paper's prose proof" in prompt
    assert "5. Submit candidate bodies" in prompt
    # Steps are renumbered contiguously when tools are absent.
    lean_prompt = build_system_prompt_v2(
        repo,
        "Rung",
        NODE_FILE,
        search_enabled=False,
        mathlib_source_enabled=False,
        goals_enabled=False,
    )
    assert "search_mathlib" not in lean_prompt
    assert "3. Submit candidate bodies" in lean_prompt


def test_info_tool_outputs_are_never_ban_scanned(tmp_path: Path) -> None:
    """Ban-scan interplay: tool RESULTS are tool turns, not candidate
    bodies — a tex full of banned tokens must flow back verbatim, while
    a banned candidate BODY on a later turn is still rejected."""
    repo = _seed_repo(tmp_path)
    (repo / "Tablet" / "Rung.tex").write_text(
        "the axiom of choice; sorry; macro_rules galore\n"
    )
    calls: List[Dict] = []
    client = _client_with_replies(
        [
            _info_reply("read_file", {"path": "Tablet/Rung.tex"}),
            _tool_reply("  admit\n"),
            _tool_reply("  trivial\n"),
        ],
        calls,
    )
    compiles: List[str] = []

    def compile_body(body: str) -> CompileVerdict:
        compiles.append(body)
        return CompileVerdict(ok=True, log="")

    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=compile_body,
        info_tools=make_info_tool_handlers(repo, "Rung"),
        extra_tool_specs=info_tool_specs(),
    )
    assert result.status == "success"
    assert compiles == ["  trivial\n"], "banned BODY still never compiles"
    tool_contents = [
        m["content"]
        for call in calls
        for m in call["payload"]["messages"]
        if m.get("role") == "tool"
    ]
    assert any("macro_rules galore" in c for c in tool_contents), (
        "tool output must pass through unscanned"
    )
    assert any("banned" in c for c in tool_contents), (
        "the banned body must still be rejected"
    )


def test_repeat_info_calls_answered_from_cache_with_warning(tmp_path: Path) -> None:
    """v2.1: identical info-tool calls come from a per-attempt cache
    with an escalating REPEAT CALL warning (observed live: the same 3
    dead-end queries 47-48x each, each a full API turn)."""
    repo = _seed_repo(tmp_path)
    reads = {"n": 0}
    tex = repo / "Tablet" / "Rung.tex"
    tex.write_text("prose\n")
    real_read_text = Path.read_text

    def counting_read_text(self, *args, **kwargs):
        if self == tex:
            reads["n"] += 1
        return real_read_text(self, *args, **kwargs)

    handlers = make_info_tool_handlers(repo, "Rung")
    import unittest.mock

    with unittest.mock.patch.object(Path, "read_text", counting_read_text):
        first = handlers["read_file"]({"path": "Tablet/Rung.tex"})
        second = handlers["read_file"]({"path": "Tablet/Rung.tex"})
        third = handlers["read_file"]({"path": "Tablet/Rung.tex"})
    assert first == "prose\n"
    assert second.startswith("REPEAT CALL #2")
    assert third.startswith("REPEAT CALL #3")
    assert "prose" in second and "prose" in third
    assert reads["n"] == 1, "the underlying read runs once"


def test_search_mathlib_unknown_identifier_auto_retries_quoted(
    tmp_path: Path, monkeypatch
) -> None:
    """v2.1: loogle's `unknown identifier` dead end auto-retries as a
    quoted name-substring search; residual errors carry a syntax
    primer."""
    repo = _seed_repo(tmp_path)
    queries: List[str] = []

    class FakeResp:
        def __init__(self, payload: Dict[str, Any]) -> None:
            self._payload = json.dumps(payload).encode()

        def read(self):
            return self._payload

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

    def fake_urlopen(url, timeout=None):
        import urllib.parse

        q = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)["q"][0]
        queries.append(q)
        if q == "isClosedMap":
            return FakeResp({"error": "unknown identifier 'isClosedMap'"})
        if q == '"isClosedMap"':
            return FakeResp(
                {
                    "hits": [
                        {
                            "name": "IsClosedMap",
                            "type": "(f : α → β) → Prop",
                            "module": "Mathlib.Topology.Defs.Basic",
                        }
                    ]
                }
            )
        return FakeResp({"error": "parse error"})

    import urllib.request

    monkeypatch.setattr(urllib.request, "urlopen", fake_urlopen)
    handlers = make_info_tool_handlers(repo, "Rung")
    out = handlers["search_mathlib"]({"query": "isClosedMap"})
    assert queries == ["isClosedMap", '"isClosedMap"']
    assert "IsClosedMap : (f : α → β) → Prop" in out
    assert "no exact identifier" in out
    # Residual error => one-line syntax primer, no retry loop.
    out2 = handlers["search_mathlib"]({"query": "((("})
    assert "Query syntax" in out2


def test_search_mathlib_degrades_gracefully_when_loogle_down(
    tmp_path: Path, monkeypatch
) -> None:
    repo = _seed_repo(tmp_path)

    def refuse(url, timeout=None):
        raise ConnectionRefusedError("no loogle here")

    import urllib.request

    monkeypatch.setattr(urllib.request, "urlopen", refuse)
    handlers = make_info_tool_handlers(repo, "Rung")
    out = handlers["search_mathlib"]({"query": "Nat.succ"})
    assert "loogle unavailable" in out


def test_search_tool_absent_when_loogle_not_configured(tmp_path: Path) -> None:
    """Config-off => the tool does not exist: no handler, no spec, no
    prompt guidance for it."""
    repo = _seed_repo(tmp_path)
    handlers = make_info_tool_handlers(repo, "Rung", search_enabled=False)
    assert "search_mathlib" not in handlers
    assert "read_file" in handlers
    specs = info_tool_specs(
        search_enabled=False, goals_enabled=False, mathlib_source_enabled=False
    )
    assert specs == [READ_FILE_TOOL, SEARCH_TABLET_TOOL]
    assert info_tool_specs(goals_enabled=False, mathlib_source_enabled=False) == [
        READ_FILE_TOOL,
        SEARCH_TABLET_TOOL,
        SEARCH_MATHLIB_TOOL,
    ]
    # v3 get_goals is advertised by default (last spec).
    assert info_tool_specs()[-1]["function"]["name"] == "get_goals"
    prompt = build_system_prompt_v2(
        repo,
        "Rung",
        NODE_FILE,
        search_enabled=False,
        goals_enabled=False,
        mathlib_source_enabled=False,
    )
    assert "read_file" in prompt
    assert "search_mathlib" not in prompt


def test_v2_prompt_directs_reading_the_tex_first(tmp_path: Path) -> None:
    repo = _seed_repo(tmp_path)
    prompt = build_system_prompt_v2(repo, "Rung", NODE_FILE)
    assert prompt.startswith(build_system_prompt(repo, "Rung", NODE_FILE))
    assert "Tablet/Rung.tex" in prompt
    assert "read_file" in prompt and "search_mathlib" in prompt


def test_mixed_lean_and_info_calls_all_answered_per_id() -> None:
    """Protocol: one result per tool_call_id, even when one assistant
    message mixes lean_run_code with an info call."""
    mixed = {
        "usage": {"prompt_tokens": 10, "completion_tokens": 5},
        "choices": [
            {
                "message": {
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "lean-1",
                            "function": {
                                "name": "lean_run_code",
                                "arguments": json.dumps({"body": "  exact no\n"}),
                            },
                        },
                        {
                            "id": "extra-1",
                            "function": {
                                "name": "mystery_tool",
                                "arguments": "{}",
                            },
                        },
                    ],
                }
            }
        ],
    }
    calls: List[Dict] = []
    client = _client_with_replies([mixed, _tool_reply("  trivial\n")], calls)
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=body == "  trivial\n", log="no"),
        info_tools={},
        extra_tool_specs=[],
    )
    assert result.status == "success"
    answered_ids = {
        m.get("tool_call_id")
        for call in calls
        for m in call["payload"]["messages"]
        if m.get("role") == "tool"
    }
    assert {"lean-1", "extra-1"} <= answered_ids


def _multi_call_reply(specs: List[tuple]) -> Dict[str, Any]:
    """Assistant message carrying several tool calls: (id, name, args)."""
    return {
        "usage": {"prompt_tokens": 10, "completion_tokens": 5},
        "choices": [
            {
                "message": {
                    "content": "",
                    "tool_calls": [
                        {
                            "id": cid,
                            "function": {"name": name, "arguments": json.dumps(args)},
                        }
                        for cid, name, args in specs
                    ],
                }
            }
        ],
    }


def _tool_results(calls: List[Dict]) -> Dict[str, str]:
    """id -> content over every tool message the driver ever sent."""
    out: Dict[str, str] = {}
    for call in calls:
        for m in call["payload"]["messages"]:
            if m.get("role") == "tool":
                out[m.get("tool_call_id")] = m.get("content", "")
    return out


def test_two_lean_calls_in_one_message_are_both_answered() -> None:
    """Combined-audit B3 root cause: the echo re-sends ALL tool_calls
    while only the FIRST lean_run_code was answered, so a two-compile
    message left an unanswered tool_call_id -> endpoint 400 -> the
    attempt died transport-class and respawned on the same prompt
    forever. Every id gets a result; only the first body is compiled."""
    reply = _multi_call_reply(
        [
            ("lean-1", "lean_run_code", {"body": "  exact wrong\n"}),
            ("lean-2", "lean_run_code", {"body": "  exact alsowrong\n"}),
        ]
    )
    calls: List[Dict] = []
    client = _client_with_replies([reply, _tool_reply("  trivial\n")], calls)
    compiled: List[str] = []

    def compile_body(body: str) -> CompileVerdict:
        compiled.append(body)
        return CompileVerdict(ok=body == "  trivial\n", log="error: wrong")

    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=compile_body,
    )
    assert result.status == "success"
    answered = _tool_results(calls)
    assert {"lean-1", "lean-2"} <= set(answered)
    assert "error: wrong" in answered["lean-1"]
    assert "one lean_run_code call per turn" in answered["lean-2"]
    # The second body is never compiled — one candidate per turn.
    assert "  exact alsowrong\n" not in compiled


def test_repeated_info_calls_in_one_message_are_each_answered() -> None:
    """Same protocol rule for the info tools the model can multi-call:
    one result per tool_call_id, never one for the batch."""
    reply = _multi_call_reply(
        [
            ("read-1", "read_file", {"path": "Tablet/A.tex"}),
            ("read-2", "read_file", {"path": "Tablet/B.tex"}),
            ("goals-1", "get_goals", {"line": 2}),
        ]
    )
    calls: List[Dict] = []
    client = _client_with_replies([reply, _tool_reply("  trivial\n")], calls)
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
        info_tools={
            "read_file": lambda args: f"contents of {args.get('path')}",
            "get_goals": lambda args: "⊢ True",
        },
        extra_tool_specs=[READ_FILE_TOOL],
    )
    assert result.status == "success"
    answered = _tool_results(calls)
    assert answered["read-1"] == "contents of Tablet/A.tex"
    assert answered["read-2"] == "contents of Tablet/B.tex"
    assert answered["goals-1"] == "⊢ True"
    assert result.info_tool_calls == 3


# ---------------------------------------------------------------------------
# Transport: backoff + key hygiene (A9)
# ---------------------------------------------------------------------------


def test_backoff_on_429_honors_retry_after() -> None:
    sleeps: List[float] = []
    attempts = {"n": 0}

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        attempts["n"] += 1
        if attempts["n"] < 3:
            return FakeResponse(429, {}, headers={"Retry-After": "7"})
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=sleeps.append)
    reply = client.chat([{"role": "user", "content": "x"}])
    assert reply["choices"]
    assert sleeps == [7.0, 7.0]


def test_retry_after_is_clamped() -> None:
    """F4: a bogus Retry-After (86400) must not wedge the single-flight
    daemon — clamp to RETRY_AFTER_CAP_SECONDS."""
    sleeps: List[float] = []
    attempts = {"n": 0}

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        attempts["n"] += 1
        if attempts["n"] == 1:
            return FakeResponse(429, {}, headers={"Retry-After": "86400"})
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=sleeps.append)
    reply = client.chat([{"role": "user", "content": "x"}])
    assert reply["choices"]
    assert sleeps == [300.0]


def test_read_timeout_derived_from_wall_and_post_return_wall_check() -> None:
    """F4: the request's read timeout comes from the remaining wall
    budget (+ grace), and a request that straddles the wall deadline
    terminates the attempt promptly — no compile, no further turns."""
    clock = {"t": 0.0}

    def now() -> float:
        return clock["t"]

    timeouts: List[float] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        timeouts.append(timeout)
        clock["t"] += 700.0  # the request straddles the 100 s wall
        return FakeResponse(200, _tool_reply("  trivial\n"))

    config = _cfg(attempt_wall_seconds=100.0)
    client = ModelClient(config, "sk-k", post=post, sleep=lambda s: None)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: pytest.fail("must not compile past the wall"),
        now=now,
    )
    assert result.status == "budget_exhausted"
    assert result.detail == "wall budget"
    assert timeouts == [pytest.approx(130.0)], (
        "read timeout must be remaining wall (100) + grace (30), not 600"
    )


def test_chat_read_timeout_caps_at_600() -> None:
    timeouts: List[float] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        timeouts.append(timeout)
        return FakeResponse(200, _tool_reply("  trivial\n"))

    # Frozen clock: each request's timeout is derived from the budget
    # left, so a real clock ticking between the two reads would put the
    # cap a microsecond under 600 and blur what is pinned here.
    client = ModelClient(
        _cfg(), "sk-k", post=post, sleep=lambda s: None, now=lambda: 0.0
    )
    client.chat([{"role": "user", "content": "x"}])  # default
    client.chat([{"role": "user", "content": "x"}], read_timeout=100_000.0)
    client.chat([{"role": "user", "content": "x"}], read_timeout=-5.0)
    assert timeouts == [600.0, 600.0, 1.0]


def test_read_timeout_floor_holds_when_the_budget_left_is_sub_second() -> None:
    """The 1 s floor under the re-derived timeout is load-bearing.

    The timeout is `max(1.0, min(600, deadline - now))` and only the
    ceiling was pinned. Both retry ladders gate on the BACKOFF fitting
    before the deadline, which leaves an arbitrarily small remainder: a
    30 s backoff admitted against 30.6 s of budget resumes with 0.6 s
    left. The floor turns that into a 1 s request, which is what makes
    the true overshoot bound wall + grace + (floor - epsilon)."""
    clock = {"t": 0.0}
    timeouts: List[float] = []
    posts = {"n": 0}
    stall = _fake_read_timeout()

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        timeouts.append(timeout)
        if posts["n"] == 1:
            raise stall("stalled")
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=sleep, now=now)
    client.chat([{"role": "user", "content": "x"}], read_timeout=30.6)
    assert timeouts == [pytest.approx(30.6), 1.0], (
        "0.6 s of budget was left after the admitted backoff; the floor "
        "raises the request's timeout to 1 s"
    )


def test_read_timeout_floor_holds_when_the_budget_is_already_spent() -> None:
    """And it holds where the remainder is NEGATIVE.

    The `reasoning_effort` fallback is an immediate retry — no backoff,
    so nothing to deadline-gate — and the 4xx that triggers it can arrive
    long after the deadline. Without the floor the retry is handed a
    negative timeout, and urllib3 2.0.7 raises `ValueError` for anything
    <= 0 ("the timeout cannot be set to a value less than or equal to
    0"). That surfaces as `model request failed: ValueError` and buries
    the real story under a fake one."""
    clock = {"t": 0.0}
    timeouts: List[float] = []
    posts = {"n": 0}

    def now() -> float:
        return clock["t"]

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        timeouts.append(timeout)
        if posts["n"] == 1:
            clock["t"] += 700.0  # 600 s past a 100 s deadline
            return FakeResponse(
                422, {"message": "Extra inputs are not permitted: reasoning_effort"}
            )
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(
        _cfg(reasoning_effort="high"),
        "sk-k",
        post=post,
        sleep=lambda s: None,
        now=now,
    )
    client.chat([{"role": "user", "content": "x"}], read_timeout=100.0)
    assert timeouts == [pytest.approx(100.0), 1.0]
    assert all(t > 0 for t in timeouts), "urllib3 rejects a timeout <= 0"


def test_the_effort_fallback_chains_a_second_floored_request() -> None:
    """Two floors, not one, is the real overshoot bound.

    The two floored paths compose: a gated backoff resumes with a
    sub-second remainder (floor #1), and if THAT request answers a 4xx
    indicting `reasoning_effort` inside its floor, the fallback retry —
    immediate, with no backoff to gate — starts past the deadline and is
    floored in its turn (floor #2). So a call bounded by `deadline` can
    end at `deadline + 2 * floor - epsilon`, and the sweep families,
    which never reach a 4xx of that shape, do not cover it."""
    clock = {"t": 0.0}
    timeouts: List[float] = []
    posts = {"n": 0}
    stall = _fake_read_timeout()

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        timeouts.append(timeout)
        if posts["n"] == 1:
            clock["t"] += 69.5  # a stall with room for exactly one backoff
            raise stall("stalled")
        if posts["n"] == 2:
            clock["t"] += 0.68  # the 422 arrives inside the 1 s floor
            return FakeResponse(
                422, {"message": "Extra inputs are not permitted: reasoning_effort"}
            )
        clock["t"] += float(timeout)
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(
        _cfg(reasoning_effort="high"), "sk-k", post=post, sleep=sleep, now=now
    )
    client.chat([{"role": "user", "content": "x"}], read_timeout=100.0)
    assert timeouts == [pytest.approx(100.0), 1.0, 1.0], (
        "the backoff left 0.5 s and the fallback none at all — both floored"
    )
    assert 101.0 < clock["t"] < 102.0, (
        f"the call ended at {clock['t']:.2f} s on a 100 s deadline: past one "
        "floor, under two"
    )


def test_transport_exhaustion_is_an_error_result() -> None:
    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        return FakeResponse(500, {})

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=lambda s: None)
    with pytest.raises(ModelTransportError):
        client.chat([{"role": "user", "content": "x"}])
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "error"


def test_transport_stall_retries_same_request_and_attempt_survives() -> None:
    """Grunt-bench §3.3: a >600 s free-tier stall surfaced as
    ReadTimeout and aborted the whole attempt, discarding ~1.3M tokens
    of context. Bounded in-attempt retry: the SAME request retries up
    to 2 times (fresh connections, backoff 30/60 s) and the attempt
    stays alive."""

    class FakeReadTimeout(Exception):
        pass

    FakeReadTimeout.__name__ = "ReadTimeout"

    sleeps: List[float] = []
    posts: List[Dict[str, Any]] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts.append({"payload": json})
        if len(posts) <= 2:
            raise FakeReadTimeout("stalled mid-response")
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=sleeps.append)
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "success", "the stall must not kill the attempt"
    assert sleeps == [30.0, 60.0]
    # The SAME request every time (conversation intact).
    assert posts[0]["payload"] == posts[1]["payload"] == posts[2]["payload"]


def test_transport_retry_exhaustion_classifies_transport() -> None:
    """After 2 retries the exception surfaces and the attempt ends
    `error` — the F1 transport class (suspension-counting, never the
    attempted-set)."""

    class FakeReadTimeout(Exception):
        pass

    FakeReadTimeout.__name__ = "ReadTimeout"

    sleeps: List[float] = []
    posts = {"n": 0}

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        raise FakeReadTimeout("stalled")

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=sleeps.append)
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "error"
    assert "ReadTimeout" in result.detail
    assert posts["n"] == 3, "original + exactly 2 retries"
    assert sleeps == [30.0, 60.0]


def _fake_read_timeout() -> type:
    class FakeReadTimeout(Exception):
        pass

    FakeReadTimeout.__name__ = "ReadTimeout"
    return FakeReadTimeout


def test_transport_timeout_at_the_wall_is_budget_exhausted() -> None:
    """F6: the read timeout is derived from the REMAINING wall, so an
    attempt that runs its budget out mid-request surfaces as ReadTimeout
    at wall+grace. That is the wall, not a transport fault — misfiling it
    `error` publishes no outcome, leaves the generation unspent, and the
    node is re-attempted from scratch (9 nodes, 2 slots each, live run)."""
    clock = {"t": 0.0}

    def now() -> float:
        return clock["t"]

    stall = _fake_read_timeout()

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        clock["t"] += 130.0  # the 100 s wall + the 30 s grace, exactly
        raise stall("stalled mid-response")

    config = _cfg(attempt_wall_seconds=100.0)
    client = ModelClient(config, "sk-k", post=post, sleep=lambda s: None, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: pytest.fail("must not compile past the wall"),
        now=now,
    )
    assert result.status == "budget_exhausted"
    assert "wall budget" in result.detail
    assert "ReadTimeout" in result.detail, "the transport cause stays diagnosable"


def test_transport_error_below_the_wall_stays_error() -> None:
    """The reclassification is by the clock alone: a transport failure
    with wall budget left over is still transport-class."""
    clock = {"t": 0.0}

    def now() -> float:
        return clock["t"]

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        clock["t"] += 10.0
        raise ValueError("malformed payload")

    config = _cfg(attempt_wall_seconds=100.0)
    client = ModelClient(config, "sk-k", post=post, sleep=lambda s: None, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
        now=now,
    )
    assert result.status == "error"
    assert result.detail == "model request failed: ValueError"


def _compaction_then_transport_failure(raiser, wall: float) -> AttemptResult:
    """First turn: a failing body big enough to trip compaction. Second
    turn (the compaction turn itself): a transport failure."""
    clock = {"t": 0.0}
    posts = {"n": 0}

    def now() -> float:
        return clock["t"]

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        if posts["n"] == 1:
            clock["t"] += 10.0
            return FakeResponse(200, _tool_reply("  exact nope\n", tokens=(120, 30)))
        clock["t"] += 200.0
        raise raiser("compaction turn broke")

    config = _cfg(
        attempt_wall_seconds=wall,
        compact_context_tokens=50,
        max_iterations=0,
        attempt_tokens=0,
    )
    client = ModelClient(config, "sk-k", post=post, sleep=lambda s: None, now=now)
    return run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="bad"),
        now=now,
    )


def test_compaction_transport_timeout_at_the_wall_is_budget_exhausted() -> None:
    result = _compaction_then_transport_failure(_fake_read_timeout(), wall=100.0)
    assert result.status == "budget_exhausted"
    assert result.detail.startswith("compaction turn: "), "the site stays named"
    assert "wall budget" in result.detail
    assert "ReadTimeout" in result.detail


def test_compaction_transport_error_below_the_wall_stays_error() -> None:
    result = _compaction_then_transport_failure(ValueError, wall=100_000.0)
    assert result.status == "error"
    assert result.detail == "compaction turn: model request failed: ValueError"


def test_compaction_turn_retried_outage_stays_error() -> None:
    """The compaction turn carries the same guard: two of the eight live
    misfilings came through this site, so it must not be the hole."""
    clock = {"t": 0.0}
    posts = {"n": 0}
    stall = _fake_read_timeout()

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        if posts["n"] == 1:
            clock["t"] += 4000.0
            return FakeResponse(200, _tool_reply("  nope\n", tokens=(120, 30)))
        clock["t"] += float(timeout)
        raise stall("black hole")

    config = _cfg(
        attempt_wall_seconds=5400.0,
        compact_context_tokens=50,
        max_iterations=0,
        attempt_tokens=0,
    )
    client = ModelClient(config, "sk-k", post=post, sleep=sleep, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="bad"),
        now=now,
    )
    assert client.transport_retries == 2
    assert clock["t"] == 5430.0, "the retries were squeezed inside wall + grace"
    assert result.status == "error"
    assert result.detail == "compaction turn: model request failed: ReadTimeout"


def _black_hole_attempt(
    *, wall: float, entry: float
) -> Tuple[AttemptResult, float, int]:
    """Drive the real loop against an endpoint that stops answering.

    The first turn consumes exactly ``entry`` seconds; every request
    after that vanishes into a black hole and ends only when its own
    socket timeout expires. Returns (result, attempt end time, retries).
    """
    clock = {"t": 0.0}
    posts = {"n": 0}
    stall = _fake_read_timeout()

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        if posts["n"] == 1:
            clock["t"] += entry
            return FakeResponse(200, _tool_reply("  nope\n"))
        # A black hole answers nothing: the request runs the FULL socket
        # timeout it was handed, whatever the clock says by then.
        clock["t"] += float(timeout)
        raise stall("black hole")

    config = _cfg(attempt_wall_seconds=wall, max_iterations=0, attempt_tokens=0)
    client = ModelClient(config, "sk-k", post=post, sleep=sleep, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="bad"),
        now=now,
    )
    return result, clock["t"], client.transport_retries


def _http_storm_attempt(
    *,
    wall: float,
    entry: float,
    status: int = 429,
    retry_after: Optional[str] = "86400",
    request_secs: float = 1.0,
) -> Tuple[AttemptResult, float, int]:
    """Drive the real loop against an endpoint that RATE-LIMITS forever.

    The first turn consumes ``entry`` seconds and answers; every request
    after that comes back ``status`` (with ``Retry-After`` when given)
    after ``request_secs``. Unlike the black hole this endpoint ANSWERS —
    the socket timeout never fires, so the whole cost is the backoff
    ladder. Returns (result, attempt end time, transport retries).
    """
    clock = {"t": 0.0}
    posts = {"n": 0}

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        if posts["n"] == 1:
            clock["t"] += entry
            return FakeResponse(200, _tool_reply("  nope\n"))
        clock["t"] += request_secs
        headers_out = {} if retry_after is None else {"Retry-After": retry_after}
        return FakeResponse(status, {}, headers=headers_out)

    config = _cfg(attempt_wall_seconds=wall, max_iterations=0, attempt_tokens=0)
    client = ModelClient(config, "sk-k", post=post, sleep=sleep, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="bad"),
        now=now,
    )
    return result, clock["t"], client.transport_retries


def _storm_then_stall_attempt(
    *,
    wall: float,
    entry: float,
    storms: int = 3,
    retry_after: Optional[str] = "86400",
) -> Tuple[AttemptResult, float, int]:
    """Drive the real loop against a provider that rate-limits and THEN
    goes dark.

    The first turn consumes ``entry`` seconds and answers; the next
    ``storms`` requests come back 429 instantly; every request after that
    vanishes into a black hole. This is the shape `http_status` cannot
    see — the call ENDS on the stall, so the last status is None, and the
    storm is remembered only by the count of failing replies. Returns
    (result, attempt end time, transport retries).
    """
    clock = {"t": 0.0}
    posts = {"n": 0}
    stall = _fake_read_timeout()

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        if posts["n"] == 1:
            clock["t"] += entry
            return FakeResponse(200, _tool_reply("  nope\n"))
        if posts["n"] <= 1 + storms:
            headers_out = {} if retry_after is None else {"Retry-After": retry_after}
            return FakeResponse(429, {}, headers=headers_out)
        clock["t"] += float(timeout)
        raise stall("black hole")

    config = _cfg(attempt_wall_seconds=wall, max_iterations=0, attempt_tokens=0)
    client = ModelClient(config, "sk-k", post=post, sleep=sleep, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="bad"),
        now=now,
    )
    return result, clock["t"], client.transport_retries


def test_http_retry_backoff_is_wall_budget_aware() -> None:
    """The 429/5xx ladder is deadline-gated like the transport one.

    It was not, and it was the only retry path that could outlive the
    deadline `chat` derives on entry: admission asked nothing, so three
    clamped `Retry-After` sleeps of 300 s ran wherever the storm began.
    A retry whose backoff would not fit is now declined and the error
    surfaces instead — the sleep never happens."""
    sleeps: List[float] = []
    clock = {"t": 0.0}
    posts = {"n": 0}

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        sleeps.append(secs)
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        return FakeResponse(429, {}, headers={"Retry-After": "86400"})

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=sleep, now=now)
    # 100 s of budget against a Retry-After clamped to 300 s: no room.
    with pytest.raises(ModelTransportError) as excinfo:
        client.chat([{"role": "user", "content": "x"}], read_timeout=100.0)
    assert posts["n"] == 1, "the declined retry was never issued"
    assert sleeps == [], "and never slept"
    assert "no budget left to retry" in str(excinfo.value)
    assert clock["t"] == 0.0

    # 400 s of budget: the first 300 s backoff fits, the second does not.
    posts["n"] = 0
    clock["t"] = 0.0
    sleeps.clear()
    with pytest.raises(ModelTransportError):
        client.chat([{"role": "user", "content": "x"}], read_timeout=400.0)
    assert posts["n"] == 2
    assert sleeps == [300.0]


def test_rate_limit_storm_past_the_wall_is_a_fault_not_the_wall() -> None:
    """A 429 is the provider naming a fault, never the wall.

    The retry-delta guard reads the TRANSPORT counter, which a rate-limit
    storm never touches, so a storm at the wall used to satisfy `not
    retried` and land `budget_exhausted`: an outcome published, the
    node's generation spent, and — through the non-transport branch of
    `_bookkeep_outcome` — `consecutive_transport_failures` and
    `error_streaks` RESET. That is the same disarming `d2d09280` closed,
    reached through the HTTP door. The `http_status` on the exception is
    the discriminator: the endpoint ANSWERED, so that request ended on a
    reply rather than on the wall-derived timeout."""
    result, end, retries = _http_storm_attempt(wall=5400.0, entry=5400.0)
    assert end > 5400.0, "the failure landed past the wall, like a clean run"
    assert retries == 0, (
        "and the transport counter never moved — the HTTP ladder is not a "
        "transport stall, and must not be reported to the operator as one"
    )
    assert result.status == "error"
    assert result.detail == "model request has no budget left to retry (last HTTP 429)"


def test_rejected_4xx_past_the_wall_is_a_fault_not_the_wall() -> None:
    """The same rule for a non-retryable 4xx, which never retries at all.

    A context-overflow 400 arriving after the wall was read as budget
    exhaustion — an outcome published and a generation spent on the one
    failure the operator can actually act on, whose body is already
    carried for exactly that purpose. Below the wall the identical 400 is
    an `error`; the endpoint answering is what settles it, not when."""
    clock = {"t": 0.0}

    def now() -> float:
        return clock["t"]

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        clock["t"] += 5401.0
        return FakeResponse(400, {"message": "input too long: 141000 tokens"})

    config = _cfg(attempt_wall_seconds=5400.0, max_iterations=0, attempt_tokens=0)
    client = ModelClient(config, "sk-k", post=post, sleep=lambda s: None, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="bad"),
        now=now,
    )
    assert result.status == "error"
    assert result.detail.startswith("model request rejected: HTTP 400: ")
    assert "input too long" in result.detail


def test_retried_outage_past_the_wall_stays_error() -> None:
    """The clock test alone misfiles a real outage as the wall.

    A black-holed endpoint now lands its last retry EXACTLY on
    wall + grace — bit-identical, on the clock, to an attempt that
    simply ran out of budget. Classifying that `budget_exhausted` takes
    the non-transport branch of `_bookkeep_outcome`, which RESETS
    `consecutive_transport_failures` and `error_streaks` — so it disarms
    the circuit breaker rather than merely failing to feed it. The
    retry delta is the whole discriminator: a clean wall exhaustion
    never retries, and a retry on the failing call is positive evidence
    of a fault."""
    result, end, retries = _black_hole_attempt(wall=5400.0, entry=4000.0)
    assert retries == 2, "the outage was retried — this is not a clean wall run"
    assert end == 5430.0, "and it landed ON the wall + grace, like a clean run"
    assert result.status == "error"
    assert result.detail == "model request failed: ReadTimeout"


def test_transport_backoff_is_never_shorter_than_the_wall_grace() -> None:
    """The inequality the retry-delta discriminator rests on.

    A transport retry is admitted only when `now + backoff < deadline`,
    and `deadline` is wall + grace. With the shortest backoff at least as
    long as the grace, every admitted retry follows a failure strictly
    BEFORE the wall — so an attempt that simply ran out of budget, whose
    first failure is its wall-derived timeout firing AT wall + grace, can
    never have retried, and the delta never flips it to `error`.

    Lower a backoff below the grace (or raise the grace above the
    shortest backoff) and a stall arriving between the wall and the
    deadline becomes retryable: a clean wall exhaustion then surfaces
    with a positive delta and is filed as a transport fault, silently,
    with every other test still passing. That is exactly the state the
    HTTP ladder is in — `BACKOFF_START_SECONDS` is 15 — which is why the
    failing-reply rule beside the delta is load-bearing rather than
    tidy."""
    # The witness: an outage whose 600 s socket timeout fires INSIDE the
    # grace band (entry 4810 -> failure at 5410, past a 5400 s wall). The
    # budget was already spent when it surfaced, so the retry must be
    # declined and the attempt read as the wall. At a 15 s backoff it is
    # admitted instead and the whole band 4800.1-4814.9 files `error`.
    result, end, retries = _black_hole_attempt(wall=5400.0, entry=4810.0)
    assert retries == 0
    assert 5400.0 < end < 5430.0, "the failure landed inside the grace band"
    assert result.status == "budget_exhausted"
    assert min(ModelClient.TRANSPORT_BACKOFF_SECONDS) >= WALL_TIMEOUT_GRACE_SECONDS


def test_no_attempt_overshoots_its_wall_by_more_than_the_grace() -> None:
    """The grace is the WHOLE overshoot budget, at every entry point.

    `chat` used to fix each request's socket timeout at the value derived
    on ENTRY and re-consult the deadline only to ADMIT retries, so a
    retry taken at deadline-epsilon then ran the full entry timeout: a
    black hole starting 4139 s into a 5400 s wall burned 3 x 600 s plus
    30+60 s of backoff and ended at 6029 s — 629 s past the wall against
    a documented grace of 30. Deriving the timeout from the budget LEFT
    on every pass bounds the whole call by its deadline, whatever the
    retry pattern.

    "Every" means BOTH ladders. Sweeping only the stalling endpoint left
    the rate-limiting one — which answers, so its cost is pure backoff —
    running 3 clamped Retry-Afters of 300 s past an ungated deadline, out
    to 6303 s. The bound over these two families is wall + grace +
    (floor - epsilon), not wall + grace exactly: a gated backoff can
    resume with a sub-second remainder, which the 1 s timeout floor then
    outlasts. The entries are deliberately off the 10 s grid as well —
    on it a black hole lands EXACTLY on wall + grace, and a tighter
    assertion would be reading the grid rather than the code (entry 4139
    ends at 5430.0, entry 4139.5 at 5430.5).

    The effort fallback chains a SECOND floored request onto the first,
    for a global bound of wall + grace + (2 * floor - epsilon); that path
    needs a 4xx that indicts the field, so it is not one of these two
    endpoints and is pinned separately in
    `test_the_effort_fallback_chains_a_second_floored_request`."""
    wall = 5400.0
    floor = 1.0  # the `max(1.0, ...)` under every request's socket timeout
    ceiling = wall + WALL_TIMEOUT_GRACE_SECONDS
    # Every phase of both retry ladders: the sweep covers entries with
    # room for 2 retries, 1, and none at all.
    entries = [
        float(e)
        for e in list(range(0, 5400, 10)) + [4139, 4140, 5399, 5400]
    ] + [4139.5, 4139.99, 5399.5]
    black_hole_worst = max(_black_hole_attempt(wall=wall, entry=e)[1] for e in entries)
    storm_worst = max(
        _http_storm_attempt(wall=wall, entry=e, status=status, retry_after=header)[1]
        for e in entries
        # A clamped Retry-After, an honored one, and the exponential
        # fallback when the provider sends no header at all.
        for status, header in ((429, "86400"), (429, "7"), (503, None))
    )
    worst = max(black_hole_worst, storm_worst)
    assert worst < ceiling + floor, (
        f"an attempt ran to {worst:.1f} s on a {wall:.0f} s wall — the "
        f"grace allows {ceiling:.0f} s (plus under the 1 s timeout floor)"
    )
    assert black_hole_worst >= ceiling, (
        "and the bound is attained: a black hole runs its last request "
        "right up to wall + grace"
    )


def test_no_rate_limit_storm_anywhere_is_ever_filed_as_the_wall() -> None:
    """The complement of the overshoot sweep, on the classification.

    A storm beginning at ANY point of the attempt is a fault. Before the
    fix the last ~900 s of the wall was a band where it read
    `budget_exhausted` instead — the band widened with the clamped
    backoff, so a provider sending `Retry-After: 86400` chose how much of
    the attempt disarmed the circuit breaker."""
    wall = 5400.0
    statuses = {
        _http_storm_attempt(
            wall=wall, entry=float(entry), status=status, retry_after=header
        )[0].status
        for entry in list(range(0, 5400, 10)) + [5399, 5400]
        for status, header in ((429, "86400"), (429, "7"), (503, None))
    }
    assert statuses == {"error"}


def test_a_storm_that_ends_in_a_stall_is_still_a_fault() -> None:
    """A status is only how the call ENDED, and a storm need not end on one.

    A provider that rate-limits and then goes dark raises a bare stall:
    no status left to read, and no transport retry either, since the
    black-holed request runs its 600 s socket timeout and leaves under
    one 30 s backoff of budget. That satisfied both halves of the wall
    test, so 1500 s of unbroken provider trouble — three clamped
    `Retry-After`s of 300 s and then a 600 s black hole, beginning 3900 s
    into a 5400 s wall — published `budget_exhausted`, spent the node's
    generation, and RESET the two counters the transport breaker runs on.
    Measured band from the onset of trouble: 600 s for a silent black
    hole, 615 s behind one 429, 705 s behind three, 1500 s behind three
    clamped ones. The COUNT of failing replies is what survives the
    stall."""
    result, end, retries = _storm_then_stall_attempt(wall=5400.0, entry=3900.1)
    assert retries == 0, "the transport counter never moved"
    assert end > 5400.0, "and the stall landed past the wall, like a clean run"
    assert result.status == "error"
    assert result.detail == "model request failed: ReadTimeout"


def test_no_storm_that_ends_in_a_stall_is_ever_filed_as_the_wall() -> None:
    """The same over every onset, for the storm shapes the ladder takes."""
    wall = 5400.0
    statuses = {
        _storm_then_stall_attempt(
            wall=wall, entry=float(entry), storms=storms, retry_after=header
        )[0].status
        for entry in list(range(0, 5400, 10)) + [3900, 4695, 4785, 5399, 5400]
        # A clamped Retry-After, an honored one, and the exponential
        # fallback when the provider sends no header at all; one 429 and
        # a full ladder of them.
        for storms, header in ((1, None), (3, None), (3, "7"), (3, "86400"))
    }
    assert statuses == {"error"}


def test_a_rejected_effort_field_then_a_stall_is_a_fault() -> None:
    """The failing reply that takes no retry at all.

    The `reasoning_effort` fallback answers a 4xx and retries at once,
    spending no budget on either retry ladder — so a rule reading retry
    COUNTS cannot see it. When that fallback request then stalls at the
    wall, the call ends with no status and no delta, and a 422 the
    provider named 600 s earlier reads as a clean wall exhaustion.
    Counting failing REPLIES covers it for free."""
    clock = {"t": 0.0}
    posts = {"n": 0}
    stall = _fake_read_timeout()

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        if posts["n"] == 1:
            clock["t"] += 4800.1  # under 600 s of budget left after this
            return FakeResponse(200, _tool_reply("  nope\n"))
        if posts["n"] == 2:
            return FakeResponse(
                422, {"message": "Extra inputs are not permitted: reasoning_effort"}
            )
        clock["t"] += float(timeout)
        raise stall("black hole")

    config = _cfg(
        attempt_wall_seconds=5400.0,
        max_iterations=0,
        attempt_tokens=0,
        reasoning_effort="high",
    )
    client = ModelClient(config, "sk-k", post=post, sleep=sleep, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="bad"),
        now=now,
    )
    assert client.transport_retries == 0, "neither ladder retried"
    assert clock["t"] > 5400.0, "and the stall landed past the wall"
    assert result.status == "error"


def test_outage_with_no_retry_room_is_still_the_wall() -> None:
    """The complement: an outage that begins with under one backoff of
    budget left takes no retry, and its timeout is the wall-derived one
    firing at wall+grace — bit-identical to a clean wall run, so it
    stays `budget_exhausted` (the 8 live `perfect` rows, all wall_secs
    5430.1x)."""
    result, end, retries = _black_hole_attempt(wall=5400.0, entry=5000.0)
    assert retries == 0
    assert end == 5430.0, "the wall-derived timeout, not the 600 s cap"
    assert result.status == "budget_exhausted"
    assert "wall budget" in result.detail
    assert "ReadTimeout" in result.detail


def test_wall_classification_ignores_retries_on_earlier_calls() -> None:
    """The guard reads the DELTA across the failing call, not the
    client's cumulative counter: one live misfiling (TwoFanMenger,
    wall_secs 5430.1) reported `1 transport retries` from a stall
    EARLIER in the attempt that recovered. A recovered stall says
    nothing about how the last call ended."""
    clock = {"t": 0.0}
    posts = {"n": 0}
    stall = _fake_read_timeout()

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        if posts["n"] == 1:
            clock["t"] += 10.0  # a stall...
            raise stall("transient")
        if posts["n"] == 2:
            clock["t"] += 4960.0  # ...that recovered, then a long turn
            return FakeResponse(200, _tool_reply("  nope\n"))
        clock["t"] += float(timeout)  # the wall-derived timeout fires
        raise stall("out of budget")

    config = _cfg(attempt_wall_seconds=5400.0, max_iterations=0, attempt_tokens=0)
    client = ModelClient(config, "sk-k", post=post, sleep=sleep, now=now)
    result = run_attempt(
        config=config,
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="bad"),
        now=now,
    )
    assert client.transport_retries == 1, "cumulative counter is non-zero"
    assert clock["t"] == 5430.0
    assert result.status == "budget_exhausted"
    assert "wall budget" in result.detail


def test_transport_retry_is_wall_budget_aware() -> None:
    """A retry whose backoff cannot fit before the attempt deadline is
    not taken — the exception surfaces immediately instead of sleeping
    past the wall."""
    clock = {"t": 0.0}

    def now() -> float:
        return clock["t"]

    def sleep(secs: float) -> None:
        clock["t"] += secs

    posts = {"n": 0}

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        raise ConnectionResetError("peer reset")

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=sleep, now=now)
    # Remaining wall 20 s < the 30 s first backoff: no retry at all.
    with pytest.raises(ModelTransportError):
        client.chat([{"role": "user", "content": "x"}], read_timeout=20.0)
    assert posts["n"] == 1
    # Remaining wall 45 s: the 30 s backoff fits, the 60 s one does not.
    posts["n"] = 0
    clock["t"] = 0.0
    with pytest.raises(ModelTransportError):
        client.chat([{"role": "user", "content": "x"}], read_timeout=45.0)
    assert posts["n"] == 2


def test_rejected_4xx_carries_a_bounded_body_snippet() -> None:
    """A 400 is only diagnosable from its body; it rides the error
    message, capped."""

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        return FakeResponse(400, {"message": "context too long: " + "x" * 500})

    client = ModelClient(_cfg(), "sk-secret-XYZ", post=post, sleep=lambda s: None)
    with pytest.raises(ModelTransportError) as excinfo:
        client.chat([{"role": "user", "content": "x"}])
    detail = str(excinfo.value)
    assert detail.startswith("model request rejected: HTTP 400: ")
    assert "context too long" in detail
    assert len(detail) <= len("model request rejected: HTTP 400: ") + 200
    assert "sk-secret-XYZ" not in detail


def test_rejected_4xx_body_snippet_is_whitespace_normalized() -> None:
    """The snippet is capped at 200 chars, so raw whitespace is not
    cosmetic: a gateway's pretty-printed HTML/JSON error page spends the
    whole budget on newlines and indentation and truncates the one
    sentence that names the fault. Collapsing runs of whitespace first
    keeps the diagnosis inside the cap — and keeps the ledger row and
    the operator log line to ONE line."""

    class HtmlErrorResponse:
        status_code = 400
        headers: Dict[str, str] = {}
        text = (
            "<html>\n"
            + (" " * 24 + "\n") * 10  # a gateway page's indentation
            + "\t\tinput too long: 141000 tokens\n</html>\n"
        )

        def json(self):
            raise ValueError("not JSON")

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        return HtmlErrorResponse()

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=lambda s: None)
    with pytest.raises(ModelTransportError) as excinfo:
        client.chat([{"role": "user", "content": "x"}])
    detail = str(excinfo.value)
    assert "\n" not in detail and "\t" not in detail, "one line, always"
    assert "  " not in detail, "runs of whitespace collapse to a single space"
    assert "input too long: 141000 tokens" in detail, (
        "the cause survives the 200-char cap only because the padding "
        "collapsed first"
    )
    assert len(detail) <= len("model request rejected: HTTP 400: ") + 200


def test_nontransport_exception_never_retries() -> None:
    posts = {"n": 0}

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        posts["n"] += 1
        raise ValueError("malformed payload")

    client = ModelClient(_cfg(), "sk-k", post=post, sleep=lambda s: None)
    with pytest.raises(ModelTransportError, match="ValueError"):
        client.chat([{"role": "user", "content": "x"}])
    assert posts["n"] == 1


def test_key_never_leaks(tmp_path: Path, capsys) -> None:
    """A9 key hygiene: the key reaches ONLY the Authorization header of
    the HTTP call; it never appears in the payload, the attempt record,
    stdout/stderr, or a transport-error message."""
    secret = "sk-SUPER-SECRET-TOKEN"
    seen: Dict[str, Any] = {}

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        seen["headers"] = headers
        seen["payload"] = json
        raise ConnectionError(f"boom with header {headers['Authorization']}")

    client = ModelClient(_cfg(), secret, post=post, sleep=lambda s: None)
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "error"
    assert secret not in result.detail, "transport errors must not carry the key"
    assert secret in seen["headers"]["Authorization"]
    assert secret not in json.dumps(seen["payload"])
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
    )
    assert secret not in json.dumps(record)
    captured = capsys.readouterr()
    assert secret not in captured.out + captured.err


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
    assert record["provenance"]["model"] == "labs-leanstral-1-5"
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


# ---------------------------------------------------------------------------
# v3: reasoning_effort (set + graceful fallback), thinking-chunk stripping
# ---------------------------------------------------------------------------


def _text_reply(content, tokens=(10, 5)) -> Dict[str, Any]:
    """A plain assistant text reply (no tool call)."""
    return {
        "usage": {"prompt_tokens": tokens[0], "completion_tokens": tokens[1]},
        "choices": [{"message": {"content": content}}],
    }


def test_reasoning_effort_sent_when_configured() -> None:
    calls: List[Dict] = []
    client = _client_with_replies([_tool_reply("  trivial\n")], calls)
    run_attempt(
        config=_cfg(reasoning_effort="high"),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert calls[0]["payload"]["reasoning_effort"] == "high"


def test_reasoning_effort_omitted_when_none() -> None:
    calls: List[Dict] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"payload": json})
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(
        _cfg(reasoning_effort="none"), "sk", post=post, sleep=lambda s: None
    )
    client.chat([{"role": "user", "content": "x"}])
    assert "reasoning_effort" not in calls[0]["payload"]


def test_reasoning_effort_graceful_fallback_on_422() -> None:
    calls: List[Dict] = []
    logs: List[str] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"payload": dict(json)})
        # First request carries the field -> reject 422; retry (no field) OK.
        if "reasoning_effort" in json:
            return FakeResponse(422, {"error": "unknown field"})
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(
        _cfg(reasoning_effort="high"),
        "sk",
        post=post,
        sleep=lambda s: None,
        log=logs.append,
    )
    reply = client.chat([{"role": "user", "content": "x"}])
    assert reply["choices"][0]["message"]["tool_calls"]  # 200 path reached
    assert client.effort_disabled is True
    assert len(calls) == 2
    assert "reasoning_effort" in calls[0]["payload"]
    assert "reasoning_effort" not in calls[1]["payload"]
    assert any("reasoning_effort rejected" in m for m in logs)
    # Disabled for the client's lifetime: no field on subsequent calls.
    client.chat([{"role": "user", "content": "y"}])
    assert "reasoning_effort" not in calls[2]["payload"]


def test_reasoning_effort_fallback_when_body_names_the_field() -> None:
    """Combined-audit F4 (positive branch): a 400 whose body indicts
    `reasoning_effort` still triggers the one-shot fallback."""
    calls: List[Dict] = []
    logs: List[str] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"payload": dict(json)})
        if "reasoning_effort" in json:
            return FakeResponse(
                400,
                {"message": "Extra inputs are not permitted", "param": "reasoning_effort"},
            )
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(
        _cfg(reasoning_effort="high"),
        "sk",
        post=post,
        sleep=lambda s: None,
        log=logs.append,
    )
    client.chat([{"role": "user", "content": "x"}])
    assert client.effort_disabled is True
    assert any("reasoning_effort rejected" in m for m in logs)


def test_unrelated_4xx_keeps_reasoning_effort_on() -> None:
    """Combined-audit F4 (negative branch): ANY non-429 4xx used to
    permanently disable the trained regime. A 400 about something else
    (here the B3 unanswered-tool_call_id shape) must leave effort on and
    take the normal error path."""
    calls: List[Dict] = []
    logs: List[str] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"payload": dict(json)})
        return FakeResponse(
            400,
            {
                "message": (
                    "Assistant message with tool_calls must be followed by "
                    "tool messages responding to each tool_call_id"
                )
            },
        )

    client = ModelClient(
        _cfg(reasoning_effort="high"),
        "sk",
        post=post,
        sleep=lambda s: None,
        log=logs.append,
    )
    with pytest.raises(ModelTransportError):
        client.chat([{"role": "user", "content": "x"}])
    assert client.effort_disabled is False
    assert len(calls) == 1, "no silent retry-without-effort"
    assert "reasoning_effort" in calls[0]["payload"]
    assert logs == []


def test_reasoning_effort_recorded_on_result() -> None:
    calls: List[Dict] = []
    client = _client_with_replies([_tool_reply("  trivial\n")], calls)
    result = run_attempt(
        config=_cfg(reasoning_effort="high"),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.reasoning_effort == "high"


def test_thinking_chunks_stripped_from_history() -> None:
    """With effort set, content is a chunk list; thinking is dropped and
    never re-sent, the fenced body is still extracted from the text."""
    from trellis.sidecar.driver import _content_text

    chunked = [
        {"type": "thinking", "text": "secret reasoning"},
        {"type": "text", "text": "Here:\n```lean\n  trivial\n```"},
    ]
    assert _content_text(chunked) == "Here:\n```lean\n  trivial\n```"
    assert "secret reasoning" not in _content_text(chunked)

    calls: List[Dict] = []

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"payload": json})
        # A chunked reply with NO tool call -> fenced-body fallback path.
        if len(calls) == 1:
            return FakeResponse(
                200,
                {
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5},
                    "choices": [{"message": {"content": chunked}}],
                },
            )
        return FakeResponse(200, _tool_reply("  trivial\n"))

    client = ModelClient(_cfg(), "sk", post=post, sleep=lambda s: None)
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "success"
    # No later request echoed the thinking text back into history.
    for call in calls:
        for m in call["payload"]["messages"]:
            assert "secret reasoning" not in str(m.get("content", ""))


def _thinking_only_reply(tokens=(10, 5)) -> Dict[str, Any]:
    """The live shape behind the HTTP 400: effort on, the model spends
    the turn reasoning and emits no text chunk and no tool call, so the
    thinking-stripped content normalizes to the empty string."""
    return {
        "usage": {"prompt_tokens": tokens[0], "completion_tokens": tokens[1]},
        "choices": [
            {"message": {"content": [{"type": "thinking", "text": "hmm"}]}}
        ],
    }


def _assistant_echoes(calls: List[Dict]) -> List[Dict[str, Any]]:
    """Every assistant message the driver ever put on the wire."""
    return [
        m
        for call in calls
        for m in call["payload"]["messages"]
        if m.get("role") == "assistant"
    ]


def test_thinking_only_reply_is_echoed_in_a_sendable_shape() -> None:
    """A thinking-only turn used to echo {"role": "assistant",
    "content": ""} and the NEXT request 400'd on it, ending an attempt
    that had run 25 minutes."""
    calls: List[Dict] = []
    client = _client_with_replies(
        [_thinking_only_reply(), _tool_reply("  trivial\n")], calls
    )
    result = run_attempt(
        config=_cfg(reasoning_effort="high"),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "success", "the attempt survives the empty turn"
    assert len(calls) == 2, "the nudge turn was actually sent"
    for call in calls:
        assert_assistant_messages_sendable(call["payload"]["messages"])
    assert _assistant_echoes(calls), "the empty turn is still in the history"


def test_empty_tool_body_echo_keeps_no_unanswered_tool_call() -> None:
    """A lean_run_code call whose body is blank takes the same nudge
    path with tool_calls in hand. They are dropped rather than echoed:
    the nudge that follows is a user message, so an echoed id would go
    unanswered — the other 400 in the same family."""
    calls: List[Dict] = []
    blank = _tool_reply("   \n")
    blank["choices"][0]["message"]["content"] = [
        {"type": "thinking", "text": "hmm"}
    ]
    client = _client_with_replies([blank, _tool_reply("  trivial\n")], calls)
    result = run_attempt(
        config=_cfg(reasoning_effort="high"),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "success"
    for call in calls:
        assert_assistant_messages_sendable(call["payload"]["messages"])


def test_thinking_only_reply_after_the_nudge_is_also_sendable() -> None:
    """The `nudged` flag latches, so a later empty turn takes the second
    nudge branch — a separate construction of the same message."""
    calls: List[Dict] = []
    client = _client_with_replies(
        [
            _text_reply("let me think...", tokens=(5, 5)),
            _thinking_only_reply(),
            _tool_reply("  trivial\n"),
        ],
        calls,
    )
    result = run_attempt(
        config=_cfg(reasoning_effort="high"),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log=""),
    )
    assert result.status == "success"
    assert len(calls) == 3
    for call in calls:
        assert_assistant_messages_sendable(call["payload"]["messages"])


def test_thinking_only_reply_survives_a_compaction_reset() -> None:
    """The compaction turn reads the same reply shape (its summary comes
    from `_content_text`) and the post-reset history is rebuilt from the
    envelope, so the empty turn must not ride through either."""
    calls: List[Dict] = []
    client = _client_with_replies(
        [
            _tool_reply("  exact nope\n", tokens=(120, 30)),
            _thinking_only_reply(tokens=(120, 30)),
            _thinking_only_reply(tokens=(10, 5)),
            _tool_reply("  trivial\n", tokens=(10, 5)),
        ],
        calls,
    )
    result = run_attempt(
        config=_cfg(reasoning_effort="high", compact_context_tokens=100),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(
            ok=body.strip() == "trivial", log="nope"
        ),
    )
    assert result.status == "success"
    assert result.compactions == 1, "the compaction turn fumbled its summary"
    for call in calls:
        assert_assistant_messages_sendable(call["payload"]["messages"])


# ---------------------------------------------------------------------------
# v3: context compaction (threshold trip -> model summary -> continue)
# ---------------------------------------------------------------------------


def test_compaction_trips_and_continues_with_cumulative_budget() -> None:
    calls: List[Dict] = []
    # reply1: failing body, tokens above the compaction threshold.
    # reply2: the compaction summary (plain text with <summary> tags).
    # reply3: a success body from the reset conversation.
    replies = [
        _tool_reply("  exact nope\n", tokens=(120, 30)),
        _text_reply("<summary>best body: trivial; plan: submit it</summary>",
                    tokens=(40, 10)),
        _tool_reply("  trivial\n", tokens=(15, 5)),
    ]

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"payload": json})
        return FakeResponse(200, replies[min(len(calls) - 1, len(replies) - 1)])

    client = ModelClient(_cfg(), "sk", post=post, sleep=lambda s: None)
    verdicts = [CompileVerdict(ok=False, log="bad"), CompileVerdict(ok=True, log="")]
    seq = iter(verdicts)
    result = run_attempt(
        config=_cfg(compact_context_tokens=50, max_iterations=0, attempt_tokens=0),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: next(seq),
    )
    assert result.status == "success"
    assert result.compactions == 1
    # Cumulative token budget across the compaction round.
    assert result.prompt_tokens == 120 + 40 + 15
    assert result.completion_tokens == 30 + 10 + 5
    assert result.compaction_round_tokens == [120 + 30 + 40 + 10]
    # The 3rd request ran off a RESET conversation: [system, envelope].
    third = calls[2]["payload"]["messages"]
    assert third[0]["role"] == "system"
    assert "continuing a proof attempt after a context compaction" in third[1]["content"]
    assert "best body: trivial" in third[1]["content"]
    # The original task is preserved verbatim in the envelope.
    assert "Current body below the marker" in third[1]["content"]


def test_compaction_synthesized_fallback_when_model_fumbles() -> None:
    calls: List[Dict] = []
    replies = [
        _tool_reply("  exact almost\n", tokens=(120, 30)),
        # Fumbled summary turn: no <summary>, empty content.
        _text_reply("", tokens=(5, 0)),
        _tool_reply("  trivial\n", tokens=(15, 5)),
    ]

    def post(url, headers=None, json=None, timeout=None):  # noqa: A002
        calls.append({"payload": json})
        return FakeResponse(200, replies[min(len(calls) - 1, len(replies) - 1)])

    client = ModelClient(_cfg(), "sk", post=post, sleep=lambda s: None)
    seq = iter([CompileVerdict(ok=False, log="nope"), CompileVerdict(ok=True, log="")])
    result = run_attempt(
        config=_cfg(compact_context_tokens=50, max_iterations=0, attempt_tokens=0),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: next(seq),
    )
    assert result.status == "success"
    assert result.compactions == 1
    # Synthesized handoff carried the last body + last feedback forward.
    envelope = calls[2]["payload"]["messages"][1]["content"]
    assert "no summary was produced" in envelope
    assert "exact almost" in envelope
    assert "nope" in envelope


def test_compaction_disabled_when_threshold_zero() -> None:
    calls: List[Dict] = []
    client = _client_with_replies(
        [_tool_reply("  exact nope\n", tokens=(10_000, 5_000))], calls
    )
    result = run_attempt(
        config=_cfg(compact_context_tokens=0, max_iterations=3),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="no"),
    )
    assert result.compactions == 0
    assert result.status == "budget_exhausted"


# ---------------------------------------------------------------------------
# v3: wall-only regime — token / iteration caps disabled (0)
# ---------------------------------------------------------------------------


def test_wall_only_regime_ignores_token_and_iteration_caps() -> None:
    """max_iterations=0 and attempt_tokens=0 => neither ever ends the
    attempt; only the wall does. Huge per-turn token counts and many
    iterations do not trip a budget."""
    clock = {"t": 0.0}

    def now() -> float:
        # Small per-call advance so many iterations fit inside the wall
        # (never a token / iteration cap ends the attempt).
        clock["t"] += 1.0
        return clock["t"]

    calls: List[Dict] = []
    client = _client_with_replies(
        [_tool_reply("  exact nope\n", tokens=(5_000_000, 1_000_000))], calls
    )
    result = run_attempt(
        # Compaction OFF to isolate the token/iteration-cap behavior.
        config=_cfg(
            attempt_wall_seconds=1000.0,
            max_iterations=0,
            attempt_tokens=0,
            compact_context_tokens=0,
        ),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=False, log="no"),
        now=now,
    )
    assert result.status == "budget_exhausted"
    assert result.detail == "wall budget"
    # Ran many turns despite 6M-token replies and no iteration cap — a
    # token or iteration cap would have stopped it at 1 turn.
    assert result.iterations >= 3
    assert result.prompt_tokens + result.completion_tokens > 500_000


# ---------------------------------------------------------------------------
# AUDIT U1 / P4 / P9 — the grep engine is ripgrep-only, contained, and
# can never end an attempt.
# ---------------------------------------------------------------------------

# Patterns whose Python `re` cost EXPLODES: `re` backtracks, and its
# search cannot be interrupted from Python, so before U1 each of these
# ran effectively forever inside `pattern.search()` on a normal Lean
# line. Measured on the host that carried the live run, `([A-Za-z_]+)+!`
# alone took 0.27 / 1.08 / 4.27 / 17.2 s at 22 / 24 / 26 / 28 word
# characters — doubling every two characters, against real Lean lines of
# 40-80. The last entry is the lookaround form that ripgrep's default
# engine REFUSES, which is exactly how the model used to force the
# silent Python fallback.
CATASTROPHIC_QUERIES = [
    "([A-Za-z_]+)+!",
    "([A-Za-z_.]+)+ :=",
    "(a+)+$",
    "(x|x)*y",
    "(?=z)(z+)+q",
]


def _redos_repo(tmp_path: Path) -> Path:
    """A tablet whose lines are the subject these patterns blow up on:
    long runs of word characters, as every real Lean line is."""
    repo = _seed_repo(tmp_path)
    line = "theorem " + "_".join("aaaaaaaa" for _ in range(10)) + " := by\n"
    for index in range(12):
        (repo / "Tablet" / f"Redos{index}.lean").write_text(line * 40)
    _seed_mathlib(repo)
    (repo / ".lake" / "packages" / "mathlib" / "Mathlib" / "Redos.lean").write_text(
        line * 40
    )
    return repo


@pytest.mark.parametrize("query", CATASTROPHIC_QUERIES)
def test_catastrophic_backtracking_query_returns_promptly(
    tmp_path: Path, query: str
) -> None:
    """U1: a query built to blow up a backtracking engine must return a
    bounded result or a clean error, PROMPTLY, on BOTH grep tools.

    The bound is real (a killed subprocess), not advisory: ripgrep's
    default engine is a finite automaton, and the timeout is enforced by
    `subprocess.run` on a child rather than checked between files by a
    Python loop that a single `search()` call never returns to."""
    import time as time_mod

    repo = _redos_repo(tmp_path)
    handlers = make_info_tool_handlers(repo, "Rung")
    for tool in ("search_tablet", "search_mathlib_source"):
        started = time_mod.monotonic()
        out = handlers[tool]({"query": query})
        elapsed = time_mod.monotonic() - started
        assert elapsed < 10.0, f"{tool} took {elapsed:.1f}s on {query!r}"
        # A real answer of some kind — hits, "no match", or a named
        # tool-level error — never a traceback and never a hang.
        assert out.startswith(f"{tool}: ") or "hit(s) for" in out


def test_no_in_process_regex_engine_remains(tmp_path: Path) -> None:
    """U1, structurally: the module must not carry a model-driven `re`
    scanner at all. A fallback that exists is a fallback that can be
    reached — the deleted one was reachable by any query ripgrep's
    default engine rejects."""
    import trellis.sidecar.driver as driver_mod

    for gone in ("compile_query", "grep_paths", "_mathlib_walk"):
        assert not hasattr(driver_mod, gone), f"{gone} is still importable"


def test_lookaround_cannot_reach_a_backtracking_engine(tmp_path: Path) -> None:
    """U1: rejected-pattern handling re-runs the query as a FIXED STRING
    (linear) instead of handing it to a backtracking engine. `--engine
    default` is passed explicitly so ripgrep cannot silently upgrade to
    PCRE2, which backtracks, on seeing lookaround."""
    repo = _seed_repo(tmp_path)
    (repo / "Tablet" / "Lit.lean").write_text("-- literally (?=z)(z+)+q here\n")
    search = make_info_tool_handlers(repo, "Rung")["search_tablet"]
    out = search({"query": "(?=z)(z+)+q"})
    assert "matched literally" in out
    assert "Tablet/Lit.lean" in out


def test_search_timeout_is_enforced_by_killing_ripgrep(
    tmp_path: Path, monkeypatch
) -> None:
    """U1: the deadline is a subprocess timeout, so it FIRES. The old
    deadline was checked between files and could not interrupt a match
    already running, which is why SEARCH_TABLET_DEADLINE_SECS never
    fired in production."""
    import subprocess as subprocess_mod

    repo = _seed_repo(tmp_path)
    real_run = subprocess_mod.run
    seen: List[float] = []

    def slow_run(command, **kwargs):
        seen.append(kwargs.get("timeout", -1.0))
        raise subprocess_mod.TimeoutExpired(command, kwargs.get("timeout", 0.0))

    monkeypatch.setattr(subprocess_mod, "run", slow_run)
    out = make_info_tool_handlers(repo, "Rung")["search_tablet"]({"query": "needle"})
    monkeypatch.setattr(subprocess_mod, "run", real_run)
    assert "timed out" in out and "search_tablet:" in out
    # A timeout is not retried into a second full-length wait: the retry
    # budget is what is LEFT, so the whole call stays under the cap.
    assert seen and all(0 < t <= SEARCH_TABLET_DEADLINE_SECS for t in seen)


def test_mathlib_symlink_cannot_leak_its_target(tmp_path: Path) -> None:
    """AUDIT P4: `read_file` gates every open with `resolve_readable`,
    but the grep walk used to `open()` whatever the walk yielded. A
    `*.lean` SYMLINK under the mathlib root then leaked its target — and
    this user's readable secrets include ~/.codex/auth.json. Containment
    is enforced at the open, so no walk can reintroduce it."""
    repo = _seed_repo(tmp_path)
    root = _seed_mathlib(repo)
    secret = tmp_path / "outside_secret.lean"
    secret.write_text("theorem SECRET_TOKEN_MARKER : True := trivial\n")
    link = root / "Leak.lean"
    try:
        link.symlink_to(secret)
    except OSError:
        pytest.skip("symlinks unavailable")
    assert link.is_file(), "the fixture symlink resolves (the leak is real)"

    handlers = make_info_tool_handlers(repo, "Rung")
    out = handlers["search_mathlib_source"]({"query": "SECRET_TOKEN_MARKER"})
    assert "SECRET_TOKEN_MARKER" not in out.replace("SECRET_TOKEN_MARKER'", "")
    assert "Leak.lean" not in out
    # read_file already refused it; it still does.
    rel = ".lake/packages/mathlib/Mathlib/Leak.lean"
    assert "SECRET_TOKEN_MARKER" not in handlers["read_file"]({"path": rel})


def test_tablet_symlink_cannot_leak_its_target(tmp_path: Path) -> None:
    """P4, the same containment on the tablet root."""
    repo = _seed_repo(tmp_path)
    secret = tmp_path / "outside_secret.lean"
    secret.write_text("theorem SECRET_TOKEN_MARKER : True := trivial\n")
    link = repo / "Tablet" / "Leak.lean"
    try:
        link.symlink_to(secret)
    except OSError:
        pytest.skip("symlinks unavailable")
    out = make_info_tool_handlers(repo, "Rung")["search_tablet"](
        {"query": "SECRET_TOKEN_MARKER"}
    )
    # The query is echoed in the verdict; what must NOT appear is a HIT.
    assert "no match" in out
    assert "Leak.lean" not in out
    assert "trivial" not in out


@pytest.mark.parametrize(
    "query", ["(" * 500 + "a" + ")" * 500, "a{4294967295}", "(?P<n>a)(?P<n>b)"]
)
def test_a_handler_exception_is_a_tool_error_not_a_dead_attempt(
    tmp_path: Path, query: str
) -> None:
    """AUDIT P9: a query that makes a handler raise OUTSIDE the class it
    catches — RecursionError from 500 nested groups, OverflowError from a
    huge repeat count — used to propagate out of `_answer_info_calls` and
    kill the whole in-flight attempt: up to 90 minutes and 150M prompt
    tokens discarded, the transport-suspension counter ticked, and at
    ERROR_STREAK_THRESHOLD the node's generation burned. It must be an
    ordinary tool result the model can simply retry past."""
    repo = _seed_repo(tmp_path)
    calls: List[Dict] = []
    client = _client_with_replies(
        [
            _info_reply("search_tablet", {"query": query}),
            _tool_reply("  trivial\n"),
        ],
        calls,
    )
    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log="ok"),
        info_tools=make_info_tool_handlers(repo, "Rung"),
        extra_tool_specs=info_tool_specs(),
    )
    # The attempt SURVIVED the poisoned query and closed on the next turn.
    assert result.status == "success"
    assert result.info_tool_counts.get("search_tablet") == 1


def test_a_raising_handler_never_escapes_the_attempt(tmp_path: Path) -> None:
    """P9 at the seam itself: ANY exception from ANY info tool — not
    just the regex family — is caught and reported."""
    calls: List[Dict] = []
    client = _client_with_replies(
        [_info_reply("boom", {"x": 1}), _tool_reply("  trivial\n")], calls
    )

    def explode(_args: Dict[str, Any]) -> str:
        raise RecursionError("maximum recursion depth exceeded")

    result = run_attempt(
        config=_cfg(),
        client=client,
        system_prompt="SYS",
        initial_body="  sorry\n",
        compile_body=lambda body: CompileVerdict(ok=True, log="ok"),
        info_tools={"boom": explode},
        extra_tool_specs=[
            {"type": "function", "function": {"name": "boom", "parameters": {}}}
        ],
    )
    assert result.status == "success"
    answered = [m for m in calls[-1]["payload"]["messages"] if m.get("role") == "tool"]
    assert any(
        "RecursionError" in str(m.get("content", "")) for m in answered
    ), "the failure is reported back to the model as a tool result"


def test_containment_refuses_an_escaping_path_at_the_open(tmp_path: Path) -> None:
    """P4 at the enforcement point, independent of what the walk yields.

    ripgrep happens not to follow symlinks, but the containment must not
    RELY on that: the walk is an external tool over a tree a mathlib bump
    can change. This drives the hit-formatting path directly with a
    match already attributed to an escaping path — the shape a
    follow-symlinks walker, a `--follow` flag, or a hardlink-like trick
    would produce — and it must render nothing."""
    from trellis.sidecar.driver import RipgrepHits, _hit_blocks, _read_lines

    repo = _seed_repo(tmp_path)
    root = _seed_mathlib(repo)
    secret = tmp_path / "outside_secret.lean"
    secret.write_text("theorem SECRET_TOKEN_MARKER : True := trivial\n")
    try:
        (root / "Leak.lean").symlink_to(secret)
    except OSError:
        pytest.skip("symlinks unavailable")

    # The low-level read refuses it outright.
    assert _read_lines(root, "Leak.lean") is None
    assert _read_lines(root, "../../../../outside_secret.lean") is None
    # A contained file still reads.
    assert _read_lines(root, "Order/Basic.lean")

    # And a hit already attributed to it renders nothing.
    found = _hit_blocks(
        root,
        lambda rel: rel,
        RipgrepHits(hits=[("Leak.lean", 1)]),
        2,
    )
    assert found.hits == 0
    assert "SECRET_TOKEN_MARKER" not in found.rendered()

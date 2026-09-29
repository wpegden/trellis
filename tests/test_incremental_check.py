"""Unit tests for the advisory warm-server pre-check translation logic.

These cover the load-bearing false-green guards (MAJOR-3) and the node/path
plumbing without needing a real `lean --server`.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from trellis import incremental_check as ic


def test_parse_target_accepts_qualified_and_bare() -> None:
    assert ic._parse_target("Tablet.Foo") == "Foo"
    assert ic._parse_target("Foo_Bar1") == "Foo_Bar1"


@pytest.mark.parametrize("bad", ["../etc", "1Foo", "Foo Bar", "", "Foo;rm"])
def test_parse_target_rejects_non_identifier(bad: str) -> None:
    with pytest.raises(ValueError):
        ic._parse_target(bad)


def test_sandbox_guard_refuses_outside_bwrap(monkeypatch) -> None:
    monkeypatch.delenv("TMPDIR", raising=False)
    inside, detail = ic._sandbox_markers()
    assert not inside
    assert "worker sandbox" in detail


def test_error_severity_diagnostic_fails() -> None:
    repo, node = Path("/x"), "N"
    uri = ic._uri_for(repo, node)
    failed, error_lines, sorry_lines = ic._classify_diagnostics(
        repo,
        node,
        {uri: [{"severity": ic._SEVERITY_ERROR, "message": "oops",
                "range": {"start": {"line": 4, "character": 2}}}]},
    )
    assert failed
    assert error_lines == ["Tablet/N.lean:5:3: error: oops"]
    assert sorry_lines == []


@pytest.mark.parametrize(
    "message",
    ["declaration uses `sorry`", "declaration uses 'sorry'", "declaration uses sorry"],
)
def test_sorry_warning_is_info_not_failure(message: str) -> None:
    # Corrected semantics: a `sorry` warning is INFO, not failure. `lake build`
    # reports `sorry` as a warning and exits 0, and a proof-formalization node
    # legitimately carries a PERMITTED open `sorry` on its authorized branch.
    # The location is surfaced (in sorry_lines) but does NOT fail the check; the
    # deterministic worker check enforces no-sorry-at-submit, not this tool.
    repo, node = Path("/x"), "N"
    uri = ic._uri_for(repo, node)
    failed, error_lines, sorry_lines = ic._classify_diagnostics(
        repo,
        node,
        {uri: [{"severity": ic._SEVERITY_WARNING, "message": message,
                "range": {"start": {"line": 0, "character": 0}}}]},
    )
    assert not failed
    assert error_lines == []
    assert sorry_lines == ["Tablet/N.lean:1:1: warning: " + message]


def test_error_plus_sorry_fails_with_error_and_notes_sorry() -> None:
    # Error + sorry together: fail on the error, but the sorry location is still
    # surfaced as info.
    repo, node = Path("/x"), "N"
    uri = ic._uri_for(repo, node)
    failed, error_lines, sorry_lines = ic._classify_diagnostics(
        repo,
        node,
        {uri: [
            {"severity": ic._SEVERITY_ERROR, "message": "boom",
             "range": {"start": {"line": 2, "character": 0}}},
            {"severity": ic._SEVERITY_WARNING, "message": "declaration uses 'sorry'",
             "range": {"start": {"line": 5, "character": 0}}},
        ]},
    )
    assert failed
    assert error_lines == ["Tablet/N.lean:3:1: error: boom"]
    assert sorry_lines == ["Tablet/N.lean:6:1: warning: declaration uses 'sorry'"]


def test_clean_node_passes_with_no_sorry() -> None:
    repo, node = Path("/x"), "N"
    uri = ic._uri_for(repo, node)
    failed, error_lines, sorry_lines = ic._classify_diagnostics(
        repo, node, {uri: []},
    )
    assert not failed
    assert error_lines == []
    assert sorry_lines == []


def test_plain_warning_does_not_fail() -> None:
    repo, node = Path("/x"), "N"
    uri = ic._uri_for(repo, node)
    failed, error_lines, sorry_lines = ic._classify_diagnostics(
        repo,
        node,
        {uri: [{"severity": ic._SEVERITY_WARNING, "message": "unused variable",
                "range": {"start": {"line": 0, "character": 0}}}]},
    )
    assert not failed
    # A non-sorry plain warning is dropped entirely (lake exits 0 with warnings).
    assert error_lines == []
    assert sorry_lines == []


def test_build_verdict_ok_with_sorry_carries_sorry_info() -> None:
    v = ic._build_verdict(False, [], ["Tablet/N.lean:1:1: warning: declaration uses `sorry`"])
    assert v == {
        "verdict": "ok",
        "sorry": ["Tablet/N.lean:1:1: warning: declaration uses `sorry`"],
    }


def test_build_verdict_fail_carries_errors_and_sorry() -> None:
    v = ic._build_verdict(True, ["Tablet/N.lean:1:1: error: x"], ["Tablet/N.lean:2:1: warning: declaration uses `sorry`"])
    assert v == {
        "verdict": "fail",
        "lines": ["Tablet/N.lean:1:1: error: x"],
        "sorry": ["Tablet/N.lean:2:1: warning: declaration uses `sorry`"],
    }


def test_build_verdict_ok_no_sorry_omits_key() -> None:
    assert ic._build_verdict(False, [], []) == {"verdict": "ok"}


def test_foreign_uri_error_fails_with_foreign_path() -> None:
    repo, node = Path("/x"), "N"
    failed, error_lines, sorry_lines = ic._classify_diagnostics(
        repo,
        node,
        {"file:///other/Tablet/Dep.lean": [
            {"severity": ic._SEVERITY_ERROR, "message": "dep broke",
             "range": {"start": {"line": 1, "character": 0}}}]},
    )
    assert failed
    assert error_lines == ["/other/Tablet/Dep.lean:2:1: error: dep broke"]
    assert sorry_lines == []


def test_transitive_import_closure_excludes_self(tmp_path) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "A.lean").write_text("import Tablet.B\n", encoding="utf-8")
    (tablet / "B.lean").write_text("import Tablet.C\n", encoding="utf-8")
    (tablet / "C.lean").write_text("-- leaf\n", encoding="utf-8")
    closure = ic._transitive_import_closure(tmp_path, "A")
    assert set(closure) == {"B", "C"}
    assert "A" not in closure


def _apply_change(old: str, change) -> str:
    rng = change["range"]
    lines = old.split("\n")
    # convert (line,char) to absolute offset
    def off(pos):
        o = sum(len(l) + 1 for l in lines[: pos["line"]]) + pos["character"]
        return o
    a, b = off(rng["start"]), off(rng["end"])
    return old[:a] + change["text"] + old[b:]


def test_range_content_change_late_edit() -> None:
    old = "line0\nline1\nline2 body\nline3\n"
    new = "line0\nline1\nline2 body MORE\nline3\n"
    change = ic._range_content_change(old, new)
    # the replaced region must be small (not the whole document)
    assert change["range"]["start"]["line"] == 2
    assert _apply_change(old, change) == new


def test_range_content_change_prefix_edit() -> None:
    old = "AAA\nBBB\nCCC\n"
    new = "AZZ\nBBB\nCCC\n"
    change = ic._range_content_change(old, new)
    assert _apply_change(old, change) == new


def test_range_content_change_insertion() -> None:
    old = "a\nb\nc\n"
    new = "a\nb\nINSERTED\nc\n"
    change = ic._range_content_change(old, new)
    assert _apply_change(old, change) == new


# --------------------------------------------------------------------------
# Broker framing + verdict translation (no real lean --server needed)
# --------------------------------------------------------------------------

def test_socket_frame_roundtrip() -> None:
    import socket as _socket

    a, b = _socket.socketpair()
    try:
        ic._send_frame(a, {"method": "check", "node": "Foo"})
        got = ic._recv_frame(b)
    finally:
        a.close()
        b.close()
    assert got == {"method": "check", "node": "Foo"}


def test_recv_frame_eof_returns_none() -> None:
    import socket as _socket

    a, b = _socket.socketpair()
    a.close()  # peer closed -> EOF on b
    try:
        assert ic._recv_frame(b) is None
    finally:
        b.close()


def _patch_broker_result(monkeypatch, result):
    monkeypatch.setattr(ic, "_broker_check", lambda repo, node: result)


def _capture_fallback(monkeypatch):
    calls = {}

    def fake(repo, node_name, *, reason, advice=""):
        calls["reason"] = reason
        calls["node"] = node_name
        calls["advice"] = advice
        return 99

    monkeypatch.setattr(ic, "_lake_build_fallback", fake)
    return calls


def test_run_incremental_ok_verdict(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("-- ok\n", encoding="utf-8")
    _patch_broker_result(monkeypatch, {"verdict": "ok"})
    assert ic._run_incremental(tmp_path, "N") == 0


def test_run_incremental_fail_verdict(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("-- broken\n", encoding="utf-8")
    _patch_broker_result(
        monkeypatch, {"verdict": "fail", "lines": ["Tablet/N.lean:1:1: error: x"]}
    )
    assert ic._run_incremental(tmp_path, "N") == 1


def test_run_incremental_ok_with_sorry_is_exit0_and_reports_sorry(
    tmp_path, monkeypatch, capsys
) -> None:
    # A node that elaborates clean but still carries a PERMITTED open `sorry`:
    # exit 0, with the sorry location surfaced as info (NOT a failure).
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("-- ok with sorry\n", encoding="utf-8")
    _patch_broker_result(
        monkeypatch,
        {"verdict": "ok",
         "sorry": ["Tablet/N.lean:3:1: warning: declaration uses `sorry`"]},
    )
    assert ic._run_incremental(tmp_path, "N") == 0
    out = capsys.readouterr().out
    assert "Tablet/N.lean:3:1: warning: declaration uses `sorry`" in out
    assert "sorry` remains" in out
    assert "`sorry` warning(s) remain" in out


def test_run_incremental_fail_with_sorry_reports_both(
    tmp_path, monkeypatch, capsys
) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("-- broken with sorry\n", encoding="utf-8")
    _patch_broker_result(
        monkeypatch,
        {"verdict": "fail",
         "lines": ["Tablet/N.lean:1:1: error: x"],
         "sorry": ["Tablet/N.lean:3:1: warning: declaration uses `sorry`"]},
    )
    assert ic._run_incremental(tmp_path, "N") == 1
    out = capsys.readouterr().out
    assert "Tablet/N.lean:1:1: error: x" in out
    assert "Tablet/N.lean:3:1: warning: declaration uses `sorry`" in out


def test_scan_for_tabs_clean_returns_none(tmp_path) -> None:
    p = tmp_path / "N.lean"
    p.write_text("def f := 1\n  def g := 2\n", encoding="utf-8")
    assert ic._scan_for_tabs(p, "N") is None


def test_scan_for_tabs_reports_tab_with_location(tmp_path, capsys) -> None:
    p = tmp_path / "N.lean"
    p.write_text("def f :=\n\tby trivial\n", encoding="utf-8")
    assert ic._scan_for_tabs(p, "N") == 1
    out = capsys.readouterr().out
    assert "Tablet/N.lean:2:1: error: tab character not allowed" in out


def test_run_incremental_fast_fails_on_tab_without_server(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("def f :=\n\tby trivial\n", encoding="utf-8")

    def _boom(*a, **k):
        raise AssertionError("no server round-trip may happen when a tab is present")

    monkeypatch.setattr(ic, "_active_prewarm_check", _boom)
    monkeypatch.setattr(ic, "_broker_check", _boom)
    monkeypatch.setattr(ic, "_lake_build_fallback", _boom)
    assert ic._run_incremental(tmp_path, "N") == 1


def test_run_incremental_fallback_on_broker_unavailable(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("-- ok\n", encoding="utf-8")
    _patch_broker_result(monkeypatch, None)
    calls = _capture_fallback(monkeypatch)
    assert ic._run_incremental(tmp_path, "N") == 99
    assert "broker unavailable" in calls["reason"]


def test_run_incremental_fallback_verdict_passes_reason(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("-- ok\n", encoding="utf-8")
    _patch_broker_result(
        monkeypatch,
        {"verdict": "fallback", "reason": "transitive import changed (stale prefix)"},
    )
    calls = _capture_fallback(monkeypatch)
    assert ic._run_incremental(tmp_path, "N") == 99
    assert "stale prefix" in calls["reason"]


def test_run_incremental_unknown_verdict_falls_back(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("-- ok\n", encoding="utf-8")
    _patch_broker_result(monkeypatch, {"verdict": "weird"})
    calls = _capture_fallback(monkeypatch)
    assert ic._run_incremental(tmp_path, "N") == 99
    assert "unrecognized" in calls["reason"]


class _ScriptedServer:
    """A _LeanServer-shaped stub whose drain() replays a scripted list of
    messages, then behaves per `tail` ('silent' | 'dead')."""

    def __init__(self, messages, tail="silent"):
        self._msgs = list(messages)
        self._tail = tail
        self._alive = True

    def drain(self, timeout):
        if self._msgs:
            return self._msgs.pop(0)
        if self._tail == "dead":
            self._alive = False
            return None
        return None  # silent

    def alive(self):
        return self._alive


def _progress(uri, n):
    return {
        "method": "$/lean/fileProgress",
        "params": {"textDocument": {"uri": uri}, "processing": [{}] * n},
    }


def _diag(uri, sev, msg):
    return {
        "method": "textDocument/publishDiagnostics",
        "params": {"uri": uri, "diagnostics": [
            {"severity": sev, "message": msg,
             "range": {"start": {"line": 0, "character": 0}}}]},
    }


def test_wait_terminal_complete_then_classify(monkeypatch) -> None:
    repo, node = Path("/x"), "N"
    uri = ic._uri_for(repo, node)
    srv = _ScriptedServer([
        _progress(uri, 1),                 # processing starts
        _diag(uri, ic._SEVERITY_ERROR, "boom"),
        _progress(uri, 0),                 # terminal (empty after non-empty)
    ])
    status, _t, by_uri = ic._wait_terminal_progress(srv, uri)
    assert status == "complete"
    failed, error_lines, _sorry = ic._classify_diagnostics(repo, node, by_uri)
    assert failed and error_lines and "boom" in error_lines[0]


def test_wait_terminal_inactivity_is_ambiguous(monkeypatch) -> None:
    # Activity then silence with NO terminal -> ambiguous (bounded, not a hang).
    monkeypatch.setattr(ic, "_QUIET_TIMEOUT_SECS", 0.3)
    uri = ic._uri_for(Path("/x"), "N")
    srv = _ScriptedServer([_progress(uri, 3)], tail="silent")
    status, _t, _b = ic._wait_terminal_progress(srv, uri)
    assert status == "ambiguous"


def test_wait_terminal_crash(monkeypatch) -> None:
    monkeypatch.setattr(ic, "_QUIET_TIMEOUT_SECS", 5.0)
    uri = ic._uri_for(Path("/x"), "N")
    srv = _ScriptedServer([_progress(uri, 2)], tail="dead")
    status, _t, _b = ic._wait_terminal_progress(srv, uri)
    assert status == "crashed"


class _FakeServer:
    """Stand-in for _LeanServer to unit-test broker invalidation logic.

    Records didOpen/didClose so tests can assert targeted reloads."""

    _next = 0

    def __init__(self, repo) -> None:
        _FakeServer._next += 1
        self.gen = _FakeServer._next
        self._alive = True
        self.opened: list[str] = []
        self.closed: list[str] = []

    def alive(self) -> bool:
        return self._alive

    def initialize(self) -> bool:
        return True

    def shutdown(self) -> None:
        self._alive = False

    def stderr_tail(self) -> str:
        return ""

    def notify(self, method, params):
        uri = params.get("textDocument", {}).get("uri", "")
        if method == "textDocument/didOpen":
            self.opened.append(uri)
        elif method == "textDocument/didClose":
            self.closed.append(uri)
        elif method == "textDocument/didChange":
            pass


def _stub_terminal_ok(monkeypatch):
    """Make _wait_terminal_progress report a clean terminal so _handle_locked
    can run without a real server."""
    monkeypatch.setattr(
        ic, "_wait_terminal_progress", lambda srv, uri: ("complete", [], {})
    )


def test_ensure_server_no_longer_falls_back_on_other_node_closure(
    tmp_path, monkeypatch
) -> None:
    """The fingerprint thrash is gone: _ensure_server only owns liveness and
    never returns a stale-prefix fallback just because a different node has a
    different import closure."""
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("import Tablet.Dep\n", encoding="utf-8")
    (tablet / "Dep.lean").write_text("-- v1\n", encoding="utf-8")

    monkeypatch.setattr(ic, "_LeanServer", _FakeServer)
    broker = ic._Broker(tmp_path)

    srv1, reason1 = broker._ensure_server("N")
    assert reason1 is None and srv1 is not None
    # A different node (different closure) must NOT trigger a restart/fallback.
    srv2, reason2 = broker._ensure_server("Dep")
    assert reason2 is None
    assert srv2.gen == srv1.gen, "server must be reused across different nodes"


def test_targeted_invalidation_reloads_only_changed_node(
    tmp_path, monkeypatch
) -> None:
    """Editing node A's import must invalidate A but preserve unrelated node
    C's warm state (ISSUE-3)."""
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "A.lean").write_text("import Tablet.Dep\n-- A\n", encoding="utf-8")
    (tablet / "Dep.lean").write_text("-- v1\n", encoding="utf-8")
    (tablet / "C.lean").write_text("-- C unrelated\n", encoding="utf-8")

    monkeypatch.setattr(ic, "_LeanServer", _FakeServer)
    _stub_terminal_ok(monkeypatch)
    broker = ic._Broker(tmp_path)

    # Open A and C (both elaborate cleanly).
    assert broker.handle_request({"node": "A"})["verdict"] == "ok"
    assert broker.handle_request({"node": "C"})["verdict"] == "ok"
    uri_a = ic._uri_for(tmp_path, "A")
    uri_c = ic._uri_for(tmp_path, "C")
    assert uri_a in broker._open_text and uri_c in broker._open_text
    srv = broker._srv

    # Change A's transitive import -> next check of A must fall back, A's warm
    # state is dropped, but C's warm state is UNTOUCHED.
    import time as _t
    _t.sleep(0.01)
    (tablet / "Dep.lean").write_text("-- v2 CHANGED\n", encoding="utf-8")
    os.utime(tablet / "Dep.lean", None)

    res = broker.handle_request({"node": "A"})
    assert res["verdict"] == "fallback" and "stale prefix" in res["reason"]
    # Same server (no full restart); A closed + dropped, C preserved.
    assert broker._srv is srv, "the whole server must NOT be restarted"
    assert uri_a in srv.closed, "A must be didClose'd (targeted reload)"
    assert uri_c not in srv.closed, "unrelated C must keep its warm state"
    assert uri_c in broker._open_text, "C's warm state must survive"


def test_inject_options_after_imports_and_offset_roundtrip() -> None:
    text = (
        "import Tablet.Dep\nimport Tablet.Dep2\n\n-- comment\n"
        "theorem t : True := trivial\n"
    )
    injected, insert_line, n = ic._inject_options(text)
    inj_lines = injected.split("\n")
    # The injected set_option lines sit AFTER the last import line.
    assert inj_lines[insert_line:insert_line + n] == list(ic._INJECTED_OPTIONS)
    assert inj_lines[insert_line - 1].startswith("import ")
    # A diagnostic line in the injected buffer maps back to the real file.
    # The theorem is on real line index 4; in the injected buffer it shifts
    # down by n. The unshift must recover the real line.
    real_theorem_line = text.split("\n").index("theorem t : True := trivial")
    inj_theorem_line = injected.split("\n").index("theorem t : True := trivial")
    assert inj_theorem_line == real_theorem_line + n
    assert (
        ic._unshift_diag_line(inj_theorem_line, insert_line, n)
        == real_theorem_line
    )
    # A diagnostic on an import line (above the injection) is unchanged.
    assert ic._unshift_diag_line(0, insert_line, n) == 0


def test_format_diag_applies_offset_for_target_only() -> None:
    repo, node = Path("/x"), "N"
    uri = ic._uri_for(repo, node)
    # insert_line=2 (after 2 imports), 3 injected lines; an error at injected
    # buffer line 6 (0-based) maps back to real line 3 (0-based) -> 1-based 4.
    line = ic._format_diag(
        repo, node,
        {"severity": ic._SEVERITY_ERROR, "message": "boom",
         "range": {"start": {"line": 6, "character": 0}}},
        uri=uri, inject_geometry=(2, 3),
    )
    assert line == "Tablet/N.lean:4:1: error: boom"
    # A foreign URI gets NO offset.
    foreign = ic._format_diag(
        repo, node,
        {"severity": ic._SEVERITY_ERROR, "message": "dep",
         "range": {"start": {"line": 6, "character": 0}}},
        uri="file:///other/Tablet/Dep.lean", inject_geometry=(2, 3),
    )
    assert foreign == "/other/Tablet/Dep.lean:7:1: error: dep"


def test_node_name_for_uri_roundtrip(tmp_path) -> None:
    (tmp_path / "Tablet").mkdir()
    uri = ic._uri_for(tmp_path, "Foo")
    assert ic._node_name_for_uri(tmp_path, uri) == "Foo"
    assert ic._node_name_for_uri(tmp_path, "file:///elsewhere/Bar.lean") is None


def test_prewarm_is_failure_open(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("-- ok\n", encoding="utf-8")
    # Broker unreachable -> prewarm must still exit 0 and NOT run lake build.
    monkeypatch.setattr(ic, "_broker_check", lambda repo, node: None)
    lake_calls = []
    monkeypatch.setattr(
        ic, "_lake_build_fallback",
        lambda *a, **k: lake_calls.append(1) or 99,
    )
    assert ic._run_prewarm(tmp_path, "N") == 0
    assert not lake_calls, "prewarm must never run lake build"
    # Missing node -> still exit 0.
    assert ic._run_prewarm(tmp_path, "DoesNotExist") == 0
    # Broker raising -> still exit 0.
    def _boom(repo, node):
        raise RuntimeError("kaboom")
    monkeypatch.setattr(ic, "_broker_check", _boom)
    assert ic._run_prewarm(tmp_path, "N") == 0


# --------------------------------------------------------------------------
# Giant-node skip heuristic + corrected server config
# --------------------------------------------------------------------------

def test_injected_options_do_not_disable_async() -> None:
    # The corrected config leaves elaboration ASYNC (no `Elab.async false`),
    # because async-OFF wedges a giant cold open. maxHeartbeats/maxRecDepth stay.
    joined = "\n".join(ic._INJECTED_OPTIONS)
    assert "Elab.async" not in joined
    assert "set_option maxHeartbeats 0" in ic._INJECTED_OPTIONS
    assert "set_option maxRecDepth 10000" in ic._INJECTED_OPTIONS


def test_server_env_does_not_pin_single_thread() -> None:
    # LEAN_NUM_THREADS=1 contributed to the wedge; threads stay at the Lean
    # default so the body-elaboration worker can spawn. Stack stays large.
    assert "LEAN_NUM_THREADS" not in ic._SERVER_ENV
    assert ic._SERVER_ENV.get("LEAN_STACK_SIZE_KB") == "2097152"


def test_is_giant_node_by_line_count(tmp_path) -> None:
    p = tmp_path / "N.lean"
    p.write_text("\n".join(["-- x"] * 5000) + "\n", encoding="utf-8")
    giant, reason = ic.is_giant_node(p, max_lines=3000)
    assert giant and "exceeds giant threshold" in reason
    # A normal node is not giant.
    small = tmp_path / "S.lean"
    small.write_text("theorem t : True := trivial\n", encoding="utf-8")
    assert ic.is_giant_node(small, max_lines=3000)[0] is False


def test_is_giant_node_by_heartbeat_ceiling(tmp_path) -> None:
    p = tmp_path / "N.lean"
    p.write_text(
        "import Tablet.Dep\nset_option maxHeartbeats 4_000_000\n"
        "theorem t : True := trivial\n",
        encoding="utf-8",
    )
    giant, reason = ic.is_giant_node(p, max_lines=0, heartbeat_ceiling=2_000_000)
    assert giant and "maxHeartbeats" in reason
    # Below the ceiling -> not giant.
    q = tmp_path / "M.lean"
    q.write_text("set_option maxHeartbeats 400000\n", encoding="utf-8")
    assert ic.is_giant_node(q, max_lines=0, heartbeat_ceiling=2_000_000)[0] is False


def test_run_incremental_skips_giant_node_to_lake(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("\n".join(["-- x"] * 5000) + "\n", encoding="utf-8")

    def _boom(*a, **k):
        raise AssertionError("a giant node must never reach a warm server")

    monkeypatch.setattr(ic, "_active_prewarm_check", _boom)
    monkeypatch.setattr(ic, "_broker_check", _boom)
    calls = _capture_fallback(monkeypatch)
    assert ic._run_incremental(tmp_path, "N") == 99
    assert "too large to warm" in calls["reason"]


def test_prewarm_skips_giant_node(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("\n".join(["-- x"] * 5000) + "\n", encoding="utf-8")

    def _boom(repo, node):
        raise AssertionError("a giant node must never be prewarmed")

    monkeypatch.setattr(ic, "_broker_check", _boom)
    assert ic._run_prewarm(tmp_path, "N") == 0


# --------------------------------------------------------------------------
# Overlay + goal-state support
#
# An overlay candidate must never be written to the agent's working tree,
# so it is elaborated as a didChange OVERLAY on the node's warm document.
# What these pin is that the overlay text is what gets elaborated, that a
# goal query rides along without forcing a second elaboration, and that a
# request without either field behaves exactly as it did before.
# --------------------------------------------------------------------------


class _GoalServer(_FakeServer):
    """_FakeServer plus a scripted reply to one position request."""

    def __init__(self, repo) -> None:
        super().__init__(repo)
        self.sent: list[str] = []
        self.positions: list[dict] = []
        self.changed: list[str] = []
        self._replies: list[dict] = []
        self._rid = 0
        self.reply = {"rendered": "h : P\n⊢ Q"}

    def notify(self, method, params):
        super().notify(method, params)
        text = params.get("textDocument", {}).get("text")
        if text is not None:
            self.sent.append(text)
        if method == "textDocument/didChange":
            self.changed.append(params["contentChanges"][0].get("text", ""))

    def request(self, method, params):
        self._rid += 1
        self.positions.append({"method": method, **params.get("position", {})})
        self._replies.append({"id": self._rid, "result": dict(self.reply)})
        return self._rid

    def drain(self, timeout):
        return self._replies.pop(0) if self._replies else None


def _overlay_repo(tmp_path):
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "N.lean").write_text("import Tablet.Dep\ndef a := sorry\n", encoding="utf-8")
    (tablet / "Dep.lean").write_text("-- dep\n", encoding="utf-8")
    return tmp_path


def test_overlay_text_is_elaborated_instead_of_the_disk_bytes(tmp_path, monkeypatch):
    repo = _overlay_repo(tmp_path)
    monkeypatch.setattr(ic, "_LeanServer", _GoalServer)
    _stub_terminal_ok(monkeypatch)
    broker = ic._Broker(repo)

    result = broker.handle_request(
        {"node": "N", "text": "import Tablet.Dep\ndef a := rfl\n"}
    )
    assert result["verdict"] == "ok"
    assert "def a := rfl" in broker._srv.sent[0]
    assert "sorry" not in broker._srv.sent[0]
    # The overlay is a buffer change only: disk is untouched.
    assert "sorry" in (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")


def test_goal_request_rides_back_on_the_verdict(tmp_path, monkeypatch):
    repo = _overlay_repo(tmp_path)
    monkeypatch.setattr(ic, "_LeanServer", _GoalServer)
    _stub_terminal_ok(monkeypatch)
    broker = ic._Broker(repo)

    result = broker.handle_request(
        {"node": "N", "text": "import Tablet.Dep\ndef a := sorry\n",
         "goal": {"line": 2, "column": 9}}
    )
    assert result["goal"] == "h : P\n⊢ Q"
    position = broker._srv.positions[0]
    assert position["method"] == "$/lean/plainGoal"
    # Line 2 of the real file sits below the injected option block.
    assert position["line"] == 1 + len(ic._INJECTED_OPTIONS)
    assert position["character"] == 9


def test_a_goal_on_an_unchanged_buffer_costs_no_re_elaboration(tmp_path, monkeypatch):
    repo = _overlay_repo(tmp_path)
    monkeypatch.setattr(ic, "_LeanServer", _GoalServer)
    _stub_terminal_ok(monkeypatch)
    broker = ic._Broker(repo)

    assert broker.handle_request({"node": "N"})["verdict"] == "ok"
    opens = len(broker._srv.sent)
    result = broker.handle_request({"node": "N", "goal": {"line": 2, "column": 9}})
    assert result["goal"] == "h : P\n⊢ Q"
    assert len(broker._srv.sent) == opens
    assert broker._srv.changed == []


def test_the_goal_never_enters_the_verdict_cache(tmp_path, monkeypatch):
    repo = _overlay_repo(tmp_path)
    monkeypatch.setattr(ic, "_LeanServer", _GoalServer)
    _stub_terminal_ok(monkeypatch)
    broker = ic._Broker(repo)

    broker.handle_request({"node": "N", "goal": {"line": 2, "column": 9}})
    assert broker.handle_request({"node": "N"}).get("goal") is None


def test_a_failed_goal_query_leaves_the_verdict_intact(tmp_path, monkeypatch):
    repo = _overlay_repo(tmp_path)
    monkeypatch.setattr(ic, "_LeanServer", _GoalServer)
    _stub_terminal_ok(monkeypatch)
    broker = ic._Broker(repo)
    broker.handle_request({"node": "N"})
    broker._srv.reply = {}  # server answers with nothing usable

    result = broker.handle_request({"node": "N", "goal": {"line": 2, "column": 9}})
    assert result["verdict"] == "ok"
    assert result["goal"] == ""


def test_a_malformed_goal_field_is_ignored(tmp_path, monkeypatch):
    repo = _overlay_repo(tmp_path)
    monkeypatch.setattr(ic, "_LeanServer", _GoalServer)
    _stub_terminal_ok(monkeypatch)
    broker = ic._Broker(repo)
    result = broker.handle_request({"node": "N", "goal": {"column": 3}})
    assert result["verdict"] == "ok"
    assert broker._srv.positions == []


def test_shift_line_into_buffer_is_the_inverse_of_unshift() -> None:
    for line0 in range(0, 12):
        shifted = ic._shift_line_into_buffer(line0, 2, 3)
        assert ic._unshift_diag_line(shifted, 2, 3) == line0


def test_broker_overlay_check_is_none_when_no_broker(tmp_path, monkeypatch):
    monkeypatch.setattr(ic, "_ensure_broker", lambda repo: False)
    assert ic.broker_overlay_check(tmp_path, "N", "text") is None


def test_broker_overlay_check_refuses_outside_the_sandbox(tmp_path, monkeypatch):
    """The broker starts a BARE `lake env lean --server`; started from a
    host shell against a live tablet that is the `.lake/packages` wipe."""
    monkeypatch.delenv("TMPDIR", raising=False)
    monkeypatch.setattr(ic, "_allow_unsandboxed", lambda: False)

    def _boom(_repo):  # pragma: no cover
        raise AssertionError("no broker may be started outside the sandbox")

    monkeypatch.setattr(ic, "_ensure_broker", _boom)
    assert ic.broker_overlay_check(tmp_path, "N", "text") is None


# ---------------------------------------------------------------------------
# Per-request heartbeat budget
#
# The injected `maxHeartbeats 0` is deliberately more permissive than
# `lake build` (the Tablet package sets only `autoImplicit false`, so a node
# builds at Lean's stock 200000 unless its own preamble says otherwise). A
# caller that is VERIFYING rather than advising needs the real build budget;
# it asks per request, and the default is untouched for everyone else.
# ---------------------------------------------------------------------------

_SRC = "import Tablet.Dep\n\ntheorem t : True := trivial\n"


def test_the_injected_default_stays_unbounded() -> None:
    assert ic._injected_options() == ic._INJECTED_OPTIONS
    assert ic._injected_options()[0] == "set_option maxHeartbeats 0"
    assert "set_option maxHeartbeats 0" in ic._inject_options(_SRC)[0]


def test_a_pinned_budget_replaces_only_the_heartbeat_option() -> None:
    pinned = ic._injected_options(200_000)
    assert "set_option maxHeartbeats 200000" in pinned
    assert "set_option maxHeartbeats 0" not in pinned
    # maxRecDepth and every other injected option survive untouched.
    assert set(pinned) - {"set_option maxHeartbeats 200000"} == set(
        ic._INJECTED_OPTIONS
    ) - {"set_option maxHeartbeats 0"}
    # Zero is Lean's own "unbounded" and is passed through as written.
    assert "set_option maxHeartbeats 0" in ic._injected_options(0)


def test_pinning_the_budget_does_not_move_the_diagnostic_geometry() -> None:
    """Callers cache (insert_line, num_injected) and unshift diagnostics with
    it; a pinned budget that changed the line count would silently mis-report
    every error position."""
    plain = ic._inject_options(_SRC)
    pinned = ic._inject_options(_SRC, max_heartbeats=200_000)
    assert plain[1:] == pinned[1:]
    assert plain[0] != pinned[0]


def test_the_injected_budget_sits_above_the_files_own_preamble() -> None:
    """So a node that declared it needs more keeps its declared budget here,
    exactly as under `lake build` — the pin is a default, not an override."""
    src = "import Tablet.Dep\n\nset_option maxHeartbeats 800000\n\ntheorem t : True := trivial\n"
    injected, _insert, _n = ic._inject_options(src, max_heartbeats=200_000)
    lines = injected.split("\n")
    assert lines.index("set_option maxHeartbeats 200000") < lines.index(
        "set_option maxHeartbeats 800000"
    )


def test_broker_request_carries_the_budget_only_when_asked(
    tmp_path, monkeypatch
) -> None:
    sent: dict = {}
    monkeypatch.setattr(ic, "_sandbox_markers", lambda: (True, ""))
    monkeypatch.setattr(ic, "_ensure_broker", lambda _r: True)

    class _Sock:
        def __enter__(self):
            return self

        def __exit__(self, *_a):
            return False

        def settimeout(self, _t):
            pass

        def connect(self, _p):
            pass

    monkeypatch.setattr(ic.socket, "socket", lambda *_a, **_k: _Sock())
    monkeypatch.setattr(ic, "_send_frame", lambda _s, req: sent.update(req))
    monkeypatch.setattr(ic, "_recv_frame", lambda _s: {"verdict": "ok"})

    ic.broker_overlay_check(tmp_path, "N", "text")
    assert "max_heartbeats" not in sent

    ic.broker_overlay_check(tmp_path, "N", "text", max_heartbeats=200_000)
    assert sent["max_heartbeats"] == 200_000


@pytest.mark.parametrize(
    "value,expected",
    [
        (200_000, 200_000),
        (0, 0),
        (None, None),
        (-1, None),  # nonsense degrades to the default, never to an error
        ("x", None),
    ],
)
def test_the_broker_parses_the_budget_off_the_request(
    tmp_path, monkeypatch, value, expected
) -> None:
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / "Tablet" / "N.lean").write_text(_SRC, encoding="utf-8")
    broker = ic._Broker(repo) if hasattr(ic, "_Broker") else None
    if broker is None:  # pragma: no cover — class renamed
        pytest.skip("broker class not exposed")
    seen: dict = {}

    def _spy(node_name, lean_path, *, overlay_text=None, goal_pos=None, max_heartbeats=None):
        seen["max_heartbeats"] = max_heartbeats
        return {"verdict": "ok"}

    monkeypatch.setattr(broker, "_handle_locked", _spy)
    req = {"method": "check", "node": "N"}
    if value is not None:
        req["max_heartbeats"] = value
    broker.handle_request(req)
    assert seen["max_heartbeats"] == expected

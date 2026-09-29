"""Unit tests for the supervisor-side active-node prewarm server + wiring.

Fast tests (no real ``lean --server``): they exercise the single-node warm
lifecycle, one-node content-keyed coherence, the worker preference order +
failure-open behavior, the false-early-complete fix (inherited from the reused
``_wait_terminal_progress`` latch), and the supervisor-hook gating/extraction.
The full real-LSP headline (prewarm -> fast first call) is the offline harness
``anp_validate.py``, not pytest.
"""
from __future__ import annotations

from pathlib import Path

import trellis.incremental_check as ic
from trellis.active_node_prewarm.config import ActiveNodePrewarmConfig
from trellis.active_node_prewarm import server as anp_server
from trellis.active_node_prewarm import supervisor_hook as hook

# Reuse the scripted-server helpers from the broker test module.
from tests.test_incremental_check import _ScriptedServer, _progress, _diag


# --------------------------------------------------------------------------
# false-early-complete fix (the latent preview_server bug)
# --------------------------------------------------------------------------

def test_initial_empty_processing_is_not_complete(monkeypatch) -> None:
    """An initial EMPTY fileProgress (processing == []) before ANY non-empty
    processing must NOT be treated as terminal. The reused
    ``_wait_terminal_progress`` requires a non-empty processing was seen first
    (the ``saw_processing`` latch). Here the only message is an empty-processing
    notification, then silence -> ambiguous (NOT a false-early complete)."""
    monkeypatch.setattr(ic, "_QUIET_TIMEOUT_SECS", 0.3)
    uri = ic._uri_for(Path("/x"), "N")
    srv = _ScriptedServer([_progress(uri, 0)], tail="silent")  # empty first
    status, _t, _b = ic._wait_terminal_progress(srv, uri)
    assert status == "ambiguous"  # NOT "complete"


def test_empty_then_real_processing_then_terminal_is_complete(monkeypatch) -> None:
    """A real elaboration that happens to emit an empty processing first still
    completes correctly once non-empty processing has been seen and drains."""
    uri = ic._uri_for(Path("/x"), "N")
    srv = _ScriptedServer([
        _progress(uri, 0),   # spurious empty first (must NOT terminate)
        _progress(uri, 2),   # real work begins
        _progress(uri, 0),   # genuine terminal (after non-empty)
    ])
    status, _t, _b = ic._wait_terminal_progress(srv, uri)
    assert status == "complete"


# --------------------------------------------------------------------------
# Single-node warm server: set_active / check / coherence (stubbed LSP)
# --------------------------------------------------------------------------

class _StubLeanServer:
    """A _LeanServer-shaped stub. didOpen/didClose recorded; the terminal wait
    is stubbed separately so we drive verdicts deterministically."""

    def __init__(self, repo, *, bwrap_role=None, burst_home=None) -> None:
        self._alive = True
        self.opened: list[str] = []
        self.closed: list[str] = []
        self.changed: list[str] = []
        self.pid = 12345

    def initialize(self) -> bool:
        return True

    def alive(self) -> bool:
        return self._alive

    def stderr_tail(self) -> str:
        return ""

    def shutdown(self) -> None:
        self._alive = False

    def notify(self, method, params):
        uri = params.get("textDocument", {}).get("uri", "")
        if method == "textDocument/didOpen":
            self.opened.append(uri)
        elif method == "textDocument/didClose":
            self.closed.append(uri)
        elif method == "textDocument/didChange":
            self.changed.append(uri)


def _make_repo(tmp_path: Path, node: str, body: str = "theorem N : True := trivial") -> Path:
    tablet = tmp_path / "Tablet"
    tablet.mkdir(parents=True, exist_ok=True)
    # The imported Dep MUST exist in the workspace mirror: a node's accepted
    # imports are present in the supervisor copy (only a freshly-created helper is
    # absent, exercised separately by the stale-mirror tests). Without this, the
    # server's missing-import pre-check would (correctly) fall back.
    (tablet / "Dep.lean").write_text(
        "theorem Dep : True := trivial\n", encoding="utf-8"
    )
    (tablet / f"{node}.lean").write_text(
        f"import Tablet.Dep\n{body}\n", encoding="utf-8"
    )
    return tmp_path


def _server(tmp_path, monkeypatch, *, verdict_seq):
    """Build a server with stubbed _LeanServer + a scripted terminal verdict
    sequence (one entry per elaboration: 'ok' | 'fail')."""
    monkeypatch.setattr(anp_server._ic, "_LeanServer", _StubLeanServer)
    seq = list(verdict_seq)

    def fake_wait(srv, uri):
        v = seq.pop(0)
        if v == "ok":
            return ("complete", [], {uri: []})
        if v == "fail":
            return ("complete", [], {uri: [
                {"severity": ic._SEVERITY_ERROR, "message": "boom",
                 "range": {"start": {"line": 5, "character": 0}}}]})
        if v == "sorry":
            # Elaborates clean but carries a PERMITTED open `sorry` (a warning,
            # not an error) — must classify as verdict ok with sorry info.
            return ("complete", [], {uri: [
                {"severity": ic._SEVERITY_WARNING,
                 "message": "declaration uses `sorry`",
                 "range": {"start": {"line": 5, "character": 0}}}]})
        return (v, [], {})  # 'ambiguous' / 'crashed'

    monkeypatch.setattr(anp_server._ic, "_wait_terminal_progress", fake_wait)
    # sandbox_role="" -> no bwrap wrapping (the _LeanServer is stubbed anyway);
    # the real-bwrap path is covered by the offline harness + the dedicated
    # _bwrap_lean_server_cmd test.
    cfg = ActiveNodePrewarmConfig(enabled=True, sandbox_role="")
    return anp_server.ActiveNodePrewarmServer(workspace=tmp_path, config=cfg)


def test_prewarm_then_fast_warm_check(tmp_path, monkeypatch) -> None:
    repo = _make_repo(tmp_path, "N")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok"])  # one elaboration only
    status = srv.set_active_node("N")
    assert status["warmed"] is True
    # The worker's first check re-serves the cached terminal verdict WITHOUT a
    # second elaboration (verdict_seq has only one entry; a second elaboration
    # would IndexError).
    result = srv.check("N")
    assert result["verdict"] == "ok"
    assert result["warm"] is True
    srv.shutdown()


def test_unchanged_active_node_carries_warm_across_calls(tmp_path, monkeypatch) -> None:
    _make_repo(tmp_path, "N")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok"])  # exactly one
    first = srv.set_active_node("N")
    assert first["reseeded"] is False or first["reseeded"] is True  # first seed
    # Calling set_active_node again with unchanged content is a no-op (no second
    # elaboration; the warm state carries across bursts).
    again = srv.set_active_node("N")
    assert again["reseeded"] is False
    assert again["reason"].startswith("active node unchanged")
    srv.shutdown()


def test_one_node_coherence_reseeds_on_content_change(tmp_path, monkeypatch) -> None:
    repo = _make_repo(tmp_path, "N")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok", "fail"])
    srv.set_active_node("N")
    assert srv.check("N")["verdict"] == "ok"
    # Mutate the accepted content; the supervisor re-declares the active node.
    (repo / "Tablet" / "N.lean").write_text(
        "import Tablet.Dep\ntheorem N : False := trivial\n", encoding="utf-8"
    )
    reseed = srv.set_active_node("N")
    assert reseed["reseeded"] is True
    # The re-seeded check must reflect the NEW content (fail), never a stale
    # green off the prior warm state.
    result = srv.check("N")
    assert result["verdict"] == "fail"
    srv.shutdown()


def test_prewarm_server_sorry_is_ok_with_sorry_info(tmp_path, monkeypatch) -> None:
    # The prewarm server mirrors lake build: a node that elaborates clean but
    # still has a PERMITTED open `sorry` returns verdict ok, never fail, with the
    # sorry location carried as info for the client to surface.
    _make_repo(tmp_path, "N")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["sorry"])
    srv.set_active_node("N")
    result = srv.check("N")
    assert result["verdict"] == "ok"
    assert result["sorry"] == [
        "Tablet/N.lean:4:1: warning: declaration uses `sorry`"
    ]
    srv.shutdown()


def test_check_overlay_text_sees_in_flight_edit(tmp_path, monkeypatch) -> None:
    """Gap-2 headline: the worker's IN-FLIGHT text (overlay_text) is elaborated
    even though the on-disk / accepted copy still passes. The server must return
    the overlay's verdict, never a stale green off the warm baseline, and must
    NOT touch its own disk copy."""
    repo = _make_repo(tmp_path, "N")
    disk = (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")
    # Baseline prewarm passes; the overlay (worker's edit) fails.
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok", "fail"])
    srv.set_active_node("N")
    edited = disk.replace("trivial", "by exact?  -- broken edit")
    result = srv.check("N", overlay_text=edited)
    assert result["verdict"] == "fail"   # the EDIT, not the warm baseline
    assert result["warm"] is False
    # The server's own workspace file is untouched (overlay is in-memory only).
    assert (repo / "Tablet" / "N.lean").read_text(encoding="utf-8") == disk
    srv.shutdown()


def test_check_overlay_matching_baseline_serves_warm(tmp_path, monkeypatch) -> None:
    """An overlay identical to the warm baseline re-serves the cached verdict
    without a second elaboration (verdict_seq has ONE entry)."""
    repo = _make_repo(tmp_path, "N")
    disk = (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok"])  # exactly one
    srv.set_active_node("N")
    result = srv.check("N", overlay_text=disk)  # same as baseline
    assert result["verdict"] == "ok"
    assert result["warm"] is True
    srv.shutdown()


def test_check_overlay_then_revert_passes(tmp_path, monkeypatch) -> None:
    """Edit introduces an error (fail), then reverting to the baseline text
    passes again — the warm baseline cache is preserved across an overlay."""
    repo = _make_repo(tmp_path, "N")
    disk = (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")
    # prewarm ok; broken overlay fail; revert re-serves the warm baseline (no
    # third elaboration needed: the baseline cache is content-keyed).
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok", "fail"])
    srv.set_active_node("N")
    broken = disk.replace("trivial", "by sorry_broken")
    assert srv.check("N", overlay_text=broken)["verdict"] == "fail"
    reverted = srv.check("N", overlay_text=disk)
    assert reverted["verdict"] == "ok"
    assert reverted["warm"] is True
    srv.shutdown()


def test_check_overlay_giant_text_falls_back(tmp_path, monkeypatch) -> None:
    """An overlay that grows the node past the giant threshold falls back even
    though the on-disk copy is small (giant classification on the EDIT)."""
    repo = _make_repo(tmp_path, "N")

    def _no_server(*a, **k):
        raise AssertionError("a giant overlay must never spawn a lean --server")

    monkeypatch.setattr(ic, "_LeanServer", _no_server)
    srv = anp_server.ActiveNodePrewarmServer(
        workspace=tmp_path,
        config=ActiveNodePrewarmConfig(enabled=True, giant_node_max_lines=10),
    )
    # Force N to be the active node without spawning a server: stub the giant
    # path is only hit in check(), so set the active-node field directly.
    srv._active_node = "N"
    giant_overlay = "\n".join(["-- x"] * 50) + "\n"
    result = srv.check("N", overlay_text=giant_overlay)
    assert result["verdict"] == "fallback"
    assert "too large to warm" in result["reason"]
    srv.shutdown()


def test_check_of_non_active_node_falls_back(tmp_path, monkeypatch) -> None:
    _make_repo(tmp_path, "N")
    _make_repo(tmp_path, "Other")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok"])
    srv.set_active_node("N")
    # A different node is NOT this server's job -> fallback (worker uses broker).
    result = srv.check("Other")
    assert result["verdict"] == "fallback"
    srv.shutdown()


def test_check_reseeds_when_disk_drifts_from_warm(tmp_path, monkeypatch) -> None:
    """If the active node's on-disk content drifts from the warm snapshot but
    set_active_node was not called, a check must re-seed (not serve stale)."""
    repo = _make_repo(tmp_path, "N")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok", "fail"])
    srv.set_active_node("N")
    (repo / "Tablet" / "N.lean").write_text(
        "import Tablet.Dep\ntheorem N : False := trivial\n", encoding="utf-8"
    )
    result = srv.check("N")  # drift -> re-seed -> fresh (fail) verdict
    assert result["verdict"] == "fail"
    assert result["warm"] is False
    srv.shutdown()


def test_ambiguous_elaboration_is_fallback_not_green(tmp_path, monkeypatch) -> None:
    _make_repo(tmp_path, "N")
    # First elaboration (prewarm) is ambiguous; the warm state is cleared, so
    # the subsequent check re-drives (second seq entry: also ambiguous ->
    # fallback). Never a green off an ambiguous elaboration.
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ambiguous", "ambiguous"])
    status = srv.set_active_node("N")
    assert status["warmed"] is False
    result = srv.check("N")
    assert result["verdict"] == "fallback"
    srv.shutdown()


# --------------------------------------------------------------------------
# Stale-mirror / new-helper imports: the recurring helper-creation bug
# --------------------------------------------------------------------------

def test_overlay_importing_new_helper_falls_back_no_elaboration(tmp_path, monkeypatch) -> None:
    """The headline bug: a worker creates a new helper this burst and imports it
    into the active node. The server's workspace mirror does NOT have the helper
    (it only syncs on acceptance), so elaboration would fail on a bad import.
    The pre-check must short-circuit to `fallback` BEFORE any elaboration (so the
    verdict_seq has ONLY the prewarm entry; a second elaboration would IndexError
    on the empty sequence)."""
    repo = _make_repo(tmp_path, "N")
    disk = (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok"])  # prewarm only
    srv.set_active_node("N")
    # The worker's in-flight edit imports a helper absent from the mirror.
    edited = "import Tablet.NewHelper\n" + disk
    result = srv.check("N", overlay_text=edited)
    assert result["verdict"] == "fallback"
    assert "NewHelper" in result["reason"]
    assert "in-burst broker" in result["reason"]
    srv.shutdown()


def test_overlay_missing_transitive_import_falls_back(tmp_path, monkeypatch) -> None:
    """A new helper that is itself present but pulls in a transitively-absent
    Tablet node is also caught (the closure follows present deps)."""
    repo = _make_repo(tmp_path, "N")
    disk = (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")
    # Helper EXISTS in the mirror but imports a node that does NOT.
    (repo / "Tablet" / "Helper.lean").write_text(
        "import Tablet.Missing\ntheorem Helper : True := trivial\n", encoding="utf-8"
    )
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok"])  # prewarm only
    srv.set_active_node("N")
    edited = "import Tablet.Helper\n" + disk
    result = srv.check("N", overlay_text=edited)
    assert result["verdict"] == "fallback"
    assert "Missing" in result["reason"]
    srv.shutdown()


def test_overlay_all_imports_present_still_elaborates(tmp_path, monkeypatch) -> None:
    """When every imported helper IS in the mirror, the pre-check passes and the
    overlay is elaborated normally (no spurious fallback)."""
    repo = _make_repo(tmp_path, "N")
    disk = (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")
    (repo / "Tablet" / "Helper.lean").write_text(
        "theorem Helper : True := trivial\n", encoding="utf-8"
    )
    # prewarm ok, then the overlay elaborates (its import resolves) -> fail.
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok", "fail"])
    srv.set_active_node("N")
    edited = "import Tablet.Helper\n" + disk.replace("trivial", "by exact?  -- broken")
    result = srv.check("N", overlay_text=edited)
    assert result["verdict"] == "fail"  # a real in-node error, not a fallback
    srv.shutdown()


def test_bad_import_diagnostic_reclassified_to_fallback(tmp_path, monkeypatch) -> None:
    """Defense-in-depth: a missing import that slipped past the pre-check (the
    elaboration surfaces an 'unknown import' error) is reclassified `fail` ->
    `fallback`, never serving the import error to the worker as a genuine
    failure."""
    repo = _make_repo(tmp_path, "N")
    disk = (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")
    monkeypatch.setattr(anp_server._ic, "_LeanServer", _StubLeanServer)

    def fake_wait(srv, uri):
        return ("complete", [], {uri: [
            {"severity": ic._SEVERITY_ERROR,
             "message": "unknown import Tablet.NewHelper",
             "range": {"start": {"line": 0, "character": 0}}}]})

    monkeypatch.setattr(anp_server._ic, "_wait_terminal_progress", fake_wait)
    cfg = ActiveNodePrewarmConfig(enabled=True, sandbox_role="")
    srv = anp_server.ActiveNodePrewarmServer(workspace=tmp_path, config=cfg)
    srv.set_active_node("N")
    # Force past the pre-check: an overlay whose imports all resolve on disk, so
    # the (simulated) bad-import diagnostic is the ONLY signal.
    result = srv.check("N", overlay_text=disk)
    assert result["verdict"] == "fallback"
    assert "import" in result["reason"]
    srv.shutdown()


def test_genuine_error_still_fails_not_reclassified(tmp_path, monkeypatch) -> None:
    """A genuine in-node error (NOT an import-resolution failure) keeps verdict
    `fail` with its diagnostics — the reclassification only catches bad-import
    diagnostics, never real mistakes."""
    repo = _make_repo(tmp_path, "N")
    disk = (repo / "Tablet" / "N.lean").read_text(encoding="utf-8")
    srv = _server(tmp_path, monkeypatch, verdict_seq=["ok", "fail"])  # 'fail' = "boom"
    srv.set_active_node("N")
    edited = disk.replace("trivial", "by exact?  -- broken edit")
    result = srv.check("N", overlay_text=edited)
    assert result["verdict"] == "fail"
    assert result["lines"]  # real diagnostics carried through
    srv.shutdown()


def test_client_new_import_chain_prewarm_fallback_to_broker(tmp_path, monkeypatch) -> None:
    """End-to-end client chain for the bug: the prewarm server returns the new
    `fallback` for a new-import overlay, and `_run_incremental` then consults the
    in-burst broker (which DOES see the new helper)."""
    repo = _make_node_file(tmp_path, "N")
    monkeypatch.setattr(
        ic, "_active_prewarm_check",
        lambda repo, node: {
            "verdict": "fallback",
            "reason": "workspace mirror missing import NewHelper; "
                      "deferring to in-burst broker",
        },
    )
    broker_calls = []

    def _broker(repo, node):
        broker_calls.append(node)
        return {"verdict": "ok"}

    monkeypatch.setattr(ic, "_broker_check", _broker)
    assert ic._run_incremental(repo, "N") == 0
    assert broker_calls == ["N"]  # the broker WAS consulted


# --------------------------------------------------------------------------
# Worker-side preference order + failure-open (incremental_check client)
# --------------------------------------------------------------------------

def _make_node_file(tmp_path, node):
    tablet = tmp_path / "Tablet"
    tablet.mkdir(parents=True, exist_ok=True)
    (tablet / f"{node}.lean").write_text("theorem N : True := trivial\n", encoding="utf-8")
    return tmp_path


def test_preference_active_prewarm_first(tmp_path, monkeypatch, capsys) -> None:
    """Step 1: when the active-node prewarm server returns ok, the broker is
    NEVER consulted."""
    repo = _make_node_file(tmp_path, "N")
    monkeypatch.setattr(ic, "_active_prewarm_check", lambda repo, node: {"verdict": "ok"})
    def _broker_must_not_run(repo, node):  # pragma: no cover
        raise AssertionError("broker must not be consulted when prewarm answers")
    monkeypatch.setattr(ic, "_broker_check", _broker_must_not_run)
    assert ic._run_incremental(repo, "N") == 0


def test_preference_falls_through_to_broker_on_prewarm_fallback(tmp_path, monkeypatch) -> None:
    """A prewarm 'fallback' (e.g. non-active node) drops to the in-burst broker."""
    repo = _make_node_file(tmp_path, "N")
    monkeypatch.setattr(ic, "_active_prewarm_check",
                        lambda repo, node: {"verdict": "fallback", "reason": "not active"})
    monkeypatch.setattr(ic, "_broker_check", lambda repo, node: {"verdict": "ok"})
    assert ic._run_incremental(repo, "N") == 0


def test_preference_falls_through_when_no_prewarm_server(tmp_path, monkeypatch) -> None:
    """No prewarm server (None) -> broker."""
    repo = _make_node_file(tmp_path, "N")
    monkeypatch.setattr(ic, "_active_prewarm_check", lambda repo, node: None)
    monkeypatch.setattr(ic, "_broker_check", lambda repo, node: {"verdict": "fail",
                        "lines": ["Tablet/N.lean:1:1: error: x"]})
    assert ic._run_incremental(repo, "N") == 1


def test_preference_broker_down_falls_to_lake(tmp_path, monkeypatch) -> None:
    """Broker down (None) -> lake build (step 3)."""
    repo = _make_node_file(tmp_path, "N")
    monkeypatch.setattr(ic, "_active_prewarm_check", lambda repo, node: None)
    monkeypatch.setattr(ic, "_broker_check", lambda repo, node: None)
    calls = []
    monkeypatch.setattr(ic, "_lake_build_fallback",
                        lambda repo, node, *, reason: calls.append(reason) or 0)
    assert ic._run_incremental(repo, "N") == 0
    assert calls and "broker unavailable" in calls[0]


def test_active_prewarm_check_inert_without_env(monkeypatch, tmp_path) -> None:
    """The client probe is inert (returns None) when the socket env is unset."""
    monkeypatch.delenv(ic._ACTIVE_PREWARM_SOCK_ENV, raising=False)
    assert ic._active_prewarm_check(tmp_path, "N") is None


def test_active_prewarm_check_inert_when_socket_missing(monkeypatch, tmp_path) -> None:
    monkeypatch.setenv(ic._ACTIVE_PREWARM_SOCK_ENV, str(tmp_path / "nope.sock"))
    assert ic._active_prewarm_check(tmp_path, "N") is None


# --------------------------------------------------------------------------
# Supervisor hook: gating + active-node extraction
# --------------------------------------------------------------------------

def test_hook_inert_when_disabled(tmp_path, monkeypatch) -> None:
    """No config / disabled -> no-op (returns None), never raises."""
    monkeypatch.setenv(hook.SERVER_SOCKET_ENV, str(tmp_path / "s.sock"))
    assert hook.maybe_prewarm_active_node(config_path=None, request={}) is None


def test_hook_inert_when_socket_unset(tmp_path, monkeypatch) -> None:
    cfg = tmp_path / "trellis.config.json"
    cfg.write_text('{"active_node_prewarm": {"enabled": true}}', encoding="utf-8")
    monkeypatch.delenv(hook.SERVER_SOCKET_ENV, raising=False)
    assert hook.maybe_prewarm_active_node(config_path=cfg, request={}) is None


def test_extract_active_node_top_level() -> None:
    assert hook._extract_active_node({"active_node": "Foo"}) == "Foo"


def test_extract_active_node_from_worker_context() -> None:
    req = {"request_summary": {"worker_context": {"active_node": "Bar"}}}
    assert hook._extract_active_node(req) == "Bar"


def test_extract_active_node_from_plan_steps() -> None:
    req = {"validation_execution_plan": {"steps": [
        {"kind": "scoped_tablet"},
        {"kind": "proof_easy_scope", "active_node": "Baz"},
    ]}}
    assert hook._extract_active_node(req) == "Baz"


def test_extract_active_node_none_when_absent() -> None:
    assert hook._extract_active_node({"authorized_nodes": ["A", "B"]}) is None


def test_export_socket_to_worker_env_inert_when_disabled(tmp_path, monkeypatch) -> None:
    env: dict = {}
    monkeypatch.setenv(hook.SERVER_SOCKET_ENV, str(tmp_path / "s.sock"))
    hook.export_socket_to_worker_env(env, config_path=None)
    assert hook.WORKER_SOCKET_ENV not in env


# --------------------------------------------------------------------------
# Giant-node skip config + server behavior
# --------------------------------------------------------------------------

def test_config_giant_knobs_default_and_parse(tmp_path) -> None:
    cfg = ActiveNodePrewarmConfig.from_mapping({})
    assert cfg.giant_node_max_lines == 3000
    assert cfg.giant_node_heartbeat_ceiling == 2_000_000
    cfg2 = ActiveNodePrewarmConfig.from_mapping(
        {"giant_node_max_lines": 500, "giant_node_heartbeat_ceiling": 9_000_000}
    )
    assert cfg2.giant_node_max_lines == 500
    assert cfg2.giant_node_heartbeat_ceiling == 9_000_000


def _giant_lean(tablet: Path, node: str, n: int = 5000) -> None:
    (tablet / f"{node}.lean").write_text(
        "\n".join(["-- x"] * n) + "\n", encoding="utf-8"
    )


def test_server_set_active_skips_giant_without_spawning(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    _giant_lean(tablet, "N")

    def _no_server(*a, **k):
        raise AssertionError("a giant node must never spawn a lean --server")

    monkeypatch.setattr(ic, "_LeanServer", _no_server)
    srv = anp_server.ActiveNodePrewarmServer(
        workspace=tmp_path, config=ActiveNodePrewarmConfig(enabled=True)
    )
    status = srv.set_active_node("N")
    assert status["warmed"] is False
    assert "too large to warm" in status["reason"]


def test_server_check_giant_returns_fallback(tmp_path, monkeypatch) -> None:
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    _giant_lean(tablet, "N")

    def _no_server(*a, **k):
        raise AssertionError("a giant node must never spawn a lean --server")

    monkeypatch.setattr(ic, "_LeanServer", _no_server)
    srv = anp_server.ActiveNodePrewarmServer(
        workspace=tmp_path, config=ActiveNodePrewarmConfig(enabled=True)
    )
    result = srv.check("N")
    assert result["verdict"] == "fallback"
    assert "too large to warm" in result["reason"]


# --------------------------------------------------------------------------
# Gap-1 wipe-safety: the server's lean --server is bwrap-confined so a
# .lake/packages re-clone hits EROFS (ro-bound source checkouts).
# --------------------------------------------------------------------------

def test_bwrap_lake_server_cmd_robinds_packages(tmp_path, monkeypatch) -> None:
    """The bwrap-confined `lean --server` command must NOT make the package
    SOURCE checkouts (`.lake/packages/<pkg>`) writable — only the build/config
    outputs. A re-clone that rewrites the checkout therefore hits EROFS and
    cannot wipe `.lake/packages`. We assert the exact bind shape produced by
    wrap_command(role="lake_compiler")."""
    import shutil as _shutil
    if _shutil.which("bwrap") is None:
        import pytest
        pytest.skip("bwrap not installed")

    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    pkg = repo / ".lake" / "packages" / "mathlib"
    (pkg / ".lake" / "build").mkdir(parents=True)
    (pkg / ".git").mkdir(parents=True)  # the source checkout we must protect
    home = tmp_path / "home"
    home.mkdir()

    cmd, env = anp_server._bwrap_lean_server_cmd(
        repo, bwrap_role="lake_compiler", burst_home=home
    )
    assert cmd[0] == "bwrap"
    assert cmd[-4:] == ["lake", "env", "lean", "--server"]
    # The whole repo is ro-bound; only the narrow lake_compiler allowlist is
    # rewritable. Reconstruct the (flag, src, dst) bind triples.
    def _binds(flag):
        out = []
        i = 0
        while i < len(cmd):
            if cmd[i] == flag:
                out.append(cmd[i + 1])
                i += 3
            else:
                i += 1
        return out

    writable = {Path(p).resolve() for p in _binds("--bind")}
    # The package BUILD dir is writable (lake needs it)...
    assert (pkg / ".lake" / "build").resolve() in writable
    # ...but the package SOURCE checkout root is NOT writable.
    assert pkg.resolve() not in writable
    # ...and neither is `.lake/packages` itself.
    assert (repo / ".lake" / "packages").resolve() not in writable
    # The repo as a whole is ro-bound (read-only), confirming the source tree
    # is mounted read-only.
    ro = {Path(p).resolve() for p in _binds("--ro-bind")}
    assert repo.resolve() in ro


# --------------------------------------------------------------------------
# Worker-burst env export (the relaunch wiring)
# --------------------------------------------------------------------------

def test_export_worker_burst_env_inert_when_disabled(tmp_path) -> None:
    env: dict = {}
    hook.export_worker_burst_env(env, config_path=None, request={"active_node": "Foo"})
    assert env == {}


def test_export_worker_burst_env_sets_prewarm_when_enabled(tmp_path, monkeypatch) -> None:
    cfg = tmp_path / "trellis.config.json"
    cfg.write_text(
        '{"active_node_prewarm": {"enabled": true, "giant_node_max_lines": 1234}}',
        encoding="utf-8",
    )
    # No server socket exported -> the socket var is absent, but the in-burst
    # prewarm flag + node + giant thresholds are still set.
    monkeypatch.delenv(hook.SERVER_SOCKET_ENV, raising=False)
    env: dict = {}
    hook.export_worker_burst_env(
        env, config_path=cfg, request={"active_node": "Foo"}
    )
    assert env[hook.WORKER_PREWARM_FLAG_ENV] == "1"
    assert env[hook.WORKER_PREWARM_NODE_ENV] == "Foo"
    assert env[hook.WORKER_GIANT_MAX_LINES_ENV] == "1234"
    assert hook.WORKER_GIANT_HEARTBEAT_ENV in env
    assert hook.WORKER_SOCKET_ENV not in env


def test_export_worker_burst_env_no_node_skips_prewarm_flag(tmp_path) -> None:
    cfg = tmp_path / "trellis.config.json"
    cfg.write_text('{"active_node_prewarm": {"enabled": true}}', encoding="utf-8")
    env: dict = {}
    hook.export_worker_burst_env(env, config_path=cfg, request={})
    # No determinable active node -> no in-burst prewarm flag, but giant
    # thresholds (which do not need a node) are still exported.
    assert hook.WORKER_PREWARM_FLAG_ENV not in env
    assert hook.WORKER_GIANT_MAX_LINES_ENV in env

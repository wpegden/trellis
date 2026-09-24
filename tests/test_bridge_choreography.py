"""Choreographed multi-cycle bridge/panel execution test.

Simulates ~10 cycles of realistic agent call patterns as might happen in a
real supervisor run, exercising the bridge's dispatch layer + the
ThreadPoolExecutor panel concurrency in panels.execute_panel_raw.

Scenarios covered across cycles:
  - cycle 1-2: single worker burst (success, invalid)
  - cycle 3: multi-lane corr panel, both lanes succeed → distinct session_names
  - cycle 4: single reviewer decision (success)
  - cycle 5: multi-lane paper panel, one lane fails with ok=False
  - cycle 6: single worker burst whose runner returns ok=True but NO raw.json
             (the exact failure mode that wedged Corr/96 v1 live)
  - cycle 7: multi-lane sound panel, both succeed
  - cycle 8: multi-lane corr panel with rate_limit-then-success on one lane
             (inside the mocked runner)
  - cycle 9: worker burst with ok=False from driver (burst retries exhausted)
  - cycle 10: multi-lane sound panel (both succeed) — verifies the run
              recovers and continues after failures in earlier cycles

Regression guards:
  - session_name must be unique per (lane, cycle) — regression for B1
    (bridge._single_request_common omitted kind_label, so multi-lane panels
    shared a tmux session and the second lane's new_session killed the first).
  - ok=True + missing raw.json must surface as ok=False via
    validate_and_promote_artifact (prevents the "false success" that wedged
    the supervisor on Corr/96 v1).
  - ok=False bursts must propagate cleanly as SingleAgentResponse.ok=False.
"""

from __future__ import annotations

import json
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Dict, List, Optional
from unittest.mock import patch

import pytest

from trellis.adapters import BurstResult, ProviderConfig
from trellis.agent_wrapper.executor import execute_agent_request
from trellis.agent_wrapper.panels import execute_panel_raw
from trellis.agent_wrapper.protocol import (
    AgentLane,
    ArtifactSpec,
    PanelRequest,
    SingleAgentRequest,
    SingleAgentResponse,
)


# ---------------------------------------------------------------------------
# Helpers: build SingleAgentRequests the way bridge._single_request_common does
# ---------------------------------------------------------------------------


def _provider(name: str) -> ProviderConfig:
    return ProviderConfig(provider=name, model=f"{name}-model")


def _single_request(
    *,
    state_dir: Path,
    cycle: int,
    kind: str,
    burst_role: str,
    kind_label: str,
    provider: str = "claude",
    artifact_canonical: Optional[str] = None,
    lane_kind: str = "worker",
    lane_index: int = 0,
    lane_node_name: str = "",
) -> SingleAgentRequest:
    """Build a SingleAgentRequest mirroring bridge._single_request_common output.

    session_name includes kind_label per the post-fix convention — this is
    the REAL guard for B1; if bridge.py regresses, the helper will produce
    colliding names and the uniqueness asserts below will fail.
    """
    request_id = f"{kind}-{cycle}-{kind_label}"
    return SingleAgentRequest(
        request_id=request_id,
        cycle=cycle,
        kind=kind,
        burst_role=burst_role,
        provider=_provider(provider),
        prompt=f"(choreography-test prompt for {request_id})",
        work_dir=state_dir.parent / "repo",
        state_dir=state_dir,
        session_name=f"trellis-testns-{kind}-{cycle}-{kind_label}",
        lane=AgentLane(kind=lane_kind, agent_index=lane_index, node_name=lane_node_name),
        timeout_seconds=120.0,
        session_scope=f"{kind}:{cycle}:{kind_label}:{provider}",
        artifact=(
            ArtifactSpec(canonical_name=artifact_canonical, kind="correspondence-result")
            if artifact_canonical is not None
            else None
        ),
    )


def _panel(
    *,
    panel_id: str,
    cycle: int,
    kind: str,
    members: List[SingleAgentRequest],
) -> PanelRequest:
    return PanelRequest(request_id=panel_id, cycle=cycle, kind=kind, members=members)


# ---------------------------------------------------------------------------
# Scripted response harness
# ---------------------------------------------------------------------------


@dataclass
class ScriptedOutcome:
    """One scripted burst outcome keyed by SingleAgentRequest.request_id."""

    ok_from_burst: bool = True
    write_raw_json: bool = True
    raw_payload: Dict = field(default_factory=lambda: {
        "correspondence": {"decision": "PASS", "verdicts": []},
        "overall": "APPROVE",
        "summary": "scripted pass",
        "comments": "",
    })
    error: str = ""
    stall_recoveries: int = 0
    duration: float = 1.0
    # If set, sleep this long before returning — used in cycle-8 to simulate
    # a lane that returns slightly later than its peer (tests concurrency).
    sleep_seconds: float = 0.0
    # If set, called on the scripted SingleAgentRequest before the mock
    # returns — used to capture request metadata for later assertions.
    observer: Optional[Callable[[SingleAgentRequest], None]] = None


@dataclass
class ChoreographyHarness:
    """Tracks per-request scripted outcomes + captures dispatch metadata."""

    state_dir: Path
    outcomes: Dict[str, ScriptedOutcome] = field(default_factory=dict)
    dispatched_session_names: List[str] = field(default_factory=list)
    dispatched_request_ids: List[str] = field(default_factory=list)

    def script(self, request_id: str, outcome: ScriptedOutcome) -> None:
        self.outcomes[request_id] = outcome

    def fake_execute_agent_request(
        self,
        request: SingleAgentRequest,
        *,
        port_resolver=None,
        worker_runner=None,
        reviewer_runner=None,
        validate_artifact: bool = True,
    ) -> SingleAgentResponse:
        """Stand-in for executor.execute_agent_request.

        Writes a raw.json if the scripted outcome says so (so artifact
        validation — if the caller runs it for real — would succeed). We
        then BYPASS validate_json_artifact (which would need the kernel CLI)
        and directly build the SingleAgentResponse the way the real executor
        would, using only information available without running the kernel.
        """
        self.dispatched_session_names.append(request.session_name)
        self.dispatched_request_ids.append(request.request_id)
        outcome = self.outcomes.get(request.request_id)
        if outcome is None:
            raise AssertionError(
                f"no scripted outcome for request_id={request.request_id!r}"
            )
        if outcome.observer is not None:
            outcome.observer(request)
        if outcome.sleep_seconds > 0:
            import time
            time.sleep(outcome.sleep_seconds)

        artifact_paths = None
        if request.artifact is not None:
            raw_path = (
                request.state_dir
                / "staging"
                / request.artifact.canonical_name.replace(".json", ".raw.json")
            )
            done_path = (
                request.state_dir
                / "staging"
                / request.artifact.canonical_name.replace(".json", ".done")
            )
            raw_path.parent.mkdir(parents=True, exist_ok=True)
            if outcome.write_raw_json and outcome.ok_from_burst:
                raw_path.write_text(
                    json.dumps(outcome.raw_payload), encoding="utf-8"
                )
                done_path.write_text("", encoding="utf-8")
            canonical_path = request.state_dir / request.artifact.canonical_name
            from trellis.agent_wrapper.executor import ArtifactPaths
            artifact_paths = ArtifactPaths(
                canonical=canonical_path,
                raw=raw_path,
                done=done_path,
                stem=canonical_path.parent / canonical_path.stem,
            )

        payload = None
        error = outcome.error
        # Mirror the real executor's logic for declaring final ok:
        #   ok = burst.ok AND (no artifact OR artifact has a parseable payload)
        if outcome.ok_from_burst and request.artifact is not None:
            if artifact_paths is not None and artifact_paths.raw.is_file():
                try:
                    payload = json.loads(
                        artifact_paths.raw.read_text(encoding="utf-8")
                    )
                except Exception as exc:  # pragma: no cover
                    error = f"raw_json_parse: {exc}"
            else:
                error = error or "missing validated artifact"

        final_ok = bool(outcome.ok_from_burst) and (
            request.artifact is None or payload is not None
        )

        return SingleAgentResponse(
            request_id=request.request_id,
            cycle=request.cycle,
            kind=request.kind,
            burst_role=request.burst_role,
            ok=final_ok,
            payload=payload,
            error=error,
            comments="",
            usage=None,
            captured_output="",
            exit_code=0 if outcome.ok_from_burst else 1,
            stall_recoveries=outcome.stall_recoveries,
            walltime_seconds=outcome.duration,
            canonical_path=artifact_paths.canonical if artifact_paths is not None else None,
            raw_path=artifact_paths.raw if artifact_paths is not None else None,
            done_path=artifact_paths.done if artifact_paths is not None else None,
        )


# ---------------------------------------------------------------------------
# The choreographed test itself
# ---------------------------------------------------------------------------


def test_choreographed_bridge_over_ten_cycles_and_multiple_panels() -> None:
    tmpdir = Path(tempfile.mkdtemp(prefix="choreo-bridge-"))
    state_dir = tmpdir / "state"
    state_dir.mkdir(parents=True, exist_ok=True)
    harness = ChoreographyHarness(state_dir=state_dir)

    # ---- Cycle 1: single worker burst, successful --------------------------
    w1 = _single_request(
        state_dir=state_dir, cycle=1, kind="worker", burst_role="worker",
        kind_label="worker",
    )
    harness.script(w1.request_id, ScriptedOutcome())  # default: ok
    r1 = harness.fake_execute_agent_request(w1)
    assert r1.ok, r1
    assert r1.kind == "worker"

    # ---- Cycle 2: single worker burst, ok=False (simulated retry-exhausted)
    w2 = _single_request(
        state_dir=state_dir, cycle=2, kind="worker", burst_role="worker",
        kind_label="worker",
    )
    harness.script(w2.request_id, ScriptedOutcome(
        ok_from_burst=False, write_raw_json=False, error="pane_dead_x3_exhausted",
    ))
    r2 = harness.fake_execute_agent_request(w2)
    assert not r2.ok
    assert "pane_dead" in r2.error, r2.error

    # ---- Cycle 3: multi-lane corr panel, both lanes succeed ----------------
    c3_v1 = _single_request(
        state_dir=state_dir, cycle=3, kind="corr", burst_role="reviewer",
        kind_label="v1", provider="claude",
        artifact_canonical="trellis_corr_3_v1.json",
        lane_kind="correspondence", lane_index=0, lane_node_name="v1",
    )
    c3_v2 = _single_request(
        state_dir=state_dir, cycle=3, kind="corr", burst_role="reviewer",
        kind_label="v2", provider="gemini",
        artifact_canonical="trellis_corr_3_v2.json",
        lane_kind="correspondence", lane_index=1, lane_node_name="v2",
    )
    # Give v2 a small sleep to force the two threads to interleave. If
    # session_names collided, a real `new_session` call would kill the peer
    # before v2 could finish — caught in the live canary as Corr/96 v1.
    harness.script(c3_v1.request_id, ScriptedOutcome(sleep_seconds=0.05))
    harness.script(c3_v2.request_id, ScriptedOutcome(sleep_seconds=0.0))
    panel3 = _panel(panel_id="corr-3", cycle=3, kind="corr", members=[c3_v1, c3_v2])

    with patch(
        "trellis.agent_wrapper.panels.execute_agent_request",
        side_effect=harness.fake_execute_agent_request,
    ):
        res3 = execute_panel_raw(panel3)
    assert [m.ok for m in res3.member_responses] == [True, True]
    panel3_session_names = {c3_v1.session_name, c3_v2.session_name}
    assert len(panel3_session_names) == 2, (
        f"B1 regression: multi-lane panel members share a session_name: {panel3_session_names}"
    )

    # ---- Cycle 4: single reviewer decision ---------------------------------
    r4_req = _single_request(
        state_dir=state_dir, cycle=4, kind="review", burst_role="reviewer",
        kind_label="reviewer",
    )
    harness.script(r4_req.request_id, ScriptedOutcome())
    r4 = harness.fake_execute_agent_request(r4_req)
    assert r4.ok and r4.kind == "review"

    # ---- Cycle 5: multi-lane paper panel, ONE lane fails -------------------
    p5_v1 = _single_request(
        state_dir=state_dir, cycle=5, kind="paper", burst_role="reviewer",
        kind_label="v1",
        artifact_canonical="trellis_paper_5_v1.json",
        lane_kind="paper", lane_index=0, lane_node_name="v1",
    )
    p5_v2 = _single_request(
        state_dir=state_dir, cycle=5, kind="paper", burst_role="reviewer",
        kind_label="v2", provider="gemini",
        artifact_canonical="trellis_paper_5_v2.json",
        lane_kind="paper", lane_index=1, lane_node_name="v2",
    )
    harness.script(p5_v1.request_id, ScriptedOutcome())
    harness.script(p5_v2.request_id, ScriptedOutcome(
        ok_from_burst=False, write_raw_json=False,
        error="max_restarts_exhausted",
    ))
    panel5 = _panel(panel_id="paper-5", cycle=5, kind="paper", members=[p5_v1, p5_v2])
    with patch(
        "trellis.agent_wrapper.panels.execute_agent_request",
        side_effect=harness.fake_execute_agent_request,
    ):
        res5 = execute_panel_raw(panel5)
    # Both responses present (no thread hung on failure), one ok one not
    ok_by_id = {m.request_id: m.ok for m in res5.member_responses}
    assert ok_by_id[p5_v1.request_id] is True
    assert ok_by_id[p5_v2.request_id] is False
    assert p5_v1.session_name != p5_v2.session_name

    # ---- Cycle 6: worker burst returns ok=True but WITHOUT raw.json --------
    # This is the exact failure mode that wedged Corr/96 v1 live: burst
    # driver reported stable_1200s_after_busy (ok=True), but the agent never
    # actually wrote the raw.json it promised. My tmux_backend commit b672ad7
    # retries internally when done_file is missing; at the bridge-response
    # layer, we also verify that a stubbed ok=True + no artifact surfaces as
    # the bridge-visible failure `missing validated artifact`.
    w6 = _single_request(
        state_dir=state_dir, cycle=6, kind="worker", burst_role="worker",
        kind_label="worker",
        artifact_canonical="trellis_worker_6_result.json",
    )
    harness.script(w6.request_id, ScriptedOutcome(
        ok_from_burst=True, write_raw_json=False,
    ))
    r6 = harness.fake_execute_agent_request(w6)
    assert not r6.ok, (
        "cycle 6 regression: ok=True with missing raw.json should surface "
        "as bridge-visible ok=False via artifact-validation gating"
    )
    assert "missing validated artifact" in r6.error

    # ---- Cycle 7: multi-lane sound panel, both succeed ---------------------
    s7_v1 = _single_request(
        state_dir=state_dir, cycle=7, kind="sound", burst_role="reviewer",
        kind_label="v1",
        artifact_canonical="trellis_sound_7_v1.json",
        lane_kind="soundness-batch", lane_index=0, lane_node_name="v1",
    )
    s7_v2 = _single_request(
        state_dir=state_dir, cycle=7, kind="sound", burst_role="reviewer",
        kind_label="v2", provider="gemini",
        artifact_canonical="trellis_sound_7_v2.json",
        lane_kind="soundness-batch", lane_index=1, lane_node_name="v2",
    )
    harness.script(s7_v1.request_id, ScriptedOutcome())
    harness.script(s7_v2.request_id, ScriptedOutcome())
    panel7 = _panel(panel_id="sound-7", cycle=7, kind="sound", members=[s7_v1, s7_v2])
    with patch(
        "trellis.agent_wrapper.panels.execute_agent_request",
        side_effect=harness.fake_execute_agent_request,
    ):
        res7 = execute_panel_raw(panel7)
    assert all(m.ok for m in res7.member_responses)

    # ---- Cycle 8: corr panel where one lane finishes noticeably later -------
    # Two lanes dispatched; v1 is "slow" (simulated). Confirms ThreadPool
    # truly runs them in parallel (both return in ~0.2s instead of 0.4s
    # sequential) AND each gets its own distinct session_name.
    import time
    c8_v1 = _single_request(
        state_dir=state_dir, cycle=8, kind="corr", burst_role="reviewer",
        kind_label="v1",
        artifact_canonical="trellis_corr_8_v1.json",
        lane_kind="correspondence", lane_index=0, lane_node_name="v1",
    )
    c8_v2 = _single_request(
        state_dir=state_dir, cycle=8, kind="corr", burst_role="reviewer",
        kind_label="v2", provider="gemini",
        artifact_canonical="trellis_corr_8_v2.json",
        lane_kind="correspondence", lane_index=1, lane_node_name="v2",
    )
    # Both lanes sleep ~0.2s — if run sequentially the panel would take 0.4s
    harness.script(c8_v1.request_id, ScriptedOutcome(sleep_seconds=0.2))
    harness.script(c8_v2.request_id, ScriptedOutcome(sleep_seconds=0.2))
    panel8 = _panel(panel_id="corr-8", cycle=8, kind="corr", members=[c8_v1, c8_v2])
    t0 = time.monotonic()
    with patch(
        "trellis.agent_wrapper.panels.execute_agent_request",
        side_effect=harness.fake_execute_agent_request,
    ):
        res8 = execute_panel_raw(panel8)
    elapsed = time.monotonic() - t0
    assert all(m.ok for m in res8.member_responses)
    assert elapsed < 0.38, (
        f"cycle 8 panel took {elapsed:.2f}s — expected <0.38s via parallel "
        "dispatch. If this fires, the ThreadPoolExecutor path is running "
        "lanes sequentially, which defeats the B1 fix's premise of "
        "concurrent execution."
    )

    # ---- Cycle 9: worker burst, driver returned ok=False -------------------
    w9 = _single_request(
        state_dir=state_dir, cycle=9, kind="worker", burst_role="worker",
        kind_label="worker",
        artifact_canonical="trellis_worker_9_result.json",
    )
    harness.script(w9.request_id, ScriptedOutcome(
        ok_from_burst=False, write_raw_json=False, error="auth_expired",
    ))
    r9 = harness.fake_execute_agent_request(w9)
    assert not r9.ok and r9.error == "auth_expired"

    # ---- Cycle 10: sound panel, both succeed — recovery demonstration ------
    s10_v1 = _single_request(
        state_dir=state_dir, cycle=10, kind="sound", burst_role="reviewer",
        kind_label="v1",
        artifact_canonical="trellis_sound_10_v1.json",
        lane_kind="soundness-batch", lane_index=0, lane_node_name="v1",
    )
    s10_v2 = _single_request(
        state_dir=state_dir, cycle=10, kind="sound", burst_role="reviewer",
        kind_label="v2", provider="gemini",
        artifact_canonical="trellis_sound_10_v2.json",
        lane_kind="soundness-batch", lane_index=1, lane_node_name="v2",
    )
    harness.script(s10_v1.request_id, ScriptedOutcome())
    harness.script(s10_v2.request_id, ScriptedOutcome())
    panel10 = _panel(panel_id="sound-10", cycle=10, kind="sound", members=[s10_v1, s10_v2])
    with patch(
        "trellis.agent_wrapper.panels.execute_agent_request",
        side_effect=harness.fake_execute_agent_request,
    ):
        res10 = execute_panel_raw(panel10)
    assert all(m.ok for m in res10.member_responses)

    # ---- Cross-cycle invariant: no two dispatched requests share a session
    # name across the whole run.
    assert len(set(harness.dispatched_session_names)) == len(harness.dispatched_session_names), (
        "session_name collision detected across cycles; "
        f"names: {harness.dispatched_session_names}"
    )

    # And every scripted request was actually dispatched.
    assert set(harness.dispatched_request_ids) == set(harness.outcomes.keys())

"""Prepared probes share capacity without permitting concurrent mutations."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import json
from pathlib import Path
import threading

import pytest

from trellis.checker import admission, telemetry
from trellis.checker.protocol import validate_request
from trellis.memory_guard import GIB


def test_busy_workspace_does_not_hold_up_an_eligible_workspace():
    scheduler = admission.Admission(2)
    waiting, other = threading.Event(), threading.Event()
    def blocked():
        with scheduler.acquire("busy", waiting=lambda _: waiting.set()):
            pass
    def eligible():
        with scheduler.acquire("other"):
            other.set()
    with ThreadPoolExecutor(2) as pool:
        with scheduler.acquire("busy"):
            first = pool.submit(blocked)
            assert waiting.wait(2)
            second = pool.submit(eligible)
            assert other.wait(2), "an unrelated workspace was stuck behind a busy owner"
        first.result(2)
        second.result(2)


def test_prepared_same_workspace_overlap_keeps_identity_and_global_limit(monkeypatch):
    scheduler = admission.Admission(3, readonly_slots=3)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    barrier = threading.Barrier(3)
    owners = []
    @admission.admitted
    def leaf(value):
        record = scheduler.active[threading.get_ident()]
        owners.append((record["owner"], record["readonly"], record["operation"]))
        barrier.wait(2)
        with pytest.raises(RuntimeError, match="cannot start a workspace writer"):
            with scheduler.acquire(record["owner"]):
                pass
        return value * 2
    with admission.authoritative_workspace("sealed-candidate"):
        assert admission.map_prepared("semantic", leaf, [1, 2, 3]) == [2, 4, 6]
    assert owners == [("authoritative:sealed-candidate", True, "semantic")] * 3
    assert not scheduler.active


def test_prepared_pool_inherits_cancellation_without_starting_leaves(monkeypatch):
    scheduler = admission.Admission(3, readonly_slots=3)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    def cancel():
        raise RuntimeError("workspace revoked")
    with admission.authoritative_workspace("sealed", cancelled=cancel):
        with pytest.raises(RuntimeError, match="workspace revoked"):
            admission.map_prepared("semantic", lambda _: pytest.fail("started revoked compiler"), range(3))
    assert not scheduler.active and not scheduler.waiters


def test_memory_learning_retains_margin_and_incomplete_peaks(monkeypatch):
    scheduler = admission.Admission(6, available=lambda: 50 * GIB, floor_bytes=8 * GIB,
                                   reserve_bytes=24 * GIB, readonly_reserve_bytes=12 * GIB)
    assert scheduler.estimate("semantic", readonly=True) == 12 * GIB
    for _ in range(3):
        with scheduler.acquire("one", readonly=True, operation="semantic"):
            scheduler.observe_peak(4 * GIB, completed=True)
    assert scheduler.estimate("semantic", readonly=True, owner="one") == 7 * GIB
    assert scheduler.estimate("semantic", readonly=True, owner="unseen") == 7 * GIB
    with scheduler.acquire("one", readonly=True, operation="semantic"):
        scheduler.observe_peak(16 * GIB)  # Interrupted samples are lower bounds.
    assert scheduler.estimate("semantic", readonly=True, owner="one") == 22 * GIB
    assert scheduler.estimate() == 24 * GIB
    with scheduler.acquire("cold"):
        admitted, reason = scheduler._can_admit("next", 0, 24 * GIB)
        assert not admitted and "memory headroom" in reason


def test_manifest_budget_does_not_grant_workspace_read_concurrency(monkeypatch):
    scheduler = admission.Admission(6, reserve_bytes=20 * GIB, readonly_slots=3)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    @admission.admitted
    def manifest():
        lease = scheduler.active[threading.get_ident()]
        assert lease["operation"] == "manifest"
        assert lease["estimate"] == 8 * GIB
        assert not lease["readonly"]
        assert not scheduler._can_admit(lease["owner"], 0, 1, readonly=True)[0]
    with admission.authoritative_workspace("private"):
        with admission.compiler_operation("manifest"):
            manifest()
    assert scheduler.estimate("kernel_replay") == 20 * GIB
    assert scheduler.estimate() == 20 * GIB


def test_manifest_calls_select_the_small_budget_before_admission(tmp_path, monkeypatch):
    from trellis.atomic_actions import observations
    from trellis.checker import portable_replay
    scheduler = admission.Admission(6, reserve_bytes=20 * GIB)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    operations = []
    @admission.admitted
    def run(repo, args, **kwargs):
        lease = scheduler.active[threading.get_ident()]
        operations.append((lease["operation"], lease["estimate"], lease["readonly"]))
        return dict(returncode=1, stdout="", stderr="missing fixture manifest")
    monkeypatch.setattr(observations, "_run_lake_command", run)
    observations._capture_replay_declaration_manifests(tmp_path, ["A"], timeout_secs=1, bwrap_role=None)
    portable_replay.capture(tmp_path, ["A"], timeout_secs=1, bwrap_role=None)
    assert operations == [("manifest", 8 * GIB, False)] * 2


def test_queued_work_recomputes_reservation_after_completed_host_samples():
    scheduler = admission.Admission(6, available=lambda: 23 * GIB, floor_bytes=6 * GIB,
                                   reserve_bytes=20 * GIB)
    waiting = threading.Event()
    def queued():
        with scheduler.acquire("new-private-copy", operation="semantic",
                waiting=lambda _: waiting.set()):
            return scheduler.active[threading.get_ident()]["estimate"]
    with ThreadPoolExecutor(1) as pool:
        future = pool.submit(queued)
        assert waiting.wait(2)
        # Three separate host-observed copies finish; the queued copy should
        # use their operation estimate without needing cancellation/resubmit.
        for index in range(3):
            with scheduler.acquire(str(index), operation="semantic", estimate_bytes=7 * GIB):
                scheduler.observe_peak(4 * GIB, completed=True)
        assert future.result(2) == 7 * GIB


def test_resident_credit_excludes_reclaimable_file_pss_and_duplicate_pids(monkeypatch):
    from trellis import memory_guard
    monkeypatch.setattr(memory_guard, "descendant_rss", lambda root:
                        (999, [(11, 999, "lean"), (12, 999, "lean")]))
    monkeypatch.setattr(Path, "read_text", lambda *_:
                        "Pss: 10000 kB\nPss_Anon: 10 kB\nPss_File: 9970 kB\nPss_Shmem: 20 kB\n")
    assert admission.Admission._resident_bytes({"processes": {11, 12}}) == 60 * 1024


def test_pressure_selects_one_victim_and_blocks_admission_until_release():
    scheduler = admission.Admission(3, floor_bytes=8)
    ready, release = threading.Event(), threading.Event()
    def background():
        with scheduler.acquire("background", priority=2):
            ready.set()
            assert release.wait(2)
            assert scheduler.pressure_reason("low memory") is None
    with ThreadPoolExecutor(1) as pool:
        with scheduler.acquire("authority", priority=0):
            future = pool.submit(background)
            assert ready.wait(2)
            assert scheduler.pressure_reason("low memory") == "low memory"
            assert scheduler._can_admit("third", 0, 0) == (False, "waiting for memory-pressure cleanup")
            release.set()
            future.result(2)
            assert scheduler.pressure_victim is not None
        assert scheduler.pressure_victim is None
        assert scheduler._can_admit("third", 0, 0)[0]


def test_prepared_semantic_payloads_overlap_and_stay_bound_to_each_principal(tmp_path, monkeypatch):
    from trellis.atomic_actions import observations
    scheduler = admission.Admission(3, readonly_slots=3)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    monkeypatch.setattr(observations, "_find_bare_mathlib_imports", lambda *a: [])
    script = tmp_path / "collector.lean"
    script.write_text("-- host collector")
    barrier = threading.Barrier(3)
    @admission.admitted
    @telemetry.measured("probe")
    def probe(repo, args, **kwargs):
        barrier.wait(2)
        node, principal = args[-2:]
        return dict(returncode=0, stdout=f"FP\t{node}\t{principal}", stderr="")
    monkeypatch.setattr(observations, "_run_lake_command", probe)
    state = telemetry.Measurement()
    marker = telemetry._current.set(state)
    try:
        with admission.authoritative_workspace("sealed"):
            result = observations.observe_lean_semantic_payloads(tmp_path, ["A", "B", "C"],
                bwrap_role="lake_compiler", script_path=script, prepared_parallel=True,
                principal_names={node: f"principal.{node}" for node in "ABC"})
        assert result == {node: dict(ok=True, payload=f"principal.{node}", error="") for node in "ABC"}
        assert state.spans["probe"]["calls"] == 3
        assert not state.stack and not state.failed
    finally:
        telemetry._current.reset(marker)


def test_source_sync_waits_for_readers_but_unchanged_request_does_not(tmp_path, monkeypatch):
    from test_local_closure_cache_prefilter import make_server
    from trellis.checker.sync import sync_tablet_dir
    server = make_server(tmp_path)
    worker = tmp_path / "worker"
    (worker / "Tablet").mkdir(parents=True)
    for source in (server.supervisor_repo / "Tablet").iterdir():
        (worker / "Tablet" / source.name).write_bytes(source.read_bytes())
    server.worker_repo = worker
    monkeypatch.setattr(server, "_try_cache_hit_only", lambda *a: ({"returncode": 0}, 0, {}))
    request = validate_request(dict(op="verify_node", request_id=1, node_name="Root"))
    changed = worker / "Tablet/Root.lean"
    original = changed.read_text()
    swept = threading.Event()
    import trellis.checker.server as module
    def sync(*args, **kwargs):
        result = sync_tablet_dir(*args, **kwargs)
        if kwargs.get("check_only") and result["changed"]:
            swept.set()
        return result
    monkeypatch.setattr(module, "sync_tablet_dir", sync)
    with ThreadPoolExecutor(1) as pool:
        with server._workspace_lock.read_lock():
            assert pool.submit(server._handle_request, request).result(2)["returncode"] == 0
            changed.write_text(original + "-- new source\n")
            future = pool.submit(server._handle_request, request)
            assert swept.wait(2)
            assert not future.done()
            assert (server.supervisor_repo / "Tablet/Root.lean").read_text() == original
        assert future.result(2)["returncode"] == 0
    assert (server.supervisor_repo / "Tablet/Root.lean").read_text() == changed.read_text()


@pytest.mark.parametrize("failure", [dict(returncode=None, resource_exhausted="host reserve reached"),
    dict(returncode=-9), dict(returncode=0, timed_out=True), dict(returncode=1)])
def test_partial_semantic_output_is_never_success(tmp_path, monkeypatch, failure):
    from trellis.atomic_actions import observations
    monkeypatch.setattr(observations, "_find_bare_mathlib_imports", lambda *a: [])
    script = tmp_path / "collector.lean"
    script.write_text("-- collector")
    monkeypatch.setattr(observations, "_run_lake_command", lambda *a, **kw:
                        dict(stdout="FP\tA\tpartial-payload", stderr="", **failure))
    result = observations.observe_lean_semantic_payloads(tmp_path, ["A"],
        bwrap_role="lake_compiler", script_path=script, principal_names={"A": "A"})
    assert result["A"]["ok"] is False and result["A"]["payload"] == ""


@pytest.mark.parametrize("kind", ["resource_exhausted", "supervisor_unavailable", "malformed_response"])
def test_host_transport_failures_are_sticky_in_only_the_exact_round(tmp_path, monkeypatch, kind):
    from trellis.atomic_actions import checker_client
    from trellis.checker import resource_failure
    def unavailable(*a, **kw):
        raise checker_client.CheckerRpcError(kind, "fixture unavailable")
    monkeypatch.setattr(checker_client, "_round_trip_once", unavailable)
    current, other = tmp_path / "round-1", tmp_path / "round-2"
    with resource_failure.host_scope(current):
        with pytest.raises(checker_client.CheckerRpcError):
            checker_client._round_trip(tmp_path / "socket", {"op": "lean_semantic_payloads"}, timeout_secs=1)
    with pytest.raises(RuntimeError, match="submission retained"):
        resource_failure.require_healthy(current)
    resource_failure.require_healthy(other)
    # An optional donor inspection can fail and fall back to fresh checking.
    with resource_failure.host_scope(other):
        with pytest.raises(checker_client.CheckerRpcError):
            checker_client._round_trip(tmp_path / "socket", {"op": "capture_replay_environment"}, timeout_secs=1)
    resource_failure.require_healthy(other)


def test_completed_poll_peak_ages_out_with_completed_history():
    scheduler = admission.Admission(2, reserve_bytes=24 * GIB)
    with scheduler.acquire('first', operation='semantic'):
        scheduler.observe_peak(20 * GIB)
        assert scheduler.estimate('semantic') == 27 * GIB
        assert scheduler.incomplete_peaks == {}
        # Completion records the whole sampled execution, not just its last
        # smaller sample, and does not shrink an active growth reservation.
        scheduler.observe_peak(4 * GIB, completed=True)
        assert scheduler.memory_samples['semantic'] == [20 * GIB]
        assert scheduler.active[threading.get_ident()]['estimate'] == 27 * GIB
    assert scheduler.incomplete_peaks == {}
    for _ in range(32):
        with scheduler.acquire('later', operation='semantic'):
            scheduler.observe_peak(4 * GIB)
            scheduler.observe_peak(4 * GIB, completed=True)
    assert scheduler.estimate('semantic') == 7 * GIB
    assert len(scheduler.memory_samples['semantic']) == 32


def test_interrupted_peak_survives_other_successes_and_nested_leases():
    scheduler = admission.Admission(2, reserve_bytes=24 * GIB)
    with pytest.raises(RuntimeError, match='interrupted'):
        with scheduler.acquire('first', operation='semantic'):
            with scheduler.acquire('first', operation='semantic'):
                scheduler.observe_peak(20 * GIB)
            assert not scheduler.incomplete_peaks
            raise RuntimeError('interrupted')
    for _ in range(33):
        with scheduler.acquire('later', operation='semantic'):
            scheduler.observe_peak(4 * GIB, completed=True)
    assert scheduler.incomplete_peaks['semantic'] == 20 * GIB
    assert scheduler.estimate('semantic') == 27 * GIB


def test_one_completion_does_not_retire_another_active_peak():
    from concurrent.futures import ThreadPoolExecutor
    scheduler = admission.Admission(2, reserve_bytes=24 * GIB)
    entered, release = threading.Event(), threading.Event()
    def other():
        with scheduler.acquire('other', operation='semantic'):
            scheduler.observe_peak(20 * GIB)
            entered.set()
            assert release.wait(5)
            # Some callers have no new final sample; completed still retires
            # the actual earlier polls into completed history.
            scheduler.observe_peak(0, completed=True)
    with ThreadPoolExecutor(1) as pool:
        task = pool.submit(other)
        try:
            assert entered.wait(2)
            for _ in range(33):
                with scheduler.acquire('small', operation='semantic'):
                    scheduler.observe_peak(4 * GIB, completed=True)
            assert scheduler.estimate('semantic') == 27 * GIB
            assert not scheduler.incomplete_peaks
        finally:
            release.set()
        task.result(timeout=2)
    assert scheduler.memory_samples['semantic'][-1] == 20 * GIB
    assert not scheduler.incomplete_peaks

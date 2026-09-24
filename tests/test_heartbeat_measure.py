"""Regression tests for ``trellis.checker.heartbeat`` — the asynchronous
kernel-side heartbeat measurement runner.

The contract under test is fail-open droppability: a measurement either
completes and lands in the runtime spool keyed by the hash it ACTUALLY ran
under, or it vanishes without a trace. No test here drives a real ``lean``
— the command builder is monkey-patched, matching this suite's convention
of unit-testing the checker without lake.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Optional

import pytest

from trellis.checker import heartbeat
from trellis.checker.heartbeat import HeartbeatMeasurer, measurement_spool_dir


@pytest.fixture
def measurer(tmp_path: Path, monkeypatch) -> HeartbeatMeasurer:
    repo = tmp_path / "repo"
    (repo / "Tablet").mkdir(parents=True)
    runtime_root = tmp_path / "runtime"
    runtime_root.mkdir()
    # Fake plugin file so the plugin-presence gate passes; no build ever
    # loads it because the command builder is patched in each test.
    fake_plugin = tmp_path / "FakeHbProf.so"
    fake_plugin.write_bytes(b"not a real plugin")
    monkeypatch.setenv("TRELLIS_HB_PLUGIN", str(fake_plugin))
    monkeypatch.setenv("TRELLIS_HB_MEM_FLOOR_GIB", "0")
    instance = HeartbeatMeasurer(repo, runtime_root)
    # Tests drive `_measure_once` synchronously; never start the thread.
    monkeypatch.setattr(instance, "_ensure_thread", lambda: None)
    return instance


def _fake_build(side_file_line: str, total_line: Optional[str] = None):
    """Command builder returning a shell command that writes the side file
    the way the plugin would (one JSON line, appended).

    ``side_file_line`` is what the ELABORATION pass (``skip_kernel_tc``)
    emits; ``total_line`` what the full pass emits. Defaulting the latter to
    the former keeps the single-number tests simple.
    """

    def build(self, node, plugin, scratch, hb_out, *, skip_kernel_tc=False):
        line = side_file_line if skip_kernel_tc else (total_line or side_file_line)
        script = f"printf '%s\\n' '{line}' >> '{hb_out}'"
        return (["/bin/sh", "-c", script], os.environ.copy())

    return build


def _hb_line(node: str, heartbeats: int) -> str:
    return json.dumps(
        {"module": f"Tablet.{node}", "decl": node, "heartbeats": heartbeats}
    )


def test_spooled_heartbeats_is_elaboration_only_with_kernel_as_context(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    """``heartbeats`` must carry the ELABORATION-only count.

    That is the quantity ``set_option maxHeartbeats`` enforces. The full
    build's larger figure is contaminated by kernel type-checking, which
    synchronous measurement pulls inline onto the measured thread and which
    the budget never governed — so it rides along as context only.
    """
    monkeypatch.setattr(measurer, "_source_closure_hash", lambda node: "hash-a")
    monkeypatch.setattr(
        HeartbeatMeasurer,
        "_build_command",
        _fake_build(_hb_line("Giant", 133873), _hb_line("Giant", 204514)),
    )
    assert measurer._measure_once("Giant", "hash-a") is None
    spooled = json.loads(
        (measurement_spool_dir(measurer.runtime_root) / "Giant.json").read_text()
    )
    assert spooled["node"] == "Giant"
    assert spooled["heartbeats"] == 133873, "elaboration-only, not the total"
    assert spooled["total_heartbeats"] == 204514
    assert spooled["kernel_heartbeats"] == 204514 - 133873
    assert spooled["heartbeats_key"] == "hash-a"


def test_total_pass_failure_still_spools_the_elaboration_count(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    """The context pass is best-effort: losing it must not lose the
    headline number, and must not publish a bogus kernel share."""
    monkeypatch.setattr(measurer, "_source_closure_hash", lambda node: "hash-a")

    def build(self, node, plugin, scratch, hb_out, *, skip_kernel_tc=False):
        if skip_kernel_tc:
            script = f"printf '%s\\n' '{_hb_line('Giant', 133873)}' >> '{hb_out}'"
        else:
            script = "exit 3"  # the full build fails
        return (["/bin/sh", "-c", script], os.environ.copy())

    monkeypatch.setattr(HeartbeatMeasurer, "_build_command", build)
    assert measurer._measure_once("Giant", "hash-a") is None
    spooled = json.loads(
        (measurement_spool_dir(measurer.runtime_root) / "Giant.json").read_text()
    )
    assert spooled["heartbeats"] == 133873
    assert "total_heartbeats" not in spooled
    assert "kernel_heartbeats" not in spooled


def test_a_total_below_elaboration_is_rejected_as_context(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    """Kernel work cannot make a build cheaper. A total under the
    elaboration figure means the passes disagreed, so the context is
    dropped rather than published as a negative kernel share."""
    monkeypatch.setattr(measurer, "_source_closure_hash", lambda node: "hash-a")
    monkeypatch.setattr(
        HeartbeatMeasurer,
        "_build_command",
        _fake_build(_hb_line("Giant", 500), _hb_line("Giant", 400)),
    )
    assert measurer._measure_once("Giant", "hash-a") is None
    spooled = json.loads(
        (measurement_spool_dir(measurer.runtime_root) / "Giant.json").read_text()
    )
    assert spooled["heartbeats"] == 500
    assert "kernel_heartbeats" not in spooled


def test_content_moving_mid_measurement_drops_the_result(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    # First hash check (pre-build) matches; the post-build check sees the
    # node changed. Neither hash honestly describes what was elaborated, so
    # nothing may be spooled.
    hashes = iter(["hash-a", "hash-b"])
    monkeypatch.setattr(
        measurer, "_source_closure_hash", lambda node: next(hashes)
    )
    line = json.dumps(
        {"module": "Tablet.Giant", "decl": "Giant", "heartbeats": 100}
    )
    monkeypatch.setattr(
        HeartbeatMeasurer, "_build_command", _fake_build(line)
    )
    reason = measurer._measure_once("Giant", "hash-a")
    assert reason == "content moved during measurement"
    assert not measurement_spool_dir(measurer.runtime_root).exists()


def test_failed_or_unavailable_measurement_leaves_no_trace(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    monkeypatch.setattr(
        measurer, "_source_closure_hash", lambda node: "hash-a"
    )
    # lean exits non-zero.
    monkeypatch.setattr(
        HeartbeatMeasurer,
        "_build_command",
        lambda self, node, plugin, scratch, hb_out, *, skip_kernel_tc=False: (
            ["/bin/sh", "-c", "exit 3"],
            os.environ.copy(),
        ),
    )
    assert measurer._measure_once("Giant", "hash-a") == "lean exited 3"
    # Build "succeeds" but the side file holds garbage.
    monkeypatch.setattr(
        HeartbeatMeasurer, "_build_command", _fake_build("not json")
    )
    assert measurer._measure_once("Giant", "hash-a") == "no usable side-file entry"
    # Plugin missing entirely.
    monkeypatch.setenv("TRELLIS_HB_PLUGIN", str(measurer.runtime_root / "absent.so"))
    assert measurer._measure_once("Giant", "hash-a") == "plugin missing"
    # Content already moved before the build started.
    monkeypatch.setenv(
        "TRELLIS_HB_PLUGIN", str(next(measurer.runtime_root.parent.glob("FakeHbProf.so")))
    )
    assert (
        measurer._measure_once("Giant", "hash-elsewhere")
        == "content moved before measurement"
    )
    assert not measurement_spool_dir(measurer.runtime_root).exists()
    # The scratch tree under .lake/build never accumulates.
    scratch_root = measurer.supervisor_repo / ".lake" / "build" / "trellis-hb-measure"
    assert not any(scratch_root.iterdir())


def test_measurement_command_requests_exact_uncapped_mode(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    """Pin the two things that make the number usable.

    ``TRELLIS_HB_SYNC`` selects the exact (async-off) plugin mode — the
    async mode inflates by a per-node factor of up to 1.44x, which reorders
    nodes and defeats the only purpose these counts have. ``TRELLIS_HB_UNCAP``
    makes the plugin raise the ceiling from inside the declaration's scope,
    which is the only thing that beats a file-local ``set_option
    maxHeartbeats N`` — without it the ~10% of nodes carrying an override,
    the expensive ones, cannot be measured at all.
    """
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setattr(
        "trellis.sandbox.wrap_command",
        lambda inner_cmd, **kwargs: ["bwrap", *inner_cmd],
    )
    def build(skip: bool):
        got = measurer._build_command(
            "Giant",
            Path("/plugin/HbProf.so"),
            measurer.runtime_root,
            measurer.runtime_root / "hb.jsonl",
            skip_kernel_tc=skip,
        )
        assert got is not None, "command builder must succeed with bwrap stubbed"
        return got

    command, env = build(True)
    joined = " ".join(command)
    assert "--setenv TRELLIS_HB_SYNC 1" in joined
    assert "--setenv TRELLIS_HB_UNCAP 1" in joined
    assert "-DmaxHeartbeats=0" in joined
    assert "-DElab.async=false" in joined
    # The headline pass excludes kernel type-checking; the source file must
    # remain the last argument.
    assert "-Ddebug.skipKernelTC=true" in joined
    assert command[-1] == "Tablet/Giant.lean"
    assert command[-3] == "-o"
    # The plain env carries them too, for the non-bwrap-inherited path.
    assert env["TRELLIS_HB_SYNC"] == "1"
    assert env["TRELLIS_HB_UNCAP"] == "1"

    # The context pass keeps kernel checking on — that is what makes it a
    # total rather than a second copy of the headline number.
    command_total, _ = build(False)
    assert "-Ddebug.skipKernelTC=true" not in " ".join(command_total)
    assert command_total[-1] == "Tablet/Giant.lean"


def test_bisect_probe_elaborates_exactly_as_production_does(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    """A Tier-2 probe asks 'would the real build survive this ceiling?', so
    it must carry the ceiling and no instrumentation whatsoever."""
    monkeypatch.setattr("trellis.sandbox.bwrap_available", lambda: True)
    monkeypatch.setattr(
        "trellis.sandbox.wrap_command",
        lambda inner_cmd, **kwargs: ["bwrap", *inner_cmd],
    )
    built = measurer._build_command(
        "Giant",
        Path("/plugin/HbProf.so"),
        measurer.runtime_root,
        measurer.runtime_root / "hb.jsonl",
        bisect_cap=9300,
    )
    assert built is not None
    command, env = built
    joined = " ".join(command)
    # The ceiling must come from the plugin's scope-set, not a CLI default:
    # a file-local `set_option maxHeartbeats` beats the latter.
    assert "--setenv TRELLIS_HB_CAP 9300" in joined
    assert env["TRELLIS_HB_CAP"] == "9300"
    assert "-DmaxHeartbeats" not in joined
    # Production-like elaboration: async on, kernel checking on, no
    # measurement side channel.
    assert "-DElab.async=false" not in joined
    assert "-Ddebug.skipKernelTC=true" not in joined
    for absent in ("TRELLIS_HB_OUT", "TRELLIS_HB_SYNC", "TRELLIS_HB_UNCAP"):
        assert absent not in joined
        assert absent not in env


def _write_node(measurer: HeartbeatMeasurer, node: str, body: str = "") -> None:
    (measurer.supervisor_repo / "Tablet" / f"{node}.lean").write_text(body)


def test_declared_limit_reads_the_nodes_own_grant(
    measurer: HeartbeatMeasurer,
) -> None:
    _write_node(measurer, "Plain", "theorem Plain : True := trivial\n")
    assert measurer._declared_limit("Plain") == 200_000
    _write_node(
        measurer, "Raised", "set_option maxHeartbeats 800000\ntheorem Raised : True := trivial\n"
    )
    assert measurer._declared_limit("Raised") == 800_000
    # Several grants: the largest governs the main declaration; a small
    # scoped one on a helper must not drag the limit down and fire Tier 2
    # on every build.
    _write_node(
        measurer,
        "Mixed",
        "set_option maxHeartbeats 400000\nset_option maxHeartbeats 1000 in\ntheorem M : True := trivial\n",
    )
    assert measurer._declared_limit("Mixed") == 400_000
    # A node whose file is unreadable falls back to Lean's default.
    assert measurer._declared_limit("Absent") == 200_000


def test_tier2_fires_only_near_the_limit_and_records_the_exact_value(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    """A node reading at/above 95% of its limit gets bisected; the spool
    keeps the Tier-1 number under ``heartbeats`` and names the authority."""
    monkeypatch.setattr(measurer, "_source_closure_hash", lambda node: "hash-a")
    _write_node(measurer, "Giant", "theorem Giant : True := trivial\n")  # 200k limit
    monkeypatch.setattr(
        HeartbeatMeasurer,
        "_build_command",
        _fake_build(_hb_line("Giant", 200761), _hb_line("Giant", 244788)),
    )
    monkeypatch.setattr(
        HeartbeatMeasurer,
        "_bisect_enforced",
        lambda self, node, plugin, scratch, upper: 199138,
    )
    assert measurer._measure_once("Giant", "hash-a") is None
    spooled = json.loads(
        (measurement_spool_dir(measurer.runtime_root) / "Giant.json").read_text()
    )
    assert spooled["heartbeats"] == 200761, "Tier 1 stays the `heartbeats` field"
    assert spooled["bisected_heartbeats"] == 199138
    assert spooled["heartbeats_method"] == "bisected"
    assert spooled["declared_limit"] == 200_000


def test_tier2_does_not_fire_on_a_comfortable_node(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    """Below the trigger the exact value cannot change any decision, so no
    bisection runs and the method reads as measured."""
    monkeypatch.setattr(measurer, "_source_closure_hash", lambda node: "hash-a")
    _write_node(measurer, "Small", "theorem Small : True := trivial\n")
    monkeypatch.setattr(
        HeartbeatMeasurer, "_build_command", _fake_build(_hb_line("Small", 1000))
    )

    def boom(self, *a, **k):
        raise AssertionError("Tier 2 must not run for a comfortable node")

    monkeypatch.setattr(HeartbeatMeasurer, "_bisect_enforced", boom)
    assert measurer._measure_once("Small", "hash-a") is None
    spooled = json.loads(
        (measurement_spool_dir(measurer.runtime_root) / "Small.json").read_text()
    )
    assert spooled["heartbeats"] == 1000
    assert "bisected_heartbeats" not in spooled
    assert spooled["heartbeats_method"] == "measured"


def test_bisection_converges_and_abandons_on_a_broken_probe(
    measurer: HeartbeatMeasurer, monkeypatch
) -> None:
    """The search returns the smallest ceiling the node builds under, and a
    probe that fails for a NON-heartbeat reason abandons rather than being
    read as 'too slow'."""
    enforced = 199138
    probes: list = []

    def probe(self, node, plugin, scratch, cap):
        probes.append(cap)
        return cap >= enforced

    monkeypatch.setattr(HeartbeatMeasurer, "_probe_builds_under_cap", probe)
    got = measurer._bisect_enforced("Giant", Path("/p"), measurer.runtime_root, 200761)
    assert got is not None
    # Sound (a ceiling that really builds) and tight enough that no decision
    # against a limit could change.
    assert got >= enforced
    assert got - enforced <= max(1, int(got * heartbeat._BISECT_REL_TOLERANCE)) * 2
    assert len(probes) <= heartbeat._BISECT_MAX_PROBES

    monkeypatch.setattr(
        HeartbeatMeasurer,
        "_probe_builds_under_cap",
        lambda self, node, plugin, scratch, cap: None,
    )
    assert (
        measurer._bisect_enforced("Giant", Path("/p"), measurer.runtime_root, 200761)
        is None
    )


def test_enqueue_is_bounded_deduped_and_never_blocks(
    measurer: HeartbeatMeasurer,
) -> None:
    measurer.enqueue("NodeA", "h1")
    measurer.enqueue("NodeA", "h1")  # duplicate → dropped
    assert measurer._queue.qsize() == 1
    for index in range(heartbeat._QUEUE_MAX + 5):  # overflow → dropped
        measurer.enqueue(f"Node{index}", "h1")
    assert measurer._queue.qsize() <= heartbeat._QUEUE_MAX
    # Invalid names never enter the queue.
    before = measurer._queue.qsize()
    measurer.enqueue("../escape", "h1")
    measurer.enqueue("", "h1")
    assert measurer._queue.qsize() == before


def test_server_memoize_feeds_the_measurer_only_for_unmeasured_costs(
    tmp_path: Path, monkeypatch
) -> None:
    """Pin the feeding seam: ``_memoize_elaboration_cost`` is the ONE
    production trigger, and it fires exactly for fresh costs lacking a
    heartbeat count."""
    import tempfile

    from trellis.checker.server import CheckerServer

    base = Path(tempfile.mkdtemp(prefix="lcs-hb-", dir=str(tmp_path)))
    repo = base / "r"
    runtime = repo / ".trellis" / "runtime" / "rt"
    runtime.mkdir(parents=True)
    (repo / "Tablet").mkdir(parents=True)
    server = CheckerServer(runtime, parallelism=1, socket_group_gid=None)

    enqueued: list[tuple] = []
    monkeypatch.setattr(
        HeartbeatMeasurer,
        "enqueue",
        lambda self, node, closure_hash: enqueued.append((node, closure_hash)),
    )
    server._memoize_elaboration_cost(
        {"cost": {"node": "Alpha", "source_closure_hash": "h1"}}
    )
    server._memoize_elaboration_cost(
        {"cost": {"node": "Beta", "source_closure_hash": "h2", "heartbeats": 5}}
    )
    assert enqueued == [("Alpha", "h1")]

    # And the hook is fail-open: a measurer whose constructor explodes
    # degrades to "disabled", never to an error on the memoize path.
    server._heartbeat_measurer = None

    def boom(self, *args, **kwargs):
        raise RuntimeError("constructor exploded")

    monkeypatch.setattr(HeartbeatMeasurer, "__init__", boom)
    server._memoize_elaboration_cost(
        {"cost": {"node": "Gamma", "source_closure_hash": "h3"}}
    )
    assert server._heartbeat_measurer is False

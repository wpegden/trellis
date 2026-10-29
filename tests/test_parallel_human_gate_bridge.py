"""Real bridge/normalizer fixtures: no provider, checker or live runtime needed."""
import json
import os
from pathlib import Path
import subprocess

import pytest


ROOT = Path(__file__).resolve().parents[1]
pytestmark = pytest.mark.skipif(
    not os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD"),
    reason="requires the production runtime CLI for native gate normalization",
)


@pytest.fixture
def canonical_bridge(tmp_path):
    repo, runtime = tmp_path / "canonical-repo", tmp_path / "canonical-runtime"
    repo.mkdir()
    runtime.mkdir()
    config = repo / "trellis.config.json"
    config.write_text(json.dumps({
        "repo_path": str(repo), "worker": {"provider": "codex"},
        "reviewer": {"provider": "codex"}, "tmux": {},
        "sandbox": {"enabled": True}, "parallel_dispatch": {"enabled": True},
    }))
    # Normalization must not load/write native state or invoke an agent/checker.
    protocol = runtime / "protocol_state.json"
    protocol.write_bytes(b"native state belongs to the caller, not the bridge\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("TRELLIS_")}
    env["TRELLIS_TRELLIS_KERNEL_CMD"] = os.environ["TRELLIS_TRELLIS_KERNEL_CMD"]
    env["TMPDIR"] = str(tmp_path)
    request = dict(kind="human_gate", id=313, cycle=59, runtime_support_required=False)

    def call(**updates):
        before = {p.relative_to(tmp_path): p.read_bytes()
                  for p in tmp_path.rglob("*") if p.is_file()}
        result = subprocess.run(
            [str(ROOT / "scripts/trellis_bridge.sh")],
            input=json.dumps(dict(config_path=str(config), runtime_root=str(runtime),
                                  request={**request, **updates})),
            env=env, capture_output=True, text=True, timeout=30,
        )
        output = json.loads(result.stdout)
        after = {p.relative_to(tmp_path): p.read_bytes()
                 for p in tmp_path.rglob("*") if p.is_file()}
        # Only the existing signal and viewer sidecar may be consumed. No burst
        # token/log, task workspace, native state write or provider receipt.
        assert not after.keys() - before.keys(), result.stderr
        for path in after:
            assert after[path] == before[path]
        assert set(before) - set(after) <= {
            Path("canonical-runtime/human_gate_response.json"),
            Path("canonical-runtime/human_gate_response.viewer_meta.json"),
        }
        return result.returncode, output

    return runtime, call


def signal(runtime, payload):
    path = runtime / "human_gate_response.json"
    path.write_text(payload)
    (runtime / "human_gate_response.viewer_meta.json").write_text("retained viewer metadata")
    return path


@pytest.mark.parametrize("choice", ["approve", "feedback"])
def test_canonical_parallel_gate_consumes_native_fresh_signal_once(canonical_bridge, choice):
    runtime, call = canonical_bridge
    path = signal(runtime, json.dumps(dict(choice=choice, cycle=59, request_id=313)))
    code, output = call()
    assert code == 0, output
    assert output["status"] == "Ok"
    assert output["choice"] == choice.title()
    assert output["request_id"] == 313 and output["cycle"] == 59
    assert not path.exists()
    assert not (runtime / "human_gate_response.viewer_meta.json").exists()
    code, output = call()
    assert code == 1 and "missing human gate response file" in output["error"]


@pytest.mark.parametrize("stamp", [dict(cycle=58, request_id=313),
                                  dict(cycle=59, request_id=312), {}])
def test_canonical_parallel_gate_preserves_native_stale_signal(canonical_bridge, stamp):
    runtime, call = canonical_bridge
    path = signal(runtime, json.dumps(dict(choice="approve", **stamp)))
    code, output = call()
    assert code == 1 and "missing human gate response file" in output["error"]
    assert path.exists() and (runtime / "human_gate_response.viewer_meta.json").exists()


@pytest.mark.parametrize("raw", ["not json", '{"choice":"unknown","cycle":59}'])
def test_canonical_parallel_gate_preserves_native_malformed_signal(canonical_bridge, raw):
    runtime, call = canonical_bridge
    path = signal(runtime, raw)
    code, output = call()
    assert code == 0 and output["status"] == "Malformed"
    assert path.exists() and (runtime / "human_gate_response.viewer_meta.json").exists()


@pytest.mark.parametrize("marker", ["system_feedback_halt.json", "checker_disagreement_halt.json"])
def test_canonical_parallel_gate_respects_halt_before_consumption(canonical_bridge, marker):
    runtime, call = canonical_bridge
    path = signal(runtime, '{"choice":"approve","cycle":59,"request_id":313}')
    (runtime / marker).write_text("retained halt")
    code, output = call()
    assert code == 1 and "halt marker present" in output["error"]
    assert path.exists()


def test_canonical_parallel_gate_requires_native_support_metadata(canonical_bridge):
    runtime, call = canonical_bridge
    path = signal(runtime, '{"choice":"approve","cycle":59,"request_id":313}')
    code, output = call(runtime_support_required=None)
    assert code == 1 and "kernel-authored runtime_support_required" in output["error"]
    assert path.exists()


@pytest.mark.parametrize("kind", ["worker", "review", "audit"])
def test_canonical_parallel_agent_request_still_requires_coordinator_workspace(canonical_bridge, kind):
    runtime, call = canonical_bridge
    path = signal(runtime, '{"choice":"approve","cycle":59,"request_id":313}')
    code, output = call(kind=kind)
    assert code == 1 and "parallel dispatch requires the native coordinator" in output["error"]
    assert path.exists()

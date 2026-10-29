import json
import os
import shlex
from pathlib import Path
import subprocess

import pytest

from trellis.modeling import reopening
from trellis.runtime import kernel_cli


def terminal():
    return dict(phase="Modeling", stage="Reviewer", cycle=42, request_seq=71,
                in_flight_request=None,
                modeling_request={"action": "reopen", "reason": 'Review "quoted" repair 雪'})


@pytest.fixture
def runtime(tmp_path):
    (tmp_path / "protocol_state.json").write_text(json.dumps(terminal()))
    (tmp_path / "launch_env.json").write_text(json.dumps({
        "env": {"TRELLIS_TRELLIS_KERNEL_CMD": "/recorded/runtime-cli"}}))
    return tmp_path


@pytest.mark.parametrize("changes", [
    {}, {"phase": "ProofFormalization"}, {"stage": "HumanGate"},
    {"in_flight_request": {}}, {"modeling_request": None},
    {"modeling_request": {"action": "fail", "reason": "Accepted fail reason"}},
    {"modeling_request": {"action": "review_route", "reason": "Still awaiting audit"}},
])
def test_projection_reuses_lifecycle_predicate_without_loading_protocol(runtime, monkeypatch, changes):
    state = terminal()
    state.update(changes)
    original_read = reopening.read_json

    def read(path):
        assert Path(path).name != "protocol_state.json", "status must not load full protocol in Python"
        return original_read(path)

    def run(command, **kwargs):
        assert command == ["/recorded/runtime-cli", "read-modeling-disposition-state", str(runtime)]
        assert kwargs["input"] == ""
        return subprocess.CompletedProcess(command, 0, json.dumps({
            "status": "modeling_disposition_state_ok", "output": state}), "")

    monkeypatch.setattr(reopening, "read_json", read)
    monkeypatch.setattr(reopening.subprocess, "run", run)
    assert reopening.read_disposition(runtime) == reopening.disposition(state)


def test_legacy_binary_uses_json_only_after_exact_unsupported_command(runtime, monkeypatch):
    calls = []

    def run(command, **kwargs):
        calls.append(command)
        if len(command) == 3:
            assert kwargs['input'] == ''
            return subprocess.CompletedProcess(command, 2, json.dumps({'status': 'invalid_request',
                'message': 'unknown runtime command `read-modeling-disposition-state`'}), '')
        assert command == ['/recorded/runtime-cli']
        assert json.loads(kwargs['input']) == {'action': 'read_modeling_disposition_state', 'root': str(runtime)}
        return subprocess.CompletedProcess(command, 0, json.dumps({'status': 'modeling_disposition_state_ok',
            'output': terminal()}), '')

    monkeypatch.setattr(reopening.subprocess, 'run', run)
    assert reopening.read_disposition(runtime) == reopening.disposition(terminal())
    assert len(calls) == 2


def test_only_old_binary_unsupported_action_falls_back(runtime, monkeypatch):
    monkeypatch.setattr(reopening.subprocess, "run", lambda command, **kwargs:
        subprocess.CompletedProcess(command, 2, json.dumps({"status": "invalid_request",
            "message": "invalid request: unknown variant `read_modeling_disposition_state`"}), ""))
    assert reopening.read_disposition(runtime) == reopening.disposition(terminal())


@pytest.mark.parametrize("status,code,message", [
    ("error", 1, "Cannot read modeling disposition state: trailing characters"),
    ("invalid_request", 2, "invalid request: missing root"),
    ("ok", 0, "unexpected response"),
])
def test_read_failures_remain_unknown_without_full_read_fallback(runtime, monkeypatch, status, code, message):
    original_read = reopening.read_json

    def read(path):
        assert Path(path).name != "protocol_state.json"
        return original_read(path)

    monkeypatch.setattr(reopening, "read_json", read)
    monkeypatch.setattr(reopening.subprocess, "run", lambda command, **kwargs:
        subprocess.CompletedProcess(command, code, json.dumps({"status": status, "message": message}), ""))
    with pytest.raises(RuntimeError, match=message):
        reopening.read_disposition(runtime)


def test_status_never_invokes_cargo(runtime, monkeypatch):
    (runtime / "launch_env.json").unlink()
    monkeypatch.setattr(kernel_cli, "kernel_cli_command", lambda: ["/tools/cargo", "run"])
    monkeypatch.setattr(reopening.subprocess, "run", lambda *args, **kwargs: pytest.fail("status invoked Cargo"))
    assert reopening.read_disposition(runtime) == reopening.disposition(terminal())


def test_missing_runtime_needs_no_executable(tmp_path, monkeypatch):
    monkeypatch.setattr(kernel_cli, "kernel_cli_command", lambda: pytest.fail("missing runtime queried executable"))
    assert reopening.read_disposition(tmp_path) is None


def test_missing_recorded_binary_preserves_legacy_status(runtime, monkeypatch):
    def missing(*args, **kwargs):
        raise FileNotFoundError("retired executable")
    monkeypatch.setattr(reopening.subprocess, "run", missing)
    assert reopening.read_disposition(runtime) == reopening.disposition(terminal())


production_binary = pytest.mark.skipif(not os.environ.get("TRELLIS_TRELLIS_KERNEL_CMD"),
                                      reason="requires the production runtime CLI")


@production_binary
def test_read_only_command_cannot_dispatch_mutable_stdin(runtime):
    before = (runtime / 'protocol_state.json').read_bytes()
    command = shlex.split(os.environ['TRELLIS_TRELLIS_KERNEL_CMD'])
    # The argument mode is authoritative. Mutable stdin is never dispatched.
    result = subprocess.run([*command, 'read-modeling-disposition-state', str(runtime)],
        input=json.dumps({'action': 'activate_pv_revision', 'root': str(runtime), 'confirm_runtime_stopped': True}),
        capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout)['status'] == 'modeling_disposition_state_ok'
    extra = subprocess.run([*command, 'read-modeling-disposition-state', str(runtime), 'step'],
        input='{}', capture_output=True, text=True, timeout=30)
    assert extra.returncode == 2
    assert (runtime / 'protocol_state.json').read_bytes() == before


@production_binary
@pytest.mark.parametrize("changes", [
    {}, {"phase": "ProofFormalization"}, {"stage": "HumanGate"},
    {"in_flight_request": {}}, {"in_flight_request": {"payload": [1, {"phase": "Modeling"}]}},
    {"modeling_request": None},
    {"modeling_request": {"action": "fail", "reason": "Accepted fail reason"}},
])
def test_production_projection_matches_disposition(runtime, monkeypatch, changes):
    state = terminal()
    state.update(changes)
    state["dispatch"] = {"tasks": [{"request": {"phase": "Modeling", "stage": "Reviewer"},
                                    "padding": '\\"{}[]雪' * 200_000}]}
    (runtime / "protocol_state.json").write_text(json.dumps(state))
    (runtime / "launch_env.json").write_text(json.dumps({
        "env": {"TRELLIS_TRELLIS_KERNEL_CMD": os.environ["TRELLIS_TRELLIS_KERNEL_CMD"]}}))
    original_read = reopening.read_json

    def read(path):
        assert Path(path).name != "protocol_state.json", "production action fell back to full Python read"
        return original_read(path)

    monkeypatch.setattr(reopening, "read_json", read)
    assert reopening.read_disposition(runtime) == reopening.disposition(state)


@production_binary
@pytest.mark.parametrize("raw", [
    '{"phase":"Modeling","dispatch":{"unfinished":[1,}}',
    '{"phase":"Modeling"} trailing',
    '{"phase":"Modeling"} {}',
    '{"phase":"Modeling","phase":"ProofFormalization"}',
    '{"phase":"Modeling","in_flight_request":{"broken": }}',
    '{"phase":"Modeling"',
    'null',
])
def test_production_projection_rejects_invalid_document(runtime, raw):
    (runtime / "protocol_state.json").write_text(raw)
    result = subprocess.run(shlex.split(os.environ["TRELLIS_TRELLIS_KERNEL_CMD"]),
                            input=json.dumps({"action": "read_modeling_disposition_state", "root": str(runtime)}),
                            capture_output=True, text=True, timeout=30)
    assert result.returncode == 1
    assert json.loads(result.stdout)["status"] == "error"
    assert "Cannot read modeling disposition state" in json.loads(result.stdout)["message"]

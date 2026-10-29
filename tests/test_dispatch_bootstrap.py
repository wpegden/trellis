from pathlib import Path

import pytest

from trellis.adapters import BurstResult, ProviderConfig
from trellis.agent_wrapper.executor import execute_agent_request
from trellis.agent_wrapper.protocol import AgentLane, SingleAgentRequest


class Ports:
    def resolve(self, *_):
        return 0


def request(tmp_path, home_name):
    home = tmp_path / "burst-homes" / home_name
    home.mkdir(parents=True)
    (home / "evidence.txt").write_text("retained bootstrap evidence")
    return SingleAgentRequest(
        request_id="worker_780", cycle=183, kind="Worker", burst_role="worker",
        provider=ProviderConfig("codex"), prompt="test", work_dir=tmp_path,
        state_dir=tmp_path / "state", session_name="bootstrap-test",
        lane=AgentLane("worker"), timeout_seconds=1, burst_home=home,
        log_dir=tmp_path / "logs", artifact_prefix="/some/path/worker_780_result",
    )


@pytest.mark.parametrize("home_name,persistent", [
    ("worker-task-2", True), ("reviewer-task-3", True),
    ("worker", True), ("reviewer", True), ("temporary-burst", False),
])
@pytest.mark.parametrize("ok", [False, True])
def test_home_lifetime_and_retained_failure(tmp_path, home_name, persistent, ok):
    req = request(tmp_path, home_name)
    output = "bash: /task/burst.sh: Permission denied"
    original = "Burst Git preflight/bootstrap failed"
    result = execute_agent_request(req, port_resolver=Ports(), worker_runner=lambda *a, **k:
        BurstResult(ok=ok, exit_code=0 if ok else 126, captured_output=output,
                    duration_seconds=0, error="" if ok else original))
    assert req.burst_home.exists() == persistent
    assert result.ok == ok
    diagnostic = req.log_dir / "worker_780_result-failure.txt"
    if ok:
        assert not diagnostic.exists()
    else:
        assert diagnostic.read_text() == output
        assert result.error == f"{original}\nLaunch diagnostic: {diagnostic}"
        assert result.captured_output == output


def test_unwritable_diagnostic_preserves_original_failure(tmp_path, monkeypatch):
    req = request(tmp_path, "worker-task-2")
    write = Path.write_text

    def fail_diagnostic(self, *args, **kwargs):
        if self.name.endswith("-failure.txt"):
            raise OSError("filesystem full")
        return write(self, *args, **kwargs)

    monkeypatch.setattr(Path, "write_text", fail_diagnostic)
    result = execute_agent_request(req, port_resolver=Ports(), worker_runner=lambda *a, **k:
        BurstResult(ok=False, exit_code=126, captured_output="Permission denied",
                    duration_seconds=0, error="Burst Git preflight/bootstrap failed"))
    assert not result.ok
    assert result.error == "Burst Git preflight/bootstrap failed"
    assert result.captured_output == "Permission denied"
    assert (req.burst_home / "evidence.txt").exists()

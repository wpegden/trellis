import json
import os
from pathlib import Path
import shutil
import sys

import pytest

from trellis.launch_verification import arm_initial_launch, initial_budget_pause
from first_launch_fixture import write_pause


@pytest.fixture
def paused(tmp_path, monkeypatch):
    runtime, repo = tmp_path / "runtime", tmp_path / "repo"
    runtime.mkdir(); repo.mkdir()
    executable = tmp_path / "runtime-cli"
    shutil.copyfile(sys.executable, executable)
    monkeypatch.setenv("TRELLIS_LAUNCH_ATTEMPT", arm_initial_launch(runtime, repo, str(executable)))
    write_pause(runtime, repo, executable)
    return runtime, repo, executable


def test_matching_first_pause_is_read_only_and_repeated_admission_is_safe(paused):
    runtime, repo, executable = paused
    before = {path.name: path.read_bytes() for path in runtime.iterdir()}
    assert initial_budget_pause(runtime, repo, str(executable))
    assert initial_budget_pause(runtime, repo, str(executable))
    assert before == {path.name: path.read_bytes() for path in runtime.iterdir()}


@pytest.mark.parametrize("filename,field,value", [
    ("pause_request.json", "launch_attempt", "previous launch"),
    ("pause_request.json", "runtime_root", "/wrong/runtime"),
    ("pause_request.json", "supervisor_executable", "/wrong/kernel"),
    ("pause_request.json", "armed_at_cycle", 8),
    ("pause_request.json", "detail", {"provider":"test", "request_id":43,"request_kind":"Worker"}),
    ("pause_request.json", "detail", {"provider":"test", "request_id":42,"request_kind":"Reviewer"}),
    ("protocol_state.json", "in_flight_request", None),
    ("runtime_metadata.json", "repo_path", "/wrong/repo"),
    ("launch_env.json", "runtime_root", "/wrong/runtime"),
    ("launch_env.json", "env", {}),
])
def test_stale_or_mismatched_pause_never_qualifies(paused, filename, field, value):
    runtime, repo, executable = paused
    path = runtime / filename
    document = json.loads(path.read_text()); document[field] = value
    path.write_text(json.dumps(document))
    assert not initial_budget_pause(runtime, repo, str(executable))


@pytest.mark.parametrize("marker", ["runtime_error_halt.json", "checker_disagreement_halt.json", "system_feedback_halt.json"])
def test_halts_take_precedence_even_when_unparseable(paused, marker):
    runtime, repo, executable = paused
    (runtime / marker).write_text("broken marker")
    assert not initial_budget_pause(runtime, repo, str(executable))


def test_replaced_or_different_executable_cannot_qualify(paused):
    runtime, repo, executable = paused
    assert not initial_budget_pause(runtime, repo, "/bin/false")
    executable.write_bytes(b"replaced")
    assert not initial_budget_pause(runtime, repo, str(executable))


def test_missing_or_malformed_runtime_cannot_qualify(paused):
    runtime, repo, executable = paused
    (runtime / "protocol_state.json").write_text("bad json")
    assert not initial_budget_pause(runtime, repo, str(executable))


@pytest.mark.parametrize("ambient_root", [None, "/different/stale/runtime"])
def test_production_trellis_shell_captures_its_actual_runtime(tmp_path, ambient_root):
    import subprocess
    source = Path(__file__).resolve().parents[1]
    runtime, repo = tmp_path / "runtime", tmp_path / "repo"
    runtime.mkdir(); repo.mkdir()
    # Avoid source-archive work; everything else in the production run launcher,
    # including token registration and environment capture, runs unchanged.
    (runtime / "trellis-source-snapshot/test-fixture").mkdir(parents=True)
    cli = tmp_path / "kernel-fixture"
    cli.write_text("#!/usr/bin/env python3\n" + '''import json,os,sys
from pathlib import Path
from first_launch_fixture import write_pause
request=json.load(sys.stdin)
assert request['action']=='run'
root=Path(request['root'])
assert os.environ['TRELLIS_KERNEL_CACHE_ROOT']==str(root)
write_pause(root, root.parent/'repo', sys.argv[0], capture=False)
''')
    cli.chmod(0o700)
    env = {**os.environ, "PYTHONPATH":f"{source / 'tests'}:{source}",
           "TRELLIS_REVIEWER_SOURCE_SHA":"test-fixture", "TRELLIS_TRELLIS_KERNEL_CMD":str(cli),
           "TRELLIS_CHECKER_SOCKET":str(runtime / "sockets/checker.sock"),
           "TRELLIS_LAUNCH_ATTEMPT":arm_initial_launch(runtime, repo, str(cli))}
    env.pop("TRELLIS_KERNEL_CACHE_ROOT", None)
    if ambient_root: env["TRELLIS_KERNEL_CACHE_ROOT"] = ambient_root
    result = subprocess.run(["bash",source / "scripts/trellis.sh","run",runtime],
                            env=env,capture_output=True,text=True,timeout=20)
    assert result.returncode == 0, result.stdout + result.stderr
    launch = json.loads((runtime / "launch_env.json").read_text())
    assert launch["env"]["TRELLIS_KERNEL_CACHE_ROOT"] == str(runtime)
    assert initial_budget_pause(runtime, repo, str(cli))

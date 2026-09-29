"""Exercise the real pause script without launching a supervisor or tmux."""

import json
import os
from pathlib import Path
import subprocess

import pytest


ROOT = Path(__file__).resolve().parents[1]


@pytest.mark.parametrize("scenario,code,new_pause", [
    ("running", 0, False),
    ("paused_again_alive", 0, True),
    ("paused_again_down", 4, True),
    ("launch_failed", 2, False),
    ("never_started", 4, False),
    ("interrupted_resume", 4, False),
])
def test_resume_preserves_new_budget_pause_or_restores_failed_launch(tmp_path, scenario, code, new_pause):
    runtime = tmp_path / "runtime"
    repo = tmp_path / "repo"
    bin_dir = tmp_path / "bin"
    for directory in (runtime, repo, bin_dir):
        directory.mkdir()
    request = runtime / "pause_request.json"
    original = {"kind": "provider_budget", "reason": "previous allowance exhaustion"}
    request.write_text(json.dumps(original))
    if scenario == "interrupted_resume":
        request.rename(runtime / "pause_request.json.resuming.interrupted")
    (runtime / "launch_env.json").write_text(json.dumps({
        "trellis_root": str(ROOT), "cwd": str(ROOT), "env": {}, "tmux_session": "test",
    }))
    programs = {
        "pgrep": '#!/bin/sh\nif [ -f "$PAUSE_TEST_ROOT/alive" ]; then echo 12345; fi\n',
        "sleep": "#!/bin/sh\nexit 0\n",
        "tmux": """#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ['PAUSE_TEST_ROOT'])
scenario = os.environ['PAUSE_TEST_SCENARIO']
if 'send-keys' in sys.argv:
    if scenario == 'launch_failed':
        sys.exit(2)
    if scenario.startswith('paused_again'):
        (root / 'runtime/pause_request.json').write_text(json.dumps({
            'kind': 'provider_budget', 'reason': 'new exhaustion on resumed request'}))
    if scenario in ('running', 'paused_again_alive'):
        (root / 'alive').touch()
""",
    }
    for name, body in programs.items():
        target = bin_dir / name
        target.write_text(body)
        target.chmod(0o700)
    env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}",
        "PAUSE_TEST_ROOT": str(tmp_path), "PAUSE_TEST_SCENARIO": scenario}
    if scenario == "interrupted_resume":
        status = subprocess.run([
            "bash", str(ROOT / "scripts/trellis_pause.sh"), "status", str(runtime), str(repo),
        ], env=env, text=True, capture_output=True, check=True)
        assert json.loads(status.stdout)["request"] == original
        assert json.loads(status.stdout)["resumable"] is True
    result = subprocess.run([
        "bash", str(ROOT / "scripts/trellis_pause.sh"), "resume", str(runtime), str(repo),
    ], env=env,
        text=True, capture_output=True, timeout=15)
    assert result.returncode == code, result.stdout + result.stderr
    if new_pause:
        assert json.loads(request.read_text())["reason"] == "new exhaustion on resumed request"
    elif scenario == "running":
        assert not request.exists()
    else:
        assert json.loads(request.read_text()) == original
    assert not list(runtime.glob("pause_request.json.resuming.*"))


def test_complete_launch_recipe_survives_stale_server_and_repeated_exhaustion(tmp_path):
    from trellis.launch_verification import capture_launch_environment
    runtime, repo, commands = (tmp_path / name for name in ("runtime", "repo", "bin"))
    for path in (runtime, repo, commands): path.mkdir()
    state = b'{"cycle":7,"request_seq":42,"in_flight_request":{"id":42,"cycle":7,"kind":"Worker"}}'
    (runtime / "protocol_state.json").write_bytes(state)
    (runtime / "pause_request.json").write_text('{"kind":"provider_budget","reason":"first pause"}')
    kernel = tmp_path / "kernel"; kernel.write_bytes(b"pinned kernel")
    expected = {"TRELLIS_LEAN_PARALLELISM":"3", "TRELLIS_BURST_LEAN_PARALLELISM":"3",
                "LEAN_NUM_THREADS":"3", "TRELLIS_TRELLIS_KERNEL_CMD":str(kernel),
                "TRELLIS_KERNEL_CACHE_ROOT":str(runtime), "TRELLIS_CHECKER_SOCKET":str(runtime / "sockets/checker.sock")}
    capture_launch_environment(runtime, {**os.environ, **expected, "TRELLIS_TMUX_SOCKET":"private-held"}, ROOT,
                               tmux_session="held", cwd=str(ROOT))
    launch_path = runtime / "launch_env.json"
    launch = json.loads(launch_path.read_text())
    runner = tmp_path / "runner.sh"
    runner.write_text("#!/bin/sh\npython3 - \"$2\" <<'PY'\n" + '''import json,os,sys
from pathlib import Path
root=Path(sys.argv[1])
keys=json.loads(os.environ['EXPECTED_KEYS'])
(root/'observed.json').write_text(json.dumps({key:os.environ[key] for key in keys}))
(root/'pause_request.json').write_text(json.dumps({'kind':'provider_budget','reason':'renewed exhaustion'}))
''' + "PY\n")
    launch["trellis_sh"] = str(runner); launch_path.write_text(json.dumps(launch))
    for name, body in {"pgrep":"#!/bin/sh\nexit 1\n", "sleep":"#!/bin/sh\nexit 0\n", "tmux":'''#!/usr/bin/env python3
import os,subprocess,sys
assert sys.argv[1:3] == ['-L','private-held'], sys.argv
if 'send-keys' in sys.argv:
    subprocess.run(['bash',os.environ['TEST_RUNTIME']+'/resume_launch.sh'],check=True,
        env={**os.environ,**{k:'stale' for k in __import__('json').loads(os.environ['EXPECTED_KEYS'])}})
'''}.items():
        path = commands / name; path.write_text(body); path.chmod(0o700)
    env = {**os.environ, "PATH":f"{commands}:{os.environ['PATH']}", "TEST_RUNTIME":str(runtime),
           "EXPECTED_KEYS":json.dumps(list(expected))}
    for _ in range(2):
        result = subprocess.run(["bash",ROOT / "scripts/trellis_pause.sh","resume",runtime,repo],
                                env=env,capture_output=True,text=True,timeout=15)
        assert result.returncode == 4, result.stdout + result.stderr
        assert json.loads((runtime / "observed.json").read_text()) == expected
        assert json.loads((runtime / "pause_request.json").read_text())["reason"] == "renewed exhaustion"
        assert (runtime / "protocol_state.json").read_bytes() == state
    kernel.write_bytes(b"replacement kernel")
    result = subprocess.run(["bash",ROOT / "scripts/trellis_pause.sh","resume",runtime,repo],
                            env=env,capture_output=True,text=True,timeout=15)
    assert result.returncode == 3
    assert "runtime executable has changed" in result.stderr

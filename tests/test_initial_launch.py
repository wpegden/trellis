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
@pytest.mark.parametrize("pane", [None, "%17"])
def test_production_trellis_shell_captures_its_actual_runtime(tmp_path, ambient_root, pane):
    import subprocess
    source = Path(__file__).resolve().parents[1]
    runtime, repo = tmp_path / "runtime", tmp_path / "repo"
    runtime.mkdir(); repo.mkdir()
    (runtime / "protocol_state.json").write_text('{}')
    # Avoid source-archive work; everything else in the production run launcher,
    # including token registration and environment capture, runs unchanged.
    (runtime / "trellis-source-snapshot/test-fixture").mkdir(parents=True)
    cli = tmp_path / "kernel-fixture"
    cli.write_text("#!/usr/bin/env python3\n" + '''import json,os,sys
from pathlib import Path
from first_launch_fixture import write_pause
request = {'action':'read_modeling_disposition_state'} if sys.argv[1:2] == ['read-modeling-disposition-state'] else json.load(sys.stdin)
if request['action'] == 'read_modeling_disposition_state':
    print(json.dumps({'status':'modeling_disposition_state_ok','output':{}}))
    sys.exit(0)
assert request['action']=='run'
root=Path(request['root'])
assert os.environ['TRELLIS_KERNEL_CACHE_ROOT']==str(root)
write_pause(root, root.parent/'repo', sys.argv[0], capture=False)
''')
    cli.chmod(0o700)
    commands = tmp_path / "bin"
    commands.mkdir()
    tmux = commands / "tmux"
    tmux.write_text("#!/usr/bin/env python3\n" + '''import sys
print('trellis-run-fixture' if '-t' in sys.argv and sys.argv[sys.argv.index('-t')+1] == '%17' else 'unrelated-viewer')
''')
    tmux.chmod(0o700)
    env = {**os.environ, "PYTHONPATH":f"{source / 'tests'}:{source}",
           "PATH":f"{commands}:{os.environ['PATH']}", "TMUX":"inherited-tmux-server",
           "TRELLIS_REVIEWER_SOURCE_SHA":"test-fixture", "TRELLIS_TRELLIS_KERNEL_CMD":str(cli),
           "TRELLIS_CHECKER_SOCKET":str(runtime / "sockets/checker.sock"),
           "TRELLIS_LAUNCH_ATTEMPT":arm_initial_launch(runtime, repo, str(cli))}
    env.pop("TRELLIS_KERNEL_CACHE_ROOT", None)
    env.pop("TMUX_PANE", None)
    if pane: env["TMUX_PANE"] = pane
    if ambient_root: env["TRELLIS_KERNEL_CACHE_ROOT"] = ambient_root
    result = subprocess.run(["bash",source / "scripts/trellis.sh","run",runtime],
                            env=env,capture_output=True,text=True,timeout=20)
    assert result.returncode == 0, result.stdout + result.stderr
    launch = json.loads((runtime / "launch_env.json").read_text())
    assert launch["env"]["TRELLIS_KERNEL_CACHE_ROOT"] == str(runtime)
    assert launch["tmux_session"] == ("trellis-run-fixture" if pane else None)
    assert initial_budget_pause(runtime, repo, str(cli))


@pytest.mark.parametrize('already_enabled', [False, True])
def test_production_launch_enables_configured_parallel_dispatch_before_work(tmp_path, already_enabled):
    import subprocess
    source = Path(__file__).resolve().parents[1]
    runtime = tmp_path / 'runtime'
    runtime.mkdir()
    (runtime / 'protocol_state.json').write_text(json.dumps({'dispatch': {} if already_enabled else None}))
    config = tmp_path / 'config.json'
    config.write_text(json.dumps({'parallel_dispatch': {'enabled': True}}))
    (runtime / 'runtime_metadata.json').write_text(json.dumps({'config_path': str(config), 'repo_path':str(tmp_path/'repo')}))
    (runtime / 'trellis-source-snapshot/test-fixture').mkdir(parents=True)
    commands = tmp_path / 'bin'
    commands.mkdir()
    events = tmp_path / 'dispatch-actions'
    shim = commands / 'python3'
    shim.write_text(f'#!{sys.executable}\n' + '''import json,os,sys
from pathlib import Path
if sys.argv[1:3] == ['-m', 'trellis.dispatch']:
    action, runtime = sys.argv[3], Path(sys.argv[4])
    assert os.environ['TRELLIS_RUNTIME_CHECKPOINT_HOOK'].endswith('trellis_git_checkpoint_hook.sh')
    state = json.loads((runtime/'protocol_state.json').read_text())
    if action == 'enable':
        assert state['dispatch'] is None
        state['dispatch'] = {}
        (runtime/'protocol_state.json').write_text(json.dumps(state))
    else:
        assert action == 'run' and state['dispatch'] is not None
    with open(os.environ['DISPATCH_ACTIONS'], 'a') as log: log.write(action+'\\n')
    sys.exit(0)
os.execv(sys.executable, [sys.executable, *sys.argv[1:]])
''')
    shim.chmod(0o700)
    cli = tmp_path / 'kernel-fixture'
    cli.write_text(f'#!{sys.executable}\n' + '''import json,sys
request = {'action':'read_modeling_disposition_state'} if sys.argv[1:2] == ['read-modeling-disposition-state'] else json.load(sys.stdin)
assert request['action']=='read_modeling_disposition_state', 'serial dispatch must not start'
print(json.dumps({'status':'modeling_disposition_state_ok','output':{}}))
''')
    cli.chmod(0o700)
    env = {**os.environ, 'PATH': f'{commands}:{os.environ["PATH"]}',
           'PYTHONPATH':str(source), 'TRELLIS_REVIEWER_SOURCE_SHA':'test-fixture',
           'TRELLIS_TRELLIS_KERNEL_CMD':str(cli), 'DISPATCH_ACTIONS':str(events)}
    result = subprocess.run(['bash', source/'scripts/trellis.sh', 'run', runtime],
                            env=env, text=True, capture_output=True, timeout=20)
    assert result.returncode == 0, result.stdout + result.stderr
    assert events.read_text().splitlines() == (['run'] if already_enabled else ['enable', 'run'])

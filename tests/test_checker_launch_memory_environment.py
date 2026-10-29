"""Execute production launch forwarding with a stale tmux environment."""
import json
import os
from pathlib import Path
import subprocess
import sys

import pytest

ROOT = Path(__file__).resolve().parents[1]
DEFAULTS = {
    'TRELLIS_CHECK_MEMORY_POLICY': 'conservative',
    'TRELLIS_CHECK_MEMORY_POLICY_GENERATION': '0',
    'TRELLIS_CHECK_CGROUP': 'off',
    'TRELLIS_CHECK_COMPILER_SWAP_GIB': '2',
    'TRELLIS_CHECK_COMPILER_AGGREGATE_GIB': '0',
    'TRELLIS_CHECK_MAX_JOB_GIB': '48',
    'TRELLIS_CHECK_SERIAL_MAX_JOB_GIB': '56',
    'TRELLIS_CHECK_SERIAL_FLOOR_GIB': '4',
    'TRELLIS_CHECK_SERIAL_WAIT_SECONDS': '300',
}
SELECTED = dict(zip(DEFAULTS, ('optimistic', "policy 9 'quoted'", 'delegated', '0', '42', '40', '55', '3', '90')))
STALE = dict(zip(DEFAULTS, ('optimistic', 'old-policy', 'delegated', '8', '58', '56', '61', '9', '999')))


def caller(selected):
    env = {key: value for key, value in os.environ.items() if key not in DEFAULTS}
    env.update(selected)
    return env


@pytest.mark.parametrize('selected', [SELECTED, {}, dict.fromkeys(DEFAULTS, '')])
def test_restart_forwards_to_checker_and_supervisor_over_stale_tmux(tmp_path, selected):
    script = (ROOT / 'scripts/restart_configured_run.sh').read_text()
    block = script[script.index('SESSION_ENV=()'):script.index('# Wait up to 30 s')]
    # Execute the real supervisor launch statement too; both must receive the
    # same array, not just an inert list of variable names in the source.
    supervisor = script[script.index('if [[ "$RUN_AFTER" -eq 1 ]]; then', script.index('SUPERVISOR_CHECKER_ENV=')):]
    supervisor = supervisor[:supervisor.index('\nfi') + 3]
    capture = tmp_path / 'tmux.py'
    output = tmp_path / 'launches.jsonl'
    capture.write_text('''import json,os,sys
args=sys.argv[1:]
env=json.loads(os.environ['STALE_MEMORY'])
for index,arg in enumerate(args):
    if arg == '-e':
        key,value=args[index+1].split('=',1)
        env[key]=value
with open(os.environ['CAPTURE_OUTPUT'],'a') as stream:
    stream.write(json.dumps(dict(args=args,environment=env))+'\\n')
''')
    env = caller(selected)
    env.update(STALE_MEMORY=json.dumps(STALE), CAPTURE_OUTPUT=str(output),
        CAPTURE_SCRIPT=str(capture), PYTHON_EXE=sys.executable, ROOT_DIR=str(ROOT),
        CHECKER_SESSION='checker', RUN_SESSION='supervisor', RUN_AFTER='1',
        RUNTIME_ROOT=str(tmp_path / 'runtime'), TRELLIS_TMUX_SOCKET='isolated-test',
        SUPERVISOR_CHECKER_ENV='')
    subprocess.run(['bash', '-eu', '-c',
        'tmux_cmd() { "$PYTHON_EXE" "$CAPTURE_SCRIPT" "$@"; }\n' + block + supervisor],
        env=env, check=True, capture_output=True, text=True)
    rows = [json.loads(line) for line in output.read_text().splitlines()]
    assert len(rows) == 2
    expected = {key: selected.get(key) or value for key, value in DEFAULTS.items()}
    for row in rows:
        assert {key: row['environment'][key] for key in DEFAULTS} == expected
        assert row['args'][0] == 'new-session'
    assert rows[0]['args'][rows[0]['args'].index('-s') + 1] == 'checker'
    assert rows[1]['args'][rows[1]['args'].index('-s') + 1] == 'supervisor'


@pytest.mark.parametrize('script_name', ['trellis_create_run.sh'])
@pytest.mark.parametrize('selected', [SELECTED, {}, dict.fromkeys(DEFAULTS, '')])
def test_creation_phase_snapshot_overrides_stale_tmux(tmp_path, script_name, selected):
    script = (ROOT / 'scripts' / script_name).read_text()
    start = script.index('python3 - "$PHASE_ENV" <<\'PY\'\n')
    code = script[start:].split('\n', 1)[1].split('\nPY\n', 1)[0]
    snapshot = tmp_path / 'phase_env.sh'
    subprocess.run([sys.executable, '-c', code, str(snapshot)], env=caller(selected), check=True)
    read_env = tmp_path / 'read.py'
    read_env.write_text('import json,os; print(json.dumps(dict(os.environ)))')
    stale = caller(STALE)
    result = subprocess.run(['bash', '-eu', '-c', 'source "$1"; exec "$2" "$3"',
        'phase', str(snapshot), sys.executable, str(read_env)], env=stale,
        check=True, capture_output=True, text=True)
    actual = json.loads(result.stdout)
    assert {key: actual[key] for key in DEFAULTS} == {
        key: selected.get(key) or value for key, value in DEFAULTS.items()}

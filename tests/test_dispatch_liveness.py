import json
import os
from pathlib import Path
import subprocess
import sys
import time

import pytest

from trellis.dispatch.liveness import coordinator_pid


@pytest.mark.parametrize('command', [
    b'python3\0-m\0trellis.dispatch\0status\0RUNTIME\0',
    b'python3\0-m\0trellis.dispatch.process\0RUNTIME\0',
    b'python3\0-m\0trellis.dispatch\0run\0RUNTIME-other\0',
    b'python3\0-c\0sleep()\0',
    b'',
])
def test_stale_coordinator_record_does_not_claim_liveness(tmp_path, command):
    runtime, proc = tmp_path / 'runtime', tmp_path / 'proc'
    (runtime / 'dispatch').mkdir(parents=True)
    (runtime / 'dispatch/coordinator.json').write_text('{"pid":123}')
    (proc / '123').mkdir(parents=True)
    (proc / '123/cmdline').write_bytes(command.replace(b'RUNTIME', os.fsencode(runtime)))
    assert coordinator_pid(runtime, proc_root=proc) is None


@pytest.mark.parametrize('action', ['run', 'resume'])
def test_pause_script_recognizes_parallel_coordinator_and_rejects_duplicate_resume(tmp_path, action):
    root = Path(__file__).resolve().parents[1]
    runtime, repo, stub = (tmp_path / name for name in ('runtime', 'repo', 'stub'))
    (runtime / 'dispatch').mkdir(parents=True)
    repo.mkdir()
    (stub / 'trellis').mkdir(parents=True)
    (stub / 'trellis/__init__.py').write_text('')
    (stub / 'trellis/dispatch.py').write_text('''import json,os,sys,time
from pathlib import Path
(Path(sys.argv[2]) / 'dispatch/coordinator.json').write_text(json.dumps({'pid':os.getpid()}))
time.sleep(30)
''')
    process = subprocess.Popen([sys.executable, '-m', 'trellis.dispatch', action, str(runtime)],
        cwd=stub, env={**os.environ, 'PYTHONPATH': str(stub)})
    try:
        for _ in range(100):
            if coordinator_pid(runtime) == process.pid:
                break
            time.sleep(.02)
        assert coordinator_pid(runtime) == process.pid
        status = subprocess.run(['bash', root / 'scripts/trellis_pause.sh', 'status', runtime, repo],
            capture_output=True, text=True, check=True)
        value = json.loads(status.stdout)
        assert value['state'] == 'running'
        assert value['wrapper_pid'] == process.pid
        resume = subprocess.run(['bash', root / 'scripts/trellis_pause.sh', 'resume', runtime, repo],
            capture_output=True, text=True)
        assert resume.returncode == 2 and 'already running' in resume.stderr
    finally:
        process.terminate()
        process.wait(timeout=5)
    assert coordinator_pid(runtime) is None

"""Real standalone carriers share CPU tokens, preserve tool inputs and drain leaves."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import pytest

from trellis.checker import admission, compute_budget, compute_leaf, memory_retry
from trellis.modeling.environment import run_source_command, environment_signature
from trellis.pv_execution import run_compute, owned_process


@pytest.fixture
def host(tmp_path, monkeypatch):
    if not Path(f'/run/user/{os.getuid()}/bus').exists():
        pytest.skip('requires delegated user manager')
    monkeypatch.delenv('TRELLIS_COMPILER_CAPABILITY', raising=False)
    for key, value in dict(TRELLIS_CHECK_HOST_CPU_POOL=str(tmp_path / 'pool'),
            TRELLIS_CHECK_HOST_CPU_BUDGET='1', TRELLIS_CHECK_CPU_BUDGET='1',
            TRELLIS_CHECK_JOBS_PARALLELISM='1', TRELLIS_CHECK_JOBS_FLOOR_GIB='.1',
            TRELLIS_CHECK_MAX_JOB_GIB='1', TRELLIS_CHECK_COMPILER_SWAP_GIB='0',
            TRELLIS_CHECK_SERIAL_WAIT_SECONDS='3').items():
        monkeypatch.setenv(key, value)
    return tmp_path


def test_exact_environment_exit_and_execution_timeout(host):
    command = ['/usr/bin/python3', '-c', 'import os,json; print(json.dumps(dict(os.environ))); raise SystemExit(7)']
    result = run_compute(command, env={'EXACT': 'yes'}, capture_output=True, timeout=2)
    assert result.returncode == 7
    # Python itself may add LC_CTYPE; no carrier control/auth variables leak.
    assert {k:v for k,v in json.loads(result.stdout).items() if k != 'LC_CTYPE'} == {'EXACT': 'yes'}
    result = run_source_command(['/usr/bin/python3', '-c', 'import time; time.sleep(20)'],
        cwd=host, env={}, stdout=host / 'timeout.log', timeout=.15)
    assert result['timeout'] and result['exit_code'] != 0


def test_cpu_queue_does_not_spend_execution_timeout(host):
    pool = compute_budget.HostCpuPool(host / 'pool', 1)
    held = pool.acquire(1)
    try:
        with ThreadPoolExecutor(1) as threads:
            future = threads.submit(run_compute, ['/bin/true'], timeout=.2, capture_output=True)
            time.sleep(.5)
            assert not future.done()
            pool.release(held)
            held = []
            assert future.result(8).returncode == 0
    finally:
        pool.release(held)


def test_own_cap_interruption_is_capacity_deferral_not_tool_failure(host, monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_MAX_JOB_GIB', '.0625')
    with pytest.raises(memory_retry.ResourceOutcome) as caught:
        run_source_command(['/usr/bin/python3', '-c', 'x=bytearray(256*1024*1024)'],
            cwd=host, env={}, stdout=host / 'oom.log', timeout=5)
    assert caught.value.detail['kind'] == 'host_pressure_requeue'
    assert caught.value.detail['exhausted_reason'] == 'helper_requires_repreparation'
    # No successful result or failure verdict was returned; the host reparses inputs.
    pool = compute_budget.HostCpuPool(host / 'pool', 1)
    tokens = pool.acquire(1)
    assert len(tokens) == 1
    pool.release(tokens)


def test_cancel_drains_independent_child_session_before_releasing_capacity(host):
    pidfile = host / 'child.pid'
    code = ('import subprocess,pathlib,time; p=subprocess.Popen(["/bin/sleep","60"],'
            'start_new_session=True); pathlib.Path(' + repr(str(pidfile)) + ').write_text(str(p.pid)); time.sleep(60)')
    with pytest.raises(ValueError, match='cancel'):
        with owned_process(['/usr/bin/python3', '-c', code], compute_timeout=30,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as process:
            deadline = time.monotonic() + 5
            while not pidfile.exists() and time.monotonic() < deadline:
                time.sleep(.05)
            assert pidfile.exists()
            raise ValueError('cancel')
    pid = pidfile.read_text()
    status = Path('/proc') / pid / 'stat'
    assert not status.exists() or status.read_text().split(') ', 1)[1].split()[0] == 'Z'
    pool = compute_budget.HostCpuPool(host / 'pool', 1)
    held = pool.acquire(1)
    assert len(held) == 1
    pool.release(held)


def test_admitted_descendants_inherit_without_another_host_token(host):
    result = run_compute([sys.executable, '-c',
        'from trellis.checker.compute_leaf import host_pool; assert host_pool() is None'],
        capture_output=True, timeout=3)
    assert result.returncode == 0, result.stderr


def test_cooperative_hash_wait_is_bounded_and_does_not_publish(host, monkeypatch):
    monkeypatch.setenv('TRELLIS_CHECK_SERIAL_WAIT_SECONDS', '.1')
    pool = compute_budget.HostCpuPool(host / 'pool', 1)
    held = pool.acquire(1)
    cache = host / 'cache.json'
    try:
        with pytest.raises(memory_retry.ResourceOutcome):
            environment_signature({'lean': '/bin/true'}, cache_path=cache)
        assert not cache.exists()
    finally:
        pool.release(held)
    signature = environment_signature({'lean': '/bin/true'}, cache_path=cache)
    assert len(signature) == 64 and cache.is_file()

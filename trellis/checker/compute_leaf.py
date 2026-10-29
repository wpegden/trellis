"""Host-only adapter for standalone deterministic source tools.

Only the leaf moves into a delegated scope. The caller, its inputs, and its
publication logic remain in their existing process. The tool's environment
and sandbox command are passed verbatim; CPU quotas cover tools which clear
their environment. This is a local launcher, never a worker RPC endpoint.
"""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def _managed_tree():
    parts = Path('/proc/self/cgroup').read_text().strip().split('/')
    return any(a == 'compilers' and b.startswith('job-') for a, b in zip(parts, parts[1:]))


def host_pool():
    # Worker commands retain their existing confined/managed route. This
    # launcher cannot turn a worker environment into a host capability.
    if (os.environ.get('TRELLIS_CHECK_STANDALONE_COMPUTE') == 'off'
            or os.environ.get('TRELLIS_COMPILER_CAPABILITY') in {'managed', 'none'} or _managed_tree()):
        return None
    explicit = os.environ.get('TRELLIS_CHECK_HOST_CPU_POOL')
    directory = Path(explicit) if explicit else Path.home() / '.cache/trellis/host-cpu-v1'
    config = directory / 'capacity.json'
    if config.exists():
        return directory, int(json.loads(config.read_text())['cpu_units'])
    if explicit:
        from .compute_budget import capacity, positive_setting
        return directory, positive_setting('TRELLIS_CHECK_HOST_CPU_BUDGET', capacity()['default_cpu_budget'])
    return None


class Launch:
    def __init__(self, request, environment, wait_seconds):
        self.request = request
        self.status = request.with_name('status.json')
        self.environment = environment
        self.wait_seconds = wait_seconds
        self.command = [sys.executable, '-m', 'trellis.checker.compute_leaf', str(request)]
        self.scope = None

    def read_status(self):
        return json.loads(self.status.read_text()) if self.status.exists() else {}

    def outcome(self, value):
        if value.get('phase') == 'deferred':
            from .memory_retry import ResourceOutcome
            raise ResourceOutcome(value['detail'])
        if value.get('phase') == 'infrastructure_error':
            raise RuntimeError('deterministic tool infrastructure failed: ' + value['message'])

    def entered(self, process):
        deadline = time.monotonic() + self.wait_seconds + 30
        while True:
            value = self.read_status()
            self.scope = value.get('scope', self.scope)
            self.outcome(value)
            if value.get('phase') in {'executing', 'finished'}:
                return
            if process.poll() is not None:
                raise RuntimeError('deterministic tool admission carrier exited before execution')
            if time.monotonic() >= deadline:
                raise RuntimeError('deterministic tool admission carrier did not report its state')
            time.sleep(.05)

    def finished(self, process):
        value = self.read_status()
        self.outcome(value)
        if value.get('timed_out'):
            request = json.loads(self.request.read_text())
            raise subprocess.TimeoutExpired(request['command'], request['timeout'])
        if process.poll() is None:
            raise RuntimeError('deterministic tool caller returned before draining its process')

    def cancel(self, process):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                # The scope belongs to this exact carrier, not its caller.
                # Kill all descendants together before reclaiming its grant.
                value = self.read_status()
                scope = value.get('scope', self.scope)
                if scope:
                    (Path(scope) / 'cgroup.kill').write_text('1')
                else:
                    process.kill()
                process.wait(timeout=10)


@contextmanager
def launch(command, *, environment, cwd, timeout):
    pool = host_pool()
    if pool is None:
        yield None
        return
    directory, units = pool
    wait = float(os.environ.get('TRELLIS_CHECK_SERIAL_WAIT_SECONDS', '300'))
    with tempfile.TemporaryDirectory(prefix='trellis-compute-leaf-') as temporary:
        request = Path(temporary) / 'request.json'
        request.write_text(json.dumps(dict(command=list(map(os.fspath, command)),
            cwd=os.fspath(cwd or Path.cwd()), environment=dict(os.environ if environment is None else environment),
            timeout=timeout)))
        request.chmod(0o600)
        source = str(Path(__file__).resolve().parents[2])
        env = dict(os.environ, PYTHONPATH=source + os.pathsep + os.environ.get('PYTHONPATH', ''),
            TRELLIS_CHECK_HOST_CPU_POOL=str(directory), TRELLIS_CHECK_HOST_CPU_BUDGET=str(units),
            TRELLIS_CHECK_CPU_BUDGET=str(units), TRELLIS_CHECK_CGROUP='delegated',
            TRELLIS_CHECK_CPU_CONTROL='cgroup', TRELLIS_CHECK_MEMORY_POLICY='available')
        env.pop('TRELLIS_CHECK_DELEGATED_BOOTSTRAP', None)
        yield Launch(request, env, wait)


def main():
    from . import admission, delegation
    from .protocol import ProtocolError
    from trellis.json_io import save_json
    request_path = Path(sys.argv[1])
    status_path = request_path.with_name('status.json')
    scope = None
    def status(phase, **values):
        save_json(status_path, dict(phase=phase, scope=scope, **values), mode=0o600)
    def cancelled(signum, frame):
        raise KeyboardInterrupt('deterministic tool cancelled')
    signal.signal(signal.SIGTERM, cancelled)
    try:
        delegation.ensure([str(request_path)], module='trellis.checker.compute_leaf')
        admission.ACTIVE = admission.configured()
        scope = str(admission.ACTIVE.groups.root)
        status('queued')
        request = json.loads(request_path.read_text())
        outcomes = []
        @admission.admitted
        def execute():
            status('executing')
            return admission.guarded_run(request['command'], timeout=request['timeout'],
                cwd=request['cwd'], env=request['environment'])
        try:
            with admission.authoritative_workspace('standalone-source-tool', capacity_failure=outcomes.append):
                result = execute()
        except ProtocolError:
            if not outcomes:
                raise
            status('deferred', detail=outcomes[-1])
            return 75
        except subprocess.TimeoutExpired:
            status('finished', timed_out=True)
            return 124
        status('finished', returncode=result.returncode)
        if result.returncode < 0:
            signum = -result.returncode
            signal.signal(signum, signal.SIG_DFL)
            os.kill(os.getpid(), signum)
        return result.returncode
    except Exception as error:
        status('infrastructure_error', message=str(error))
        return 125


if __name__ == '__main__':
    raise SystemExit(main())

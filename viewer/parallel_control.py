"""Apply operator admission limits through the run's native event journal."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from trellis.dispatch.native import NativeLedger
from trellis.runtime.bridge import _mint_burst_token, _register_burst_token, _unregister_burst_token


@contextmanager
def control_token(runtime):
    token = _mint_burst_token()
    _register_burst_token(runtime, token=token, burst_id='viewer-agent-limits',
                          kind='viewer_capacity', request_id=0, cycle=0)
    try:
        yield token
    finally:
        _unregister_burst_token(runtime, token=token)


def capacity_event(value):
    fields = {'expected_worker_slots', 'expected_heavy_worker_slots', 'worker_slots', 'heavy_worker_slots'}
    if not isinstance(value, dict) or set(value) != fields:
        raise ValueError('Supply the current and requested worker and heavy-agent limits')
    for name, limit in value.items():
        if name == 'expected_heavy_worker_slots' and limit is None:
            continue
        if type(limit) is not int or not 1 <= limit <= 1024:
            raise ValueError('Agent limits must be whole numbers from 1 to 1024')
    return dict(kind='agent_capacity_changed', **value)


def change_limits(runtime, value):
    event = capacity_event(value)
    runtime = Path(runtime).resolve()
    launch = json.loads((runtime / 'launch_env.json').read_text())
    if not launch.get('runtime_root') or Path(launch['runtime_root']).resolve() != runtime:
        raise ValueError('The recorded control environment belongs to a different runtime')
    # The active run chooses its deployed kernel and synchronous checkpoint
    # hook. A viewer update alone cannot switch a running process's kernel.
    env = dict(launch['env'])
    if not env.get('TRELLIS_TRELLIS_KERNEL_CMD'):
        raise ValueError('This run has no recorded native control environment')
    if not env.get('TRELLIS_RUNTIME_CHECKPOINT_HOOK'):
        # The coordinator supplies this default after launch capture. Use the
        # run's deployment, never the newer viewer's checkpoint implementation.
        root = launch.get('trellis_root')
        hook = Path(root) / 'scripts/trellis_git_checkpoint_hook.sh' if root else None
        if hook is None or not hook.is_absolute() or not hook.is_file():
            raise ValueError('This run has no recorded native checkpoint hook')
        env['TRELLIS_RUNTIME_CHECKPOINT_HOOK'] = str(hook)
    socket = runtime / 'sockets/checker.sock'
    recorded_socket = env.get('TRELLIS_CHECKER_SOCKET')
    if recorded_socket and Path(recorded_socket).resolve() != socket.resolve():
        raise ValueError('The recorded checker socket belongs to a different runtime')
    env['TRELLIS_CHECKER_SOCKET'] = str(socket)
    env['TRELLIS_KERNEL_CACHE_ROOT'] = str(runtime)
    with control_token(runtime) as token:
        env['TRELLIS_CHECKER_TOKEN'] = token
        previous = {key: os.environ.get(key) for key in env}
        try:
            os.environ.update(env)
            # observe retains the existing native lock, publication guard and
            # synchronous checkpoint. Lower limits affect future admission only.
            result = NativeLedger(runtime).observe(event)
        finally:
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
    return {'worker_slots': result['config']['worker_slots'],
            'heavy_worker_slots': result['config']['heavy_worker_slots']}


if __name__ == '__main__':
    try:
        print(json.dumps(change_limits(sys.argv[1], json.load(sys.stdin))))
    except (OSError, ValueError, RuntimeError, KeyError) as error:
        print(json.dumps({'error': str(error)}))
        sys.exit(1)

"""Exclusive memory delegation without changing checker PID or launch ownership.

A process session is not a cgroup: Popen(start_new_session=True) inside a
rollout service still shares that service's cgroup. Only an exclusive root
may be adopted. Rollouts should preflight before stopping a healthy checker,
and supervise the coordinator separately (its worker lifetime is independent).
"""
from __future__ import annotations

import os
from pathlib import Path
import sys

_ATTEMPTED = 'TRELLIS_CHECK_DELEGATED_BOOTSTRAP'


def current_root():
    relative = next(line[3:] for line in Path('/proc/self/cgroup').read_text().splitlines()
                    if line.startswith('0::'))
    return Path('/sys/fs/cgroup') / relative.lstrip('/')


def delegated(root):
    return (f'user@{os.getuid()}.service' in root.parts
        and root.suffix in {'.service', '.scope'}
        and os.access(root / 'cgroup.subtree_control', os.W_OK)
        and {'memory', *({'cpu'} if os.environ.get('TRELLIS_CHECK_CPU_CONTROL') == 'cgroup' else set())}
            <= set((root / 'cgroup.controllers').read_text().split()))


def exclusive(root):
    """Read-only admission check; compiler_cgroup retains the final guard."""
    try:
        if (root / 'cgroup.procs').read_text().split() != [str(os.getpid())]:
            return False
        # populated includes all descendants, so no recursive cgroup walk.
        for child in root.iterdir():
            if child.is_dir():
                events = dict(line.split() for line in (child / 'cgroup.events').read_text().splitlines())
                if events.get('populated') != '0':
                    return False
        return True
    except (OSError, ValueError):
        return False


def ensure(argv, *, module='trellis.checker.server', fresh=False):
    if os.environ.get('TRELLIS_CHECK_CGROUP', 'off') != 'delegated':
        return
    attempted = os.environ.get(_ATTEMPTED) == str(os.getpid())
    root = current_root()
    if delegated(root) and exclusive(root) and (not fresh or attempted):
        return
    if attempted:
        raise RuntimeError('checker memory delegation did not provide a writable memory controller')
    uid = os.getuid()
    runtime = Path(f'/run/user/{uid}')
    if not (runtime / 'bus').exists():
        raise RuntimeError('checker memory delegation requires the current user systemd manager')
    env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime),
        DBUS_SESSION_BUS_ADDRESS=f'unix:path={runtime}/bus', **{_ATTEMPTED: str(os.getpid())})
    # A scope execs the command in this PID and inherits its environment,
    # working directory and file descriptors. The existing launcher, pidfile,
    # signals and cleanup continue to own the actual checker process.
    controllers = 'memory cpu' if os.environ.get('TRELLIS_CHECK_CPU_CONTROL') == 'cgroup' else 'memory'
    command = ['systemd-run', '--user', '--quiet', '--scope', '--collect',
        '--no-ask-password', '--expand-environment=no', '--property=Delegate=' + controllers,
        '--', sys.executable, '-m', module, *argv]
    os.execvpe(command[0], command, env)


def preflight():
    """Exercise fresh delegated controls without opening any runtime/socket."""
    import json
    from .compiler_cgroup import configured
    if os.environ.get('TRELLIS_CHECK_CGROUP') != 'delegated':
        raise RuntimeError('preflight requires TRELLIS_CHECK_CGROUP=delegated')
    ensure([], module=__spec__.name, fresh=True)
    groups = configured()
    group = groups.create(64 * 2**20)
    group.close()
    print(json.dumps(dict(ready=True, pid=os.getpid(), delegated_root=str(groups.root),
                         runtime_mutations=False)))


if __name__ == '__main__':
    preflight()

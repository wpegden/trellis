"""Optional delegated compiler groups; no privileged helper or detached jobs.

Only the checker creates limits. The small exec carrier joins before executing
bwrap, retaining Popen identity and descendant ownership. Agent sandboxes do
not mount the host cgroup filesystem.
"""
from __future__ import annotations

import os
from pathlib import Path
import sys
import time
import uuid


def _values(path):
    return dict((key, int(value)) for key, value in
                (line.split() for line in path.read_text().splitlines()))


class CompilerGroup:
    def __init__(self, root, limit_bytes, swap_bytes, *, cpu_control=False):
        self.cpu_control = cpu_control
        self.path = root / ('job-' + uuid.uuid4().hex)
        self.path.mkdir()
        try:
            (self.path / 'memory.max').write_text(str(int(limit_bytes)) if limit_bytes else 'max')
            (self.path / 'memory.swap.max').write_text(str(int(swap_bytes)))
            (self.path / 'memory.oom.group').write_text('1')
            self.events_before = _values(self.path / 'memory.events.local')
            self.limit_bytes = limit_bytes
        except BaseException:
            self.path.rmdir()
            raise

    def command(self, command):
        # This file is host-owned and executed before entering confinement.
        # Do not use preexec_fn in the multithreaded checker.
        return [sys.executable, str(Path(__file__).resolve()), str(self.path), '--', *command]

    def limit(self, value):
        # Reused warm groups begin a new lease; historical OOM events cannot
        # classify this command. Capture before lowering a limit.
        self.events_before = _values(self.path / 'memory.events.local')
        if value != self.limit_bytes:
            (self.path / 'memory.max').write_text(str(int(value)) if value else 'max')
            self.limit_bytes = value

    def sample(self):
        events = _values(self.path / 'memory.events.local')
        return dict(current_bytes=int((self.path / 'memory.current').read_text()),
                    peak_bytes=int((self.path / 'memory.peak').read_text()),
                    swap_bytes=int((self.path / 'memory.swap.current').read_text()),
                    peak_scope='compiler group lifetime; includes file cache',
                    max_events=events.get('max', 0) - self.events_before.get('max', 0),
                    oom_events=events.get('oom', 0) - self.events_before.get('oom', 0),
                    oom_kills=events.get('oom_kill', 0) - self.events_before.get('oom_kill', 0))

    def cpu_limit(self, units):
        if self.cpu_control:
            (self.path / 'cpu.max').write_text(f'{int(units) * 100000} 100000')

    def freeze(self, frozen):
        # Warm processes retain memory but release their CPU lease while idle.
        # Freezing the group also covers background descendants.
        if self.cpu_control:
            (self.path / 'cgroup.freeze').write_text('1' if frozen else '0')
            deadline = time.monotonic() + 10
            while bool(_values(self.path / 'cgroup.events').get('frozen')) != frozen:
                if time.monotonic() >= deadline:
                    raise RuntimeError('warm compiler group did not change freeze state')
                time.sleep(.01)

    def close(self):
        # A child can outlive its Popen leader. Never release the group's
        # capacity merely because that leader exited.
        if _values(self.path / 'cgroup.events').get('populated'):
            (self.path / 'cgroup.kill').write_text('1')
        deadline = time.monotonic() + 10
        while _values(self.path / 'cgroup.events').get('populated'):
            if time.monotonic() >= deadline:
                raise RuntimeError('compiler cgroup did not drain; admission remains closed')
            time.sleep(.02)
        self.path.rmdir()


class CompilerGroups:
    def __init__(self, delegated, *, aggregate_bytes=0, swap_bytes=2 * 2**30, cpu_control=False):
        self.root = Path(delegated).resolve()
        self.swap_bytes = swap_bytes
        self.cpu_control = cpu_control
        if 'memory' not in (self.root / 'cgroup.controllers').read_text().split():
            raise RuntimeError('checker service has no delegated memory controller')
        if cpu_control and 'cpu' not in (self.root / 'cgroup.controllers').read_text().split():
            raise RuntimeError('checker service has no delegated CPU controller')
        if (self.root / 'memory.oom.group').read_text().strip() != '0':
            raise RuntimeError('checker parent must use memory.oom.group=0; launch in a delegated scope or service with OOMPolicy=continue')
        # Caller initializes before launching checker workers. The checker
        # moves to a leaf so the parent can enable the memory controller.
        manager = self.root / 'manager'
        manager.mkdir(exist_ok=True)
        (manager / 'cgroup.procs').write_text(str(os.getpid()))
        if (self.root / 'cgroup.procs').read_text().strip():
            raise RuntimeError('delegated checker group still has direct processes')
        controls = '+memory +cpu' if cpu_control else '+memory'
        (self.root / 'cgroup.subtree_control').write_text(controls)
        self.jobs = self.root / 'compilers'
        self.jobs.mkdir(exist_ok=True)
        (self.jobs / 'memory.oom.group').write_text('0')
        (self.jobs / 'memory.max').write_text(str(int(aggregate_bytes)) if aggregate_bytes else 'max')
        (self.jobs / 'cgroup.subtree_control').write_text(controls)
        # Safe service restart: stale groups are only under our delegated
        # compiler subtree. Unit shutdown should already have drained them.
        for path in self.jobs.iterdir():
            if path.is_dir() and path.name.startswith('job-'):
                if _values(path / 'cgroup.events').get('populated'):
                    raise RuntimeError('previous compiler cgroup is populated; drain the checker service before restart')
                path.rmdir()

    def create(self, limit_bytes):
        return CompilerGroup(self.jobs, limit_bytes, self.swap_bytes, cpu_control=self.cpu_control)

    def headroom(self):
        """Read enclosing limits; never drain or change another service's groups."""
        result = float('inf')
        path = self.jobs
        while (path / 'memory.max').exists():
            limit = (path / 'memory.max').read_text().strip()
            if limit != 'max':
                current = int((path / 'memory.current').read_text())
                result = min(result, max(0, int(limit) - current))
            path = path.parent
        return result


def configured():
    cpu_control = os.environ.get('TRELLIS_CHECK_CPU_CONTROL', 'off')
    if cpu_control not in {'off', 'cgroup'}:
        raise ValueError('TRELLIS_CHECK_CPU_CONTROL must be off or cgroup')
    setting = os.environ.get('TRELLIS_CHECK_CGROUP', 'off')
    if setting == 'off':
        if cpu_control == 'cgroup':
            raise ValueError('CPU control requires delegated compiler groups')
        return None
    if setting != 'delegated':
        raise ValueError('TRELLIS_CHECK_CGROUP must be off or delegated')
    relative = next(line[3:] for line in Path('/proc/self/cgroup').read_text().splitlines()
                    if line.startswith('0::'))
    root = Path('/sys/fs/cgroup') / relative.lstrip('/')
    # Requiring an explicitly delegated launch prevents modifying arbitrary
    # login/session cgroups. systemd owns the service root, we own descendants.
    # Some supported user managers omit the optional user.delegate xattr.
    # Explicit host configuration plus writable controls in this UID's own
    # service is the capability; never attempt to manage a login/session root.
    if (f'user@{os.getuid()}.service' not in root.parts or not root.name.endswith(('.service', '.scope'))
            or not os.access(root / 'cgroup.subtree_control', os.W_OK)):
        raise RuntimeError('checker requires a delegated user service')
    return CompilerGroups(root,
        cpu_control=cpu_control == 'cgroup',
        aggregate_bytes=float(os.environ.get('TRELLIS_CHECK_COMPILER_AGGREGATE_GIB', '0')) * 2**30,
        swap_bytes=float(os.environ.get('TRELLIS_CHECK_COMPILER_SWAP_GIB', '2')) * 2**30)


def _exec():
    if len(sys.argv) < 4 or sys.argv[2] != '--':
        raise SystemExit('invalid compiler group carrier')
    root = Path(sys.argv[1])
    (root / 'cgroup.procs').write_text(str(os.getpid()))
    # exec preserves the existing directly tracked process identity. The
    # confined program cannot run/fork before membership is established.
    os.execvpe(sys.argv[3], sys.argv[3:], os.environ)


if __name__ == '__main__':
    try:
        _exec()
    except Exception as error:
        print(f'compiler group launch failed: {error}', file=sys.stderr)
        raise SystemExit(125) from None

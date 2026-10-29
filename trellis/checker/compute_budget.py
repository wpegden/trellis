"""Host CPU capacity and shared leases for deterministic checker work.

The lease files coordinate checker services belonging to the same operator.
Locks are held by the supervisor until its compiler tree has drained. They
carry no proof authority and are released by the kernel on supervisor exit.
"""
from __future__ import annotations

import fcntl
import json
import math
import os
from pathlib import Path


def capacity(*, affinity=None, sys_cpu=Path('/sys/devices/system/cpu'),
             cgroup=None):
    cpus = set(os.sched_getaffinity(0) if affinity is None else affinity)
    cores = set()
    for cpu in cpus:
        topology = sys_cpu / f'cpu{cpu}' / 'topology'
        try:
            cores.add(((topology / 'physical_package_id').read_text().strip(),
                       (topology / 'core_id').read_text().strip()))
        except OSError:
            cores.add(('unknown', str(cpu)))
    physical = max(1, len(cores))
    budget = max(1, physical - 2)
    if cgroup is None:
        try:
            relative = next(line[3:] for line in Path('/proc/self/cgroup').read_text().splitlines()
                            if line.startswith('0::'))
            cgroup = Path('/sys/fs/cgroup') / relative.lstrip('/')
        except (OSError, StopIteration):
            cgroup = None
    quota = None
    if cgroup is not None:
        for parent in [cgroup, *cgroup.parents]:
            try:
                limit, period = (parent / 'cpu.max').read_text().split()
                if limit != 'max':
                    value = int(limit) / int(period)
                    quota = value if quota is None else min(quota, value)
            except (OSError, ValueError, ZeroDivisionError):
                continue
    if quota is not None:
        budget = min(budget, max(1, math.floor(quota)))
    return dict(physical_cores=physical, logical_cpus=len(cpus),
                quota_cpus=quota, default_cpu_budget=budget)


def positive_setting(name, default):
    value = int(os.environ.get(name, str(default)))
    if value < 1:
        raise ValueError(f'{name} must be a positive integer')
    return value


class _CpuLease(list):
    def __init__(self, handles, gate):
        super().__init__(handles)
        self.gate = gate


class HostCpuPool:
    """Nonblocking, partially grantable CPU leases across checker processes."""
    def __init__(self, directory, units):
        self.directory, self.units = Path(directory), units
        self.directory.mkdir(parents=True, exist_ok=True)
        # A mismatched launch setting must not create a second, overlapping
        # host budget. Changing it requires draining the old services first.
        with (self.directory / 'configuration.lock').open('a+') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            path = self.directory / 'capacity.json'
            if path.exists() and json.loads(path.read_text()) != {'cpu_units': units}:
                raise ValueError('checker services disagree on host CPU pool capacity')
            if not path.exists():
                path.write_text(json.dumps({'cpu_units': units}) + '\n')

    def acquire(self, requested, *, exclusive=False):
        gate = (self.directory / 'exclusive.lock').open('a+')
        try:
            fcntl.flock(gate, (fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH) | fcntl.LOCK_NB)
        except BlockingIOError:
            gate.close()
            return []
        handles = []
        try:
            for index in range(self.units):
                handle = (self.directory / f'cpu-{index}.lock').open('a+')
                try:
                    fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    handle.close()
                    continue
                handles.append(handle)
                if len(handles) >= requested:
                    break
            if handles:
                return _CpuLease(handles, gate)
            gate.close()
            return []
        except BaseException:
            self.release(handles)
            gate.close()
            raise

    @staticmethod
    def release(handles):
        for handle in handles:
            handle.close()
        if hasattr(handles, 'gate'):
            handles.gate.close()

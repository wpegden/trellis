"""Host-only admission for new private preparations; no native state changes.

The allowance is an initial-allocation estimate plus free-space headroom. It
does not bound later provider output, compiler trees, or copy-on-write growth.
Existing attempts, acceptance, publication and cleanup keep their own guards.
"""
from pathlib import Path
from contextlib import contextmanager
import os
import threading
import time

from .candidate import canonical_json
from .publication import atomic

GIB = 1024 ** 3


class PreparationDeferred(Exception):
    """No preparation was authorized; retain saved inputs for a later tick."""


class PreparationCapacity:
    def __init__(self, runtime, repo, *, floor_bytes=12 * GIB,
                 workspace_bytes=3 * GIB, floor_inodes=100_000,
                 workspace_inodes=10_000, retry_seconds=30,
                 clock=time.monotonic, statvfs=os.statvfs):
        self.runtime, self.repo = Path(runtime), Path(repo)
        self.floor_bytes, self.workspace_bytes = floor_bytes, workspace_bytes
        self.floor_inodes, self.workspace_inodes = floor_inodes, workspace_inodes
        self.retry_seconds, self.clock, self.statvfs = retry_seconds, clock, statvfs
        self.retry_at = 0.0
        self.lock = threading.Lock()
        self.last_wait = None
        self.local = threading.local()

    @contextmanager
    def admitted(self, workspaces):
        """Keep one preflight decision through an atomic preparation batch.

        Rechecking the full estimate after writing its first objects would
        charge those same allocations twice and could strand a partial batch.
        External growth can still exceed the estimate; I/O errors stay visible.
        """
        self.require(workspaces)
        previous = getattr(self.local, 'workspaces', 0)
        self.local.workspaces = max(previous, workspaces)
        try:
            yield
        finally:
            self.local.workspaces = previous

    def ready(self):
        with self.lock:
            return self.clock() >= self.retry_at

    def require(self, workspaces=1):
        if type(workspaces) is not int or workspaces < 0:
            raise ValueError('preparation workspace count must be a nonnegative integer')
        if workspaces == 0:
            return
        if workspaces <= getattr(self.local, 'workspaces', 0):
            return
        self.require_allocation(workspaces * self.workspace_bytes,
                                workspaces * self.workspace_inodes, workspaces=workspaces)

    def require_allocation(self, allocation_bytes, allocation_inodes, *, workspaces=None, check_floor=False):
        """Admit a measured initial allocation, retaining the same floors/backoff.

        This check deliberately does not borrow a workspace-count lease: the
        caller has just priced a different allocation after pure preparation.
        """
        if any(type(value) is not int or value < 0
               for value in (allocation_bytes, allocation_inodes)):
            raise ValueError('preparation allocation must use nonnegative integers')
        if allocation_bytes == allocation_inodes == 0 and not check_floor:
            return
        with self.lock:
            if self.clock() < self.retry_at:
                raise PreparationDeferred('storage preparation retry is pending')
            filesystems = {}
            for path in (self.runtime, self.repo):
                device = path.stat().st_dev
                if device in filesystems:
                    continue
                status = self.statvfs(path)
                # Private workspaces and compact batch objects live below
                # runtime; the canonical filesystem still needs checkpoint room.
                private = device == self.runtime.stat().st_dev
                filesystems[device] = dict(path=str(path),
                    free_bytes=status.f_bavail * status.f_frsize,
                    required_bytes=self.floor_bytes + (allocation_bytes if private else 0),
                    free_inodes=status.f_favail,
                    required_inodes=self.floor_inodes + (allocation_inodes if private else 0))
            blocked = [row for row in filesystems.values()
                       if row['free_bytes'] < row['required_bytes']
                       or row['free_inodes'] < row['required_inodes']]
            if blocked:
                self.retry_at = self.clock() + self.retry_seconds
                wait = dict(waiting=True, workspaces=workspaces, allocation_bytes=allocation_bytes,
                    allocation_inodes=allocation_inodes, filesystems=list(filesystems.values()),
                    retry_seconds=self.retry_seconds,
                    note='Initial preparation estimate; active growth is not bounded. Native work and receipts are unchanged.')
                # Keep one bounded host diagnostic, outside protocol/receipts.
                # Real write/I/O failure remains an infrastructure error.
                if wait != self.last_wait:
                    atomic(self.runtime / 'dispatch/preparation-capacity.json', canonical_json(wait))
                    self.last_wait = wait
                raise PreparationDeferred('insufficient storage for new private preparation')
            self.retry_at = 0.0
            if self.last_wait is not None:
                atomic(self.runtime / 'dispatch/preparation-capacity.json', canonical_json(
                    dict(waiting=False, filesystems=list(filesystems.values()))))
                self.last_wait = None

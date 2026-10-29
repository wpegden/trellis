"""Reserve semantic inputs from host-check preparation through publication.

Worker execution is independent of this queue. Read-only overlap is harmless;
a writer waits for readers and writers of the same semantic input. Unknown
footprints are exclusive. Admission is fair among conflicting requests while
allowing unrelated checks to use idle compiler capacity.
"""
from __future__ import annotations

import asyncio
from contextlib import asynccontextmanager
from dataclasses import dataclass


@dataclass(frozen=True)
class Footprint:
    reads: frozenset[str] = frozenset()
    writes: frozenset[str] = frozenset()
    exclusive: bool = False

    def conflicts(self, other):
        return (self.exclusive or other.exclusive
                or bool(self.writes & (other.reads | other.writes))
                or bool(other.writes & self.reads))


class AcceptanceQueue:
    def __init__(self):
        self.condition = asyncio.Condition()
        self.pending = []
        self.active = {}

    def restore(self, reservations):
        """Restore surviving checks before admitting any new preparation.

        Legacy checks may already overlap. They retain their original evidence
        and finish under publication guards; new work waits for their reads.
        """
        if self.active or self.pending:
            raise RuntimeError('host reservations must be restored before admission')
        self.active.update(reservations)

    @asynccontextmanager
    async def reserve(self, identity, footprint, *, adopt=False):
        ticket = (identity, footprint)
        async with self.condition:
            if adopt and identity in self.active:
                if self.active[identity] != footprint:
                    raise RuntimeError('surviving host acceptance changed its reservation')
            elif identity in self.active or any(item[0] == identity for item in self.pending):
                raise RuntimeError('host acceptance already has an input reservation')
            else:
                self.pending.append(ticket)
                try:
                    await self.condition.wait_for(lambda: self._eligible(ticket))
                except BaseException:
                    self.pending.remove(ticket)
                    self.condition.notify_all()
                    raise
                self.pending.remove(ticket)
                self.active[identity] = footprint
        try:
            yield
        finally:
            async with self.condition:
                del self.active[identity]
                self.condition.notify_all()

    def _eligible(self, ticket):
        _, footprint = ticket
        if any(footprint.conflicts(other) for other in self.active.values()):
            return False
        # Do not bypass an earlier conflicting waiter, including an exclusive
        # structural check. Disjoint work is free to pass it.
        for previous in self.pending:
            if previous is ticket:
                return True
            if footprint.conflicts(previous[1]):
                return False
        return False

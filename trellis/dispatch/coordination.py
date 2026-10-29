"""Event-driven orchestration over the kernel dispatch ledger.

The kernel owns legality, reservations, retries and mathematical acceptance.
This loop owns process lifetimes and runs independent host checks concurrently.
The driver serializes canonical publication and uses
isolated task runtimes; it must never run a worker against the canonical repo.
All driver operations are restart-idempotent, keyed by task/review identity.
"""
from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Protocol


@dataclass(frozen=True)
class Completion:
    task_id: int
    candidate_sha256: str | None = None
    failure: dict | None = None


class Driver(Protocol):
    async def snapshot(self) -> dict: ...
    async def prepare_one_retry(self) -> bool: ...
    def set_handoff_notifier(self, notify) -> None: ...
    async def launch_or_adopt(self, task: dict) -> Completion: ...
    async def review(self, cursor: dict, *, prepared: bool = False) -> None: ...
    async def start_review(self) -> dict | None: ...
    async def observe(self, event: dict) -> None: ...
    async def complete_delivery(self, task: dict, completion: Completion) -> None: ...
    async def integrate(self, task_id: int, candidate_sha256: str) -> None: ...
    async def stop_and_reap(self, task: dict) -> None: ...
    async def run_verifiers(self) -> dict | None: ...
    async def recover_publication(self) -> None: ...
    def now_ms(self) -> int: ...


class Coordinator:
    def __init__(self, driver: Driver):
        self.driver = driver
        self.changed = asyncio.Event()
        self.runners: dict[int, asyncio.Task] = {}
        self.integrations: dict[int, asyncio.Task] = {}
        self.reaping: dict[int, asyncio.Task] = {}
        self.reviewer: asyncio.Task | None = None
        self.verifiers: asyncio.Task | None = None
        self.fatal: BaseException | None = None
        self.closed = False
        self.handoffs = {}
        self._handoff_enabled = hasattr(driver, 'set_handoff_notifier')
        if hasattr(driver, 'set_notifier'):
            driver.set_notifier(self.notify)
        if self._handoff_enabled:
            driver.set_handoff_notifier(self._handoff)

    def notify(self):
        self.changed.set()

    def _handoff(self, task_id, kind):
        pending = self.handoffs.get((task_id, kind))
        if pending is not None and pending[1] is not None:
            pending[1].set()
            self.notify()

    def _start(self, awaitable, *, handoff=None):
        task = asyncio.create_task(awaitable)
        if handoff is not None and self._handoff_enabled:
            self.handoffs[handoff] = (task, None if handoff[1] == 'stopping' else asyncio.Event())
        def finished(task):
            # Infrastructure faults stop dispatch; they are never converted to
            # mathematical failure and never silently retried in a tight loop.
            if not task.cancelled() and task.exception() is not None:
                self.fatal = task.exception()
            self.notify()
        task.add_done_callback(finished)
        return task

    def _handoffs_ready(self):
        # A slow launcher must not stop timeout, cleanup or operator-control
        # polling. Only further preparation waits for its actual handoff.
        for key, (task, event) in list(self.handoffs.items()):
            if task.done():
                if not task.cancelled():
                    task.result()
                del self.handoffs[key]
            elif event is not None and event.is_set():
                del self.handoffs[key]
        return not self.handoffs

    async def _execute(self, task):
        from .preparation_capacity import PreparationDeferred
        try:
            completion = await self.driver.launch_or_adopt(task)
        except PreparationDeferred:
            return 'preparation_deferred'  # Keep the native reservation exact.
        if completion.task_id != task['id']:
            raise RuntimeError('worker completion belongs to a different task')
        await self.driver.complete_delivery(task, completion)

    async def _integrate(self, task):
        # The driver guards this exact submission under the canonical lock.
        # Scheduling sweeps belong to tick, not delivery or acceptance guards.
        await self.driver.integrate(task['id'], task['status']['candidate_sha256'])
        # Each check uses its own workspace and managed compiler admission.
        # The driver rechecks canonical inputs under the publication lock.
        # Sealed candidates do not expire; slots remain held through cleanup.

    async def _expire(self, task):
        await self.driver.observe(dict(kind='attempt_stopping', task_id=task['id'],
                                       outcome={'kind': 'timed_out'}))

    async def _reap(self, task):
        # Stop waiters before deliberately terminating their launchers. The
        # driver drains any in-flight thread operation on cancellation, so a
        # late launch or publication cannot escape the subsequent cleanup.
        pending = [jobs[task['id']] for jobs in (self.runners, self.integrations)
                   if task['id'] in jobs]
        for job in pending:
            if not job.done():
                job.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
        # Cancelling a callback is not evidence that its compiler or descendants
        # exited. Keep the reservation until both channels and processes drain.
        await self.driver.stop_and_reap(task)
        await self.driver.observe(dict(kind='processes_reaped', task_id=task['id'], now_ms=self.driver.now_ms()))

    async def tick(self):
        if self.fatal is not None:
            raise self.fatal
        snapshot = await self.driver.snapshot()
        drained = True
        for waiter in [*self.runners.values(), *self.integrations.values(),
                       *self.reaping.values(), self.reviewer, self.verifiers]:
            if waiter is not None:
                if not waiter.done() or waiter.cancelled():
                    drained = False
                else:
                    waiter.result()  # A pending done callback must not hide a fault.
        parked = (self.verifiers.result() or {}) if (
            self.verifiers is not None and self.verifiers.done() and not self.verifiers.cancelled()) else {}
        verifiers_drained = all(j['status']['status'] in ('queued', 'resolved') or
            snapshot.get('pausing') and j['status']['status'] == 'checking'
            and parked.get(j['id']) == j['status']
            for j in snapshot.get('verifiers', {}).get('jobs', {}).values())
        if (snapshot.get('stage') == 'Complete' or snapshot.get('pv_revision_ready') or snapshot.get('pausing')) and (
            drained and all(t['status']['status'] == 'resolved' for t in snapshot['tasks'].values())
            and (snapshot.get('reviewer') is None or snapshot.get('pausing') and
                 (self.reviewer is None or self.reviewer.done()))
            and verifiers_drained
        ):
            self.closed = True
            return None
        now = self.driver.now_ms()
        preparation_ready = getattr(self.driver, 'preparation_ready', lambda: True)()
        for task in snapshot['tasks'].values():
            identity = task['id']
            status = task['status']['status']
            if status == 'resolved':
                for jobs in (self.runners, self.integrations, self.reaping):
                    if identity in jobs and jobs[identity].done():
                        del jobs[identity]
                continue
            deadline = task.get('deadline_ms')
            if status == 'running' and task.get('worker_attempt') is None and deadline is not None and now >= deadline:
                await self._expire(task)
                # CandidateReady may have won under the native lock. Reap only
                # after a fresh snapshot confirms an actual stopping outcome.
                continue
            runner = self.runners.get(identity)
            deferred = (runner is not None and runner.done() and not runner.cancelled()
                        and runner.exception() is None and runner.result() == 'preparation_deferred')
            if status == 'reserved' and (task.get('worker_attempt') is not None or deferred):
                previous = [jobs[identity] for jobs in (self.runners, self.integrations) if identity in jobs]
                if any(not job.done() for job in previous):
                    continue
                self.runners.pop(identity, None)
                self.integrations.pop(identity, None)
            ready = getattr(self.driver, 'worker_preparation_ready', lambda _: preparation_ready)(task)
            if status == 'reserved' and (not ready or not task.get('execution_runnable', True)):
                continue
            if status in ('reserved', 'running', 'submitted') and identity not in self.runners:
                self.runners[identity] = self._start(self._execute(task), handoff=(identity, 'worker'))
            elif status == 'checking' and identity not in self.integrations:
                self.integrations[identity] = self._start(self._integrate(task), handoff=(identity, 'checking'))
            elif status == 'stopping' and identity not in self.reaping:
                self.reaping[identity] = self._start(self._reap(task), handoff=(identity, 'stopping'))
        handed_off = self._handoffs_ready()
        if handed_off and not snapshot.get('pausing') and hasattr(self.driver, 'prepare_one_retry'):
            # A task object or scheduled coroutine is not a provider permit.
            # Give durable work its real handoff before the next heavy native
            # preparation can occupy the lock needed by those launches.
            if await self.driver.prepare_one_retry():
                self.notify()
                return 0  # Expose this reservation before preparing another.
        preparation_ready = getattr(self.driver, 'preparation_ready', lambda: True)()
        if not snapshot.get('pausing') and (self.reviewer is None or self.reviewer.done()):
            cursor = snapshot.get('reviewer')
            prepared = False
            if preparation_ready and handed_off and cursor is None and snapshot.get('reviewer_runnable'):
                cursor = await self.driver.start_review()
                prepared = True
            ready = getattr(self.driver, 'review_preparation_ready', lambda _: preparation_ready)(cursor)
            if cursor is not None and ready:
                self.reviewer = self._start(self.driver.review(cursor, prepared=prepared))
        # The driver drains only currently runnable, exact-identity verifier
        # jobs and returns when none remain; it owns the separate lane limit.
        active_verifiers = any(j['status']['status'] in ('running', 'checking')
                              for j in snapshot.get('verifiers', {}).get('jobs', {}).values())
        verifier_ready = getattr(self.driver, 'verifier_preparation_ready',
                                 lambda _: preparation_ready or active_verifiers)(snapshot)
        if (snapshot.get('verifiers_runnable') and verifier_ready
                and not (snapshot.get('pausing') and verifiers_drained)
                and (self.verifiers is None or self.verifiers.done())):
            self.verifiers = self._start(self.driver.run_verifiers())
        deadlines = [t['deadline_ms'] for t in snapshot['tasks'].values()
                     if t.get('worker_attempt') is None and t.get('deadline_ms') is not None and t['status']['status'] == 'running']
        return min(((d - now) / 1000 for d in deadlines), default=None)

    async def run(self):
        # Must finish a recorded publication before any new snapshot is exposed
        # to a reviewer or worker, even when all process records look terminal.
        await self.driver.recover_publication()
        while not self.closed:
            self.changed.clear()
            delay = await self.tick()
            if self.closed:
                break
            try:
                # External human decisions and operational controls update the
                # native journal without completing one of our coroutines.
                # Poll that boundary lightly; unchanged state launches nothing.
                await asyncio.wait_for(self.changed.wait(), timeout=30 if delay is None else min(30, max(.001, delay)))
            except asyncio.TimeoutError:
                pass

    async def shutdown(self):
        # Operational shutdown parks/adopts durable attempts. It is not a
        # cancellation, a mathematical outcome, or permission to free slots.
        self.closed = True
        self.notify()
        cleanup = getattr(self.driver, '_build_cleanup', None)
        if cleanup is not None:
            cleanup.close()
        pending = [*self.runners.values(), *self.integrations.values(), *self.reaping.values(),
                   self.reviewer, self.verifiers]
        pending = [t for t in pending if t is not None and not t.done()]
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)

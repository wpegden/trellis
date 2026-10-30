import asyncio
import copy

import pytest

from trellis.dispatch.coordination import Completion, Coordinator


class Driver:
    def __init__(self):
        self.time = 1000
        self.tasks = {str(i): dict(id=i, status={'status': 'running'}, deadline_ms=2000) for i in (1, 2)}
        self.done = {i: asyncio.Future() for i in (1, 2)}
        self.cleanup = {i: asyncio.Event() for i in (1, 2)}
        self.review_count = 0
        self.cursor = None
        self.seq = 0
        self.reviewed = 0
        self.published = []
        self.active_checks = 0
        self.max_checks = 0
        self.check_done = asyncio.Event()

    def now_ms(self): return self.time
    async def snapshot(self):
        return dict(tasks=copy.deepcopy(self.tasks), reviewer=self.cursor,
                    reviewer_runnable=self.seq > self.reviewed and self.cursor is None,
                    verifiers_runnable=False)
    async def launch_or_adopt(self, task): return await self.done[task['id']]
    async def complete_delivery(self, launched, completion):
        task = self.tasks[str(launched['id'])]
        if task['status']['status'] not in ('reserved', 'running', 'submitted'):
            return
        failure = completion.failure
        if failure is None and task.get('worker_attempt') is None and self.time >= task['deadline_ms']:
            failure = {'kind': 'timed_out'}
        if failure is not None:
            await self.observe(dict(kind='attempt_stopping', task_id=task['id'], outcome=failure))
        else:
            await self.observe(dict(kind='candidate_ready', task_id=task['id'],
                                    candidate_sha256=completion.candidate_sha256))
    async def observe(self, e):
        t = self.tasks[str(e['task_id'])]
        if e['kind'] == 'candidate_ready':
            assert t['status']['status'] == 'running'
            t['status'] = dict(status='checking', candidate_sha256=e['candidate_sha256'])
        elif e['kind'] == 'attempt_stopping':
            t['status'] = dict(status='stopping', outcome=e['outcome'])
        elif e['kind'] == 'processes_reaped':
            assert self.cleanup[t['id']].is_set()
            t['status']['status'] = 'resolved'
            self.seq += 1
    async def integrate(self, task_id, candidate):
        self.active_checks += 1
        self.max_checks = max(self.max_checks, self.active_checks)
        try:
            await self.check_done.wait()
            self.published.append(task_id)
            self.tasks[str(task_id)]['status'] = dict(status='stopping', outcome={'kind':'accepted'})
        finally:
            self.active_checks -= 1
    async def stop_and_reap(self, task): await self.cleanup[task['id']].wait()
    async def start_review(self):
        self.cursor = dict(request_id=self.review_count + 1, observed_event=self.seq)
        return self.cursor
    async def review(self, cursor, *, prepared=False):
        self.review_count += 1
        self.reviewed = cursor['observed_event']
        self.cursor = None
    async def run_verifiers(self): pass
    async def recover_publication(self): pass


async def tick(c):
    await c.tick()
    for _ in range(3): await asyncio.sleep(0)


@pytest.mark.parametrize('case', ['exact', 'changed_hash', 'running', 'not_paused', 'cancelled'])
def test_pause_requires_current_exact_drained_verifier_result(case):
    async def scenario():
        d = Driver(); d.tasks = {}
        sealed = dict(status='checking', result_sha256='a' * 64)
        current = copy.deepcopy(sealed)
        if case == 'changed_hash':
            current['result_sha256'] = 'b' * 64
        if case == 'running':
            current = {'status': 'running'}
        async def snapshot():
            return dict(stage='Complete', pausing=case != 'not_paused', tasks={},
                reviewer=None, verifiers={'jobs': {'91': dict(id=91, status=current)}})
        d.snapshot = snapshot
        c = Coordinator(d)
        async def drained():
            return {91: sealed}
        c.verifiers = c._start(drained())
        if case == 'cancelled':
            c.verifiers.cancel()
            await asyncio.gather(c.verifiers, return_exceptions=True)
        else:
            await c.verifiers
        await c.tick()
        assert c.closed == (case == 'exact')
        assert current['status'] == ('running' if case == 'running' else 'checking')
        await c.shutdown()
    asyncio.run(scenario())


@pytest.mark.parametrize('cleanup_fails', [False, True])
def test_pause_waits_for_local_verifier_cleanup_even_after_native_resolution(cleanup_fails):
    async def scenario():
        d = Driver(); d.tasks = {}
        cleanup = asyncio.Event()
        async def snapshot():
            return dict(pausing=True, tasks={}, reviewer=None,
                verifiers={'jobs': {'91': dict(id=91, status={'status': 'resolved'})}})
        d.snapshot = snapshot
        c = Coordinator(d)
        async def finish():
            await cleanup.wait()
            if cleanup_fails:
                raise RuntimeError('cleanup acknowledgment failed')
            return {}
        c.verifiers = c._start(finish())
        await c.tick()
        assert not c.closed
        cleanup.set()
        if cleanup_fails:
            with pytest.raises(RuntimeError, match='cleanup acknowledgment'):
                await c.verifiers
            with pytest.raises(RuntimeError, match='cleanup acknowledgment'):
                await c.tick()
            assert not c.closed
        else:
            await c.verifiers
            await c.tick()
            assert c.closed
        await c.shutdown()
    asyncio.run(scenario())


def test_independent_execution_concurrent_acceptance_and_slots_held_until_reaped():
    async def scenario():
        d = Driver(); c = Coordinator(d)
        await tick(c)
        assert len(c.runners) == 2
        d.done[2].set_result(Completion(2, 'b' * 64))
        d.done[1].set_result(Completion(1, 'a' * 64))
        await asyncio.sleep(0)
        await tick(c)
        assert all(t['status']['status'] == 'checking' for t in d.tasks.values())
        assert d.review_count == 0 and d.active_checks == 2
        d.check_done.set()
        await tick(c)
        assert sorted(d.published) == [1, 2] and d.max_checks == 2
        await tick(c)
        assert d.review_count == 0, 'agent/check completion does not release a slot'
        d.cleanup[1].set(); d.cleanup[2].set()
        await tick(c); await tick(c)
        assert all(t['status']['status'] == 'resolved' for t in d.tasks.values())
        assert d.review_count == 1, 'simultaneous completions coalesce into one review'
        await tick(c)
        assert d.review_count == 1, 'unchanged state must not spin the reviewer'
    asyncio.run(scenario())


def test_submitted_candidate_survives_worker_deadline_while_running_peer_expires():
    async def scenario():
        d = Driver(); c = Coordinator(d)
        await tick(c)
        d.done[1].set_result(Completion(1, 'a' * 64))
        await asyncio.sleep(0); await tick(c)
        d.time = 2000
        await tick(c)
        assert d.tasks['1']['status']['status'] == 'checking'
        assert d.tasks['2']['status']['status'] == 'stopping'
        assert d.published == [] and d.review_count == 0
        d.check_done.set()
        await tick(c); await tick(c)
        assert d.published == [1]
        d.cleanup[1].set(); d.cleanup[2].set()
        await tick(c); await tick(c)
        assert all(t['status']['status'] == 'resolved' for t in d.tasks.values())
        assert d.tasks['1']['status']['outcome']['kind'] == 'accepted'
        assert d.tasks['2']['status']['outcome']['kind'] == 'timed_out'
    asyncio.run(scenario())


def test_fast_acceptance_releases_its_slot_while_another_check_continues():
    async def scenario():
        d = Driver()
        release = {i: asyncio.Event() for i in (1, 2)}
        async def integrate(task_id, candidate):
            await release[task_id].wait()
            d.published.append(task_id)
            d.tasks[str(task_id)]['status'] = dict(status='stopping', outcome={'kind': 'accepted'})
        d.integrate = integrate
        c = Coordinator(d)
        try:
            await tick(c)
            for id in (1, 2):
                d.done[id].set_result(Completion(id, str(id) * 64))
            await asyncio.sleep(0); await tick(c)
            release[2].set()
            await tick(c); await tick(c)
            assert d.published == [2] and d.review_count == 0
            d.cleanup[2].set()
            await tick(c); await tick(c)
            assert d.tasks['2']['status']['status'] == 'resolved'
            assert d.review_count == 1
            assert d.tasks['1']['status']['status'] == 'checking'
            assert not c.integrations[1].done()
        finally:
            await c.shutdown()
    asyncio.run(scenario())


def test_concurrent_acceptances_survive_deadline_and_coordinator_restart():
    async def scenario():
        d = Driver(); c = Coordinator(d)
        await tick(c)
        for id in (1, 2):
            d.done[id].set_result(Completion(id, str(id) * 64))
        await asyncio.sleep(0); await tick(c)
        assert d.active_checks == 2
        assert all(t['status']['status'] == 'checking' for t in d.tasks.values())
        d.time = 6000  # Both old snapshots still carry the expired worker deadline.
        await tick(c)
        assert not c.reaping and not d.published
        await c.shutdown()
        resumed = Coordinator(d)
        try:
            await tick(resumed)
            assert d.active_checks == 2 and d.review_count == 0
            assert not resumed.reaping
            d.check_done.set()
            await tick(resumed); await tick(resumed)
            assert sorted(d.published) == [1, 2]
            assert all(t['status']['status'] == 'stopping' for t in d.tasks.values())
            assert d.review_count == 0, 'checking completion still waits for cleanup'
            d.cleanup[1].set(); d.cleanup[2].set()
            await tick(resumed); await tick(resumed)
            assert all(t['status']['status'] == 'resolved' for t in d.tasks.values())
        finally:
            await resumed.shutdown()
    asyncio.run(scenario())


def test_candidate_submission_wins_race_with_stale_timeout_snapshot(tmp_path):
    from trellis.dispatch.native import NativeLedger
    async def scenario():
        d = Driver(); d.tasks.pop('2'); d.time = 2000
        events = []
        def kernel(payload):
            event = payload['event']
            if event is not None:
                events.append(event)
                raise AssertionError('stale timeout must not reach native event application')
            # CandidateReady committed after the coordinator read Running.
            d.tasks['1']['status'] = dict(status='checking', candidate_sha256='a' * 64)
            return dict(status='dispatch_ok', output=dict(tasks=copy.deepcopy(d.tasks)))
        native = NativeLedger(tmp_path, kernel=kernel)
        async def observe(event):
            assert event['kind'] == 'attempt_stopping'
            return native.observe(event)
        d.observe = observe
        c = Coordinator(d)
        try:
            await tick(c)
            assert d.tasks['1']['status']['status'] == 'checking'
            assert not c.reaping and not events
            await tick(c)
            assert d.active_checks == 1 and not c.reaping and c.fatal is None
        finally:
            await c.shutdown()
    asyncio.run(scenario())


@pytest.mark.parametrize('status', ['running', 'checking'])
@pytest.mark.parametrize('cleanup_fails', [False, True])
def test_stopping_cancels_waiter_before_reaping_without_hiding_cleanup_failure(status, cleanup_fails):
    async def scenario():
        d = Driver()
        d.tasks['2']['deadline_ms'] = 9000
        d.tasks['1']['status'] = dict(status=status, candidate_sha256='a' * 64)
        started, launcher_exited, cancelled = asyncio.Event(), asyncio.Event(), asyncio.Event()
        cleanup_started = asyncio.Event()
        async def wait():
            started.set()
            try:
                await launcher_exited.wait()
                raise RuntimeError('attempt launcher exited without a result')
            except asyncio.CancelledError:
                cancelled.set()
                raise
        async def launch(task):
            if task['id'] == 1:
                return await wait()
            return await d.done[2]
        d.launch_or_adopt = launch
        d.integrate = lambda *_: wait()
        async def reap(task):
            assert task['id'] == 1
            cleanup_started.set()
            launcher_exited.set()
            # Force the old waiter/callback to run before process cleanup ends.
            await asyncio.sleep(0)
            await asyncio.sleep(0)
            await d.cleanup[1].wait()
            if cleanup_fails:
                raise RuntimeError('descendant cleanup was not acknowledged')
        d.stop_and_reap = reap
        c = Coordinator(d)
        try:
            await tick(c)
            await started.wait()
            d.time = 2000
            outcome = {'kind': 'timed_out'} if status == 'running' else {'kind': 'cancelled', 'reason': 'operator'}
            if status == 'checking':
                await d.observe(dict(kind='attempt_stopping', task_id=1, outcome=outcome))
            await tick(c)
            await tick(c)
            await cleanup_started.wait()
            assert cancelled.is_set()
            assert c.fatal is None
            assert d.tasks['1']['status']['status'] == 'stopping'
            assert d.tasks['2']['status']['status'] == 'running'
            assert not c.runners[2].done()
            assert not d.published and d.review_count == 0
            d.cleanup[1].set()
            for _ in range(4): await asyncio.sleep(0)
            if cleanup_fails:
                with pytest.raises(RuntimeError, match='cleanup was not acknowledged'):
                    await c.tick()
                assert d.tasks['1']['status']['status'] == 'stopping'
            else:
                await tick(c)
                assert d.tasks['1']['status'] == dict(status='resolved', outcome=outcome)
                assert c.fatal is None and not d.published
        finally:
            await c.shutdown()
    asyncio.run(scenario())


@pytest.mark.parametrize('status', ['running', 'checking'])
def test_unexpected_launcher_exit_still_stops_coordinator(status):
    async def scenario():
        d = Driver()
        d.tasks['1']['status'] = dict(status=status, candidate_sha256='a' * 64)
        async def failed(*_):
            raise RuntimeError('attempt launcher exited without a result')
        if status == 'running': d.launch_or_adopt = failed
        else: d.integrate = failed
        c = Coordinator(d)
        try:
            await tick(c)
            with pytest.raises(RuntimeError, match='launcher exited without a result'):
                await c.tick()
            assert not d.published
            assert d.tasks['1']['status']['status'] == status
        finally:
            await c.shutdown()
    asyncio.run(scenario())


def test_one_worker_failure_does_not_cancel_the_other():
    async def scenario():
        d = Driver(); c = Coordinator(d)
        await tick(c)
        d.done[1].set_result(Completion(1, failure={'kind':'launch_failed', 'reason':'provider unavailable'}))
        d.done[2].set_result(Completion(2, 'b' * 64))
        d.check_done.set(); d.cleanup[1].set(); d.cleanup[2].set()
        for _ in range(5): await tick(c)
        assert d.published == [2]
        assert d.tasks['1']['status']['outcome']['kind'] == 'launch_failed'
        assert d.tasks['2']['status']['outcome']['kind'] == 'accepted'
    asyncio.run(scenario())


def test_only_freshly_prepared_reviewer_skips_adoption_preparation():
    async def run(adopt):
        d = Driver(); d.tasks = {}; d.seq = 1
        cursor = dict(request_id=7, observed_event=0)
        d.cursor = cursor if adopt else None
        calls = []
        async def start():
            calls.append('prepare')
            return cursor
        async def review(actual, *, prepared=False):
            calls.append(('review', actual, prepared))
        d.start_review, d.review = start, review
        c = Coordinator(d)
        await tick(c)
        assert calls == ([('review', cursor, False)] if adopt else
                         ['prepare', ('review', cursor, True)])
        await c.shutdown()
    asyncio.run(run(False))
    asyncio.run(run(True))


def test_completed_pv_generation_stops_without_starting_another_cycle():
    async def scenario():
        d = Driver(); d.tasks = {}
        async def snapshot():
            return dict(stage='Start', pv_revision_ready=True, tasks={}, reviewer=None,
                        verifiers={'jobs': {}}, verifiers_runnable=True)
        async def unexpected():
            raise AssertionError('a frozen generation must not start another exclusive step')
        d.snapshot, d.run_verifiers = snapshot, unexpected
        c = Coordinator(d)
        await tick(c)
        assert c.closed and c.verifiers is None and d.review_count == 0
    asyncio.run(scenario())


def test_delivery_and_integration_do_not_run_scheduling_sweeps():
    async def scenario():
        d = Driver()
        async def unexpected():
            pytest.fail('task guards must not route, prepare or prune other work')
        d.snapshot = unexpected
        c = Coordinator(d)
        d.done[1].set_result(Completion(1, 'a' * 64))
        await c._execute(copy.deepcopy(d.tasks['1']))
        assert d.tasks['1']['status'] == dict(status='checking', candidate_sha256='a' * 64)
        d.check_done.set()
        await c._integrate(copy.deepcopy(d.tasks['1']))
        assert d.published == [1]
    asyncio.run(scenario())


class HandoffDriver(Driver):
    def __init__(self):
        super().__init__()
        self.preparations = 0
        self.pausing = False

    def set_handoff_notifier(self, notify):
        self.handoff = notify

    async def prepare_one_retry(self):
        self.preparations += 1
        return False

    async def snapshot(self):
        return dict(await super().snapshot(), pausing=self.pausing)


def test_each_retry_gets_actual_launch_handoff_before_next_preparation():
    async def scenario():
        d = HandoffDriver(); d.tasks = {}
        allow_permit, permitted, second_started, finish_second = (asyncio.Event() for _ in range(4))
        events = []
        async def prepare():
            d.preparations += 1
            if d.preparations == 1:
                d.tasks['1'] = dict(id=1, status={'status': 'reserved'}, deadline_ms=None)
                events.append('first durable preparation')
                return True
            assert permitted.is_set(), 'scheduled launcher is not a permit'
            events.append('second preparation')
            second_started.set()
            await finish_second.wait()
            return False
        async def launch(task):
            events.append('launcher')
            await allow_permit.wait()
            events.append('permit')
            permitted.set()
            d.handoff(task['id'], 'worker')
            # The provider stays live while the next preparation runs.
            return await d.done[task['id']]
        d.prepare_one_retry, d.launch_or_adopt = prepare, launch
        c = Coordinator(d)
        preparing = None
        try:
            assert await c.tick() == 0
            await tick(c)
            await tick(c)
            assert d.preparations == 1 and events == ['first durable preparation', 'launcher']
            allow_permit.set()
            await asyncio.wait_for(permitted.wait(), 1)
            assert c.changed.is_set(), 'actual permit must wake preparation'
            preparing = asyncio.create_task(c.tick())
            await asyncio.wait_for(second_started.wait(), 1)
            assert not c.runners[1].done() and not preparing.done()
            finish_second.set()
            await preparing
            assert events == ['first durable preparation', 'launcher', 'permit', 'second preparation']
        finally:
            finish_second.set()
            if preparing is not None:
                await asyncio.gather(preparing, return_exceptions=True)
            await c.shutdown()
    asyncio.run(scenario())


def test_restart_checking_registration_and_stopping_reap_precede_retry_preparation():
    async def scenario():
        d = HandoffDriver()
        d.tasks['1']['status'] = dict(status='checking', candidate_sha256='a' * 64)
        d.tasks['2']['status'] = dict(status='stopping', outcome={'kind': 'accepted'})
        allow_registration, registered = asyncio.Event(), asyncio.Event()
        async def integrate(task_id, candidate):
            await allow_registration.wait()
            d.handoff(task_id, 'checking')
            registered.set()  # Same coroutine enters the queue before yielding.
            await d.check_done.wait()
        d.integrate = integrate
        c = Coordinator(d)
        try:
            await tick(c)
            await tick(c)
            assert d.preparations == 0
            allow_registration.set()
            await registered.wait()
            await tick(c)
            assert d.preparations == 0, 'Stopping still holds its cleanup barrier'
            d.cleanup[2].set()
            await tick(c); await tick(c)
            assert d.preparations == 1
            assert d.tasks['2']['status']['status'] == 'resolved'
            assert not c.integrations[1].done(), 'registration does not wait for acceptance completion'
        finally:
            await c.shutdown()
    asyncio.run(scenario())


@pytest.mark.parametrize('fails', [False, True])
def test_completion_before_handoff_retires_waiter_without_hiding_failure(fails):
    async def scenario():
        d = HandoffDriver(); d.tasks.pop('2')
        d.tasks['1']['status'] = dict(status='checking', candidate_sha256='a' * 64)
        async def integrate(*_):
            if fails:
                raise RuntimeError('retained acceptance authentication failed')
            d.tasks['1']['status'] = {'status': 'resolved'}
        d.integrate = integrate
        c = Coordinator(d)
        try:
            await tick(c)
            if fails:
                with pytest.raises(RuntimeError, match='authentication failed'):
                    await c.tick()
                assert d.preparations == 0
            else:
                await tick(c)
                assert d.preparations == 1 and not c.handoffs
        finally:
            await c.shutdown()
    asyncio.run(scenario())


def test_pending_launch_keeps_timeout_and_reap_responsive_and_pause_blocks_preparation():
    async def scenario():
        d = HandoffDriver(); d.tasks.pop('2')
        c = Coordinator(d)
        try:
            await tick(c)
            assert not c.runners[1].done() and d.preparations == 0
            d.time = 2000
            await tick(c); await tick(c)
            assert d.tasks['1']['status']['status'] == 'stopping'
            assert c.runners[1].cancelled() and not c.reaping[1].done()
            d.pausing = True
            d.cleanup[1].set()
            await tick(c); await tick(c)
            assert d.tasks['1']['status']['status'] == 'resolved'
            assert d.preparations == 0
        finally:
            await c.shutdown()
    asyncio.run(scenario())

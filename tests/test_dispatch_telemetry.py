import json
import sqlite3
import pytest

from trellis.dispatch.telemetry import Measurements
from trellis.dispatch.native import NativeLedger


def test_worker_turn_and_acceptance_measurements_remain_separate_requests(tmp_path):
    for relative in ('runtime', 'turns/1/runtime', 'turns/2/runtime', 'acceptance/1/runtime',
                     'acceptance/1/finalization/1/runtime', 'acceptance/1/finalization/2/runtime'):
        path = tmp_path / 'dispatch/1' / relative / 'checker-state/server.log'
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps(dict(measurement=dict(status='ok', request_elapsed_ms=10))) + '\n')
    row = Measurements(tmp_path).sample({'1': {}}, now=100)['tasks']['1']
    assert row['authoritative_requests'] == 6 and row['authoritative_elapsed_ms'] == 60


def test_append_truncation_and_admission_measurements(tmp_path):
    log = tmp_path / 'dispatch/1/runtime/checker-state/server.log'
    log.parent.mkdir(parents=True)
    row = json.dumps(dict(measurement=dict(status='ok', request_elapsed_ms=120,
        locks=[dict(mode='admission', waiting_ms=10, acquired_ms=35)]))).encode()
    log.write_bytes(row + b'\n' + row[:20])
    m = Measurements(tmp_path)
    first = m.sample({'1': {}}, now=100)['tasks']['1']
    assert first['authoritative_requests'] == 1
    assert first['queue_wait_ms'] == 25
    with log.open('ab') as stream:
        stream.write(row[20:] + b'\n')
    second = m.sample({'1': {}}, now=120)['tasks']['1']
    assert second['authoritative_requests'] == 2
    assert second['authoritative_elapsed_ms'] == 240
    log.write_bytes(row + b'\n')
    assert m.sample({'1': {}}, now=140)['tasks']['1']['authoritative_requests'] == 1


def test_jobs_queue_and_execution_are_separate_and_tokens_never_projected(tmp_path):
    path = tmp_path / 'checker-state/jobs/jobs.sqlite3'
    path.parent.mkdir(parents=True)
    with sqlite3.connect(path) as db:
        db.execute('CREATE TABLE jobs (request TEXT, submitted REAL, started REAL, finished REAL, state TEXT, result TEXT)')
        db.execute('INSERT INTO jobs VALUES (?,?,?,?,?,?)',
            (json.dumps(dict(scope='1', token='secret-token')), 100, 101, 103, 'done',
             json.dumps(dict(queue_ms=1000, execution_ms=2000, stdout='secret-output'))))
        db.execute('INSERT INTO jobs VALUES (?,?,?,?,?,?)',
            (json.dumps(dict(scope='1/acceptance/2')), 109, None, None, 'queued', None))
        db.execute('INSERT INTO jobs VALUES (?,?,?,?,?,?)',
            (json.dumps(dict(scope='unrelated')), 0, None, None, 'queued', None))
    m = Measurements(tmp_path)
    data = m.sample({'1': {}}, now=110)
    assert set(data['tasks']) == {'1'}
    row = data['tasks']['1']
    assert row['queue_wait_ms'] == 2000
    assert row['advisory_execution_ms'] == 2000
    assert row['advisory_jobs'] == 2 and row['queued'] == 1
    assert 'secret' not in json.dumps(data)
    assert not m.sample({}, now=130)['tasks']


def test_native_aggregates_preserve_payload_output_exceptions_and_lock_release(tmp_path):
    output = {'secret-result': ['unchanged']}
    payload = dict(action='dispatch_control', secret='not logged')
    seen = []
    def kernel(value):
        seen.append(value)
        return dict(status='dispatch_ok', output=output)
    native = NativeLedger(tmp_path, kernel=kernel)
    with native.locked():
        assert native._call(payload) is output
    assert seen == [payload] and seen[0] is payload
    error = RuntimeError('secret native refusal')
    def refuse(_):
        raise error
    native.kernel = refuse
    with pytest.raises(RuntimeError) as caught:
        with native.locked():
            native._call(dict(action='unknown-secret-action'))
    assert caught.value is error
    # A separate fd can acquire the same lock after the exceptional path.
    import fcntl
    with (tmp_path / 'dispatch/coordinator.lock').open('rb') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    rows = native.measurements.snapshot()['rows']
    assert rows['rpc:dispatch_control']['calls'] == 1
    assert rows['rpc:other']['failures'] == 1
    assert rows['lock_wait']['calls'] == rows['lock_held']['calls'] == 2
    data = Measurements(tmp_path).sample({}, now=100, native=native.measurements)
    assert 'secret' not in json.dumps(data)


def test_native_measurement_clock_failure_does_not_change_work(tmp_path, monkeypatch):
    import trellis.dispatch.telemetry as telemetry
    def failed_clock():
        raise RuntimeError('injected measurement failure')
    monkeypatch.setattr(telemetry.time, 'monotonic_ns', failed_clock)
    native = NativeLedger(tmp_path, kernel=lambda _: dict(status='dispatch_ok', output=42))
    with native.locked():
        assert native._call(dict(action='dispatch_control')) == 42
    assert native.measurements.snapshot()['rows'] == {}


def test_control_event_breakdown_is_bounded_and_preserves_aggregate(tmp_path):
    native = NativeLedger(tmp_path, kernel=lambda p: dict(status='dispatch_ok', output=p))
    payloads = [dict(action='dispatch_control', event=e) for e in
                (None, {'kind': 'launched', 'task_id': 99}, {'kind': 'candidate_ready'},
                 {'kind': 'secret-event-name'}, 'secret-invalid-event')]
    for payload in payloads:
        assert native._call(payload) is payload
    rows = native.measurements.snapshot()['rows']
    assert rows['rpc:dispatch_control']['calls'] == 5
    assert rows['control:read']['calls'] == 1
    assert rows['control:launched']['calls'] == 1
    assert rows['control:candidate_ready']['calls'] == 1
    assert rows['control:other']['calls'] == 2
    assert 'secret' not in json.dumps(rows)
    native.kernel = lambda _: dict(status='refused')
    with pytest.raises(RuntimeError):
        native._call(dict(action='dispatch_control', event={'kind': 'launched'}))
    assert native.measurements.snapshot()['rows']['control:launched']['failures'] == 1


def test_phase_exception_and_clock_failure_do_not_change_operation(monkeypatch):
    from trellis.dispatch import telemetry
    m = telemetry.NativeMeasurements()
    error = ValueError('original')
    with pytest.raises(ValueError) as caught:
        with telemetry.phase(m, 'workspace.sync'):
            raise error
    assert caught.value is error
    assert m.snapshot()['rows']['phase:workspace.sync']['failures'] == 1
    with telemetry.phase(m, 'secret-unbounded-name'):
        pass
    assert list(m.snapshot()['rows']) == ['phase:workspace.sync']
    def broken():
        raise RuntimeError('clock failed')
    monkeypatch.setattr(telemetry.time, 'monotonic_ns', broken)
    with telemetry.phase(m, 'workspace.copy'):
        pass
    with telemetry.phase(None, 'workspace.copy'):
        pass
    assert list(m.snapshot()['rows']) == ['phase:workspace.sync']


def test_staged_current_times_state_and_observation_without_extra_reads(tmp_path):
    from types import SimpleNamespace
    from trellis.dispatch.staged_driver import _current
    calls = []
    task = {'status': {'status': 'checking', 'candidate_sha256': 'sealed'}}
    state = {'dispatch': {'tasks': {'1': task}}}
    source = {'graph': {}}
    private = {'private': True}
    native = NativeLedger(tmp_path, kernel=lambda p: dict(status='dispatch_ok', output=private))
    def observe(s, repo):
        assert s is state
        calls.append('observe')
        return source
    native.observe_sources = observe
    def read_state():
        calls.append('state')
        return state
    driver = SimpleNamespace(native=native, repo=tmp_path, state=read_state,
                             require_no_halt=lambda: None)
    assert _current(driver, 1, 'sealed') == (task, source, private)
    assert calls == ['state', 'observe']
    rows = native.measurements.snapshot()['rows']
    assert rows['phase:acceptance.state_read']['calls'] == 1
    assert rows['phase:acceptance.source_observation']['calls'] == 1
    assert rows['rpc:dispatch_acceptance_state']['calls'] == 1


def test_optional_phase_sink_failure_never_masks_work_exception():
    from trellis.dispatch.telemetry import phase
    class BrokenSink:
        def begin(self, *_):
            raise RuntimeError('broken sink begin')
        def end(self, *_, **__):
            raise RuntimeError('broken sink end')
    original = ValueError('wrapped operation')
    with phase(BrokenSink(), 'workspace.sync'):
        pass
    with pytest.raises(ValueError) as caught:
        with phase(BrokenSink(), 'workspace.sync'):
            raise original
    assert caught.value is original


def test_cleanup_sample_identifies_process_and_current_drain_without_summing(tmp_path):
    import os
    from types import SimpleNamespace
    from trellis.dispatch.process import identity
    collector = SimpleNamespace(summary={'drain_id': '1', 'redundant_host_results': 3,
                                         'git_maintenance': {'repacks': 2}})
    measurement = Measurements(tmp_path)
    first = measurement.sample({}, now=100, cleanup=collector)['coordinator_cleanup']
    assert first['process'] == identity(os.getpid())
    assert first['scope'] == 'coordinator_process_current_drain'
    assert first['observer_started_at'] == measurement.started_at
    assert first['status'] == 'available' and first['sampled_at'] == 100
    assert 'excludes sidecars' in first['counter_semantics']
    collector.summary = {'drain_id': '2', 'redundant_host_results': 1,
                         'git_maintenance': {'repacks': 2}}
    second = measurement.sample({}, now=120, cleanup=collector)['coordinator_cleanup']
    assert second['summary']['redundant_host_results'] == 1  # Not a lifetime sum.
    assert second['summary']['drain_id'] == '2'
    assert first['summary']['redundant_host_results'] == 3
    assert json.loads((tmp_path / 'dispatch/telemetry.json').read_bytes())['coordinator_cleanup'] == second


def test_cleanup_unavailable_preserves_independent_last_success(tmp_path):
    class Collector:
        broken = False
        @property
        def summary(self):
            if self.broken:
                raise RuntimeError('private error is not telemetry')
            return {'drain_id': '1', 'git_maintenance': {'repacks': 2}}
    collector = Collector()
    measurement = Measurements(tmp_path)
    first = measurement.sample({}, now=100, cleanup=collector)
    first['coordinator_cleanup']['summary']['git_maintenance']['repacks'] = 999
    collector.broken = True
    result = measurement.sample({}, now=120, cleanup=collector)['coordinator_cleanup']
    assert result['status'] == 'unavailable'
    assert result['last_success'] == {'sampled_at': 100, 'summary': {
        'drain_id': '1', 'git_maintenance': {'repacks': 2}}}
    assert 'private error' not in json.dumps(result)


def test_cleanup_absence_new_observer_and_identity_failure_do_not_reuse_history(tmp_path, monkeypatch):
    from types import SimpleNamespace
    from trellis.dispatch import process
    prior = Measurements(tmp_path)
    prior.sample({}, now=100, cleanup=SimpleNamespace(summary={'drain_id': '1', 'owners': 9}))
    def unavailable(_):
        raise OSError('proc unavailable')
    monkeypatch.setattr(process, 'identity', unavailable)
    current = Measurements(tmp_path)
    result = current.sample({}, now=120)['coordinator_cleanup']
    assert result['status'] == 'not_started' and result['process'] is None
    assert 'summary' not in result and current._cleanup_sample is None
    assert 'owners' not in json.dumps(result)


def test_active_operations_keep_nested_thread_attribution_and_completed_rows():
    import threading
    from trellis.dispatch.telemetry import NativeMeasurements, phase
    measurements = NativeMeasurements()
    entered = threading.Event()
    release = threading.Event()
    thread_ids = []
    def worker():
        thread_ids.append(threading.get_native_id())
        with phase(measurements, 'workspace.copy'):
            entered.set()
            assert release.wait(5)
    worker_thread = threading.Thread(target=worker)
    with phase(measurements, 'acceptance.state_read'):
        worker_thread.start()
        try:
            assert entered.wait(5)
            first = measurements.snapshot()
            assert first['rows'] == {}
            active = {row['name']: row for row in first['active']}
            assert active['phase:workspace.copy']['native_thread_id'] == thread_ids[0]
            assert active['phase:acceptance.state_read']['native_thread_id'] == threading.get_native_id()
            assert all(row['elapsed_ms'] >= 0 for row in active.values())
            assert 'stale samples' in first['active_sampling']
        finally:
            release.set()
            worker_thread.join(5)
        assert not worker_thread.is_alive()
        during = measurements.snapshot()
        assert [row['name'] for row in during['active']] == ['phase:acceptance.state_read']
        assert during['rows']['phase:workspace.copy']['calls'] == 1
    final = measurements.snapshot()
    assert final['active'] == []
    assert final['rows']['phase:acceptance.state_read']['calls'] == 1
    assert len(first['active']) == 2  # Independent snapshot, not live mutable state.


def test_active_operations_bound_names_and_inventory_without_losing_aggregates():
    from trellis.dispatch.telemetry import NativeMeasurements, _MAX_ACTIVE_NATIVE_OPERATIONS
    measurements = NativeMeasurements()
    tickets = [measurements.begin('rpc', 'secret-payload-name')
               for _ in range(_MAX_ACTIVE_NATIVE_OPERATIONS + 2)]
    active = measurements.snapshot()
    assert len(active['active']) == _MAX_ACTIVE_NATIVE_OPERATIONS
    assert {row['name'] for row in active['active']} == {'rpc:other'}
    assert active['active_dropped_total'] == 2
    assert 'secret' not in json.dumps(active)
    for ticket in tickets:
        measurements.end(ticket)
    final = measurements.snapshot()
    assert final['active'] == []
    assert final['rows']['rpc:other']['calls'] == len(tickets)


def test_active_operation_cleanup_survives_failed_end_clock(monkeypatch):
    from trellis.dispatch import telemetry
    measurements = telemetry.NativeMeasurements()
    ticket = measurements.begin('phase', 'workspace.sync')
    assert len(measurements.snapshot()['active']) == 1
    def broken():
        raise RuntimeError('diagnostic clock failed')
    monkeypatch.setattr(telemetry.time, 'monotonic_ns', broken)
    measurements.end(ticket, ok=False)
    assert measurements.snapshot()['active'] == []
    assert measurements.snapshot()['rows'] == {}

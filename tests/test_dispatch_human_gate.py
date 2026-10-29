"""Human decisions enter parallel dispatch only through ordinary native Step."""
from contextlib import contextmanager
from types import SimpleNamespace

import pytest

from trellis.dispatch.driver import ProductionDriver
from trellis.runtime.kernel_cli import KernelCliError


def driver_at_gate(tmp_path, *, mode='approve', signal=True, paused=False, frozen=False, stage='HumanGate'):
    path = tmp_path / 'human_gate_response.json'
    raw = b'{"choice":"approve","cycle":59,"request_id":313}'
    if signal:
        path.write_bytes(raw)
    status = dict(stage=stage, tasks={}, verifiers={'jobs': {}}, pv_revision_ready=frozen,
                  routing_pending=False, reviewer_runnable=False)
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = driver.runtime = driver.repo = tmp_path
    driver.pausing = lambda: paused
    driver.require_no_halt = lambda: None
    driver._prepare_verifiers = lambda _: False
    driver.measurements = SimpleNamespace(sample=lambda _: None)
    events, locked = [], False
    @contextmanager
    def lock():
        nonlocal locked
        assert not locked
        locked = True
        try:
            yield
        finally:
            locked = False
    def native_step(payload):
        assert locked
        assert payload == dict(action='step', root=str(tmp_path)), 'host must not inject a human choice'
        assert path.read_bytes() == raw, 'only ordinary native bridge may consume the signal'
        events.append('step')
        if mode == 'stale':
            raise KernelCliError('missing human gate response file (stale or unstamped)')
        if mode == 'failure':
            raise KernelCliError('actual native source validation failed')
        if mode == 'unexpected':
            return {'status': 'dispatch_ok'}
        path.unlink()  # Ordinary native bridge consumption, not host code.
        status['stage'] = 'Start'
        status['phase'] = 'TheoremStating'
        return {'status': 'ok'}
    def current(payload):
        assert locked
        assert payload == dict(action='dispatch_control', root=str(tmp_path), event=None)
        events.append('status')
        return dict(status)
    driver.native = SimpleNamespace(locked=lock, kernel=native_step, _call=current,
                                   _require_recovered_publication=lambda: None)
    return driver, path, raw, events


def test_parallel_snapshot_consumes_existing_viewer_signal_once_and_exposes_new_state(tmp_path):
    driver, path, _, events = driver_at_gate(tmp_path)
    result = driver._snapshot()
    assert result['phase'] == 'TheoremStating' and result['stage'] == 'Start'
    assert not path.exists()
    assert events == ['status', 'step', 'status']
    driver._snapshot()
    assert events == ['status', 'step', 'status', 'status'], 'later snapshots must not repeat approval'


@pytest.mark.parametrize('options', [dict(signal=False), dict(paused=True), dict(frozen=True), dict(stage='Reviewer')])
def test_parallel_gate_waits_without_signal_or_under_pause_or_outside_current_gate(tmp_path, options):
    driver, path, raw, events = driver_at_gate(tmp_path, **options)
    driver._snapshot()
    assert events == ['status']
    if path.exists():
        assert path.read_bytes() == raw


def test_stale_human_signal_remains_pending_instead_of_stopping_coordinator(tmp_path):
    driver, path, raw, events = driver_at_gate(tmp_path, mode='stale')
    assert driver._snapshot()['stage'] == 'HumanGate'
    assert events == ['status', 'step'] and path.read_bytes() == raw


@pytest.mark.parametrize('mode,exception,message', [
    ('failure', KernelCliError, 'actual native source validation failed'),
    ('unexpected', RuntimeError, 'unexpected response'),
])
def test_gate_consumption_preserves_real_native_faults(tmp_path, mode, exception, message):
    driver, path, raw, events = driver_at_gate(tmp_path, mode=mode)
    with pytest.raises(exception, match=message):
        driver._snapshot()
    assert path.read_bytes() == raw and events == ['status', 'step']

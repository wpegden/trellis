"""A capacity interruption never repeats an already completed provider turn."""
import json
from pathlib import Path

import pytest
from trellis.modeling import driver as module
from trellis.checker.memory_retry import ResourceOutcome


@pytest.fixture
def driver(tmp_path, monkeypatch):
    (tmp_path / 'work').mkdir()
    (tmp_path / 'work/input').write_text('candidate')
    instance = module.Driver.__new__(module.Driver)
    instance.root, instance.path = tmp_path, tmp_path / 'state.json'
    instance.config = {}
    (tmp_path / 'config.json').write_text('{}')
    instance.state = dict(status='active', stage='setup', sequence=0,
        next={'role': 'worker'}, history=[], feedback='', observation={})
    monkeypatch.setattr(module, 'check_halt', lambda root: None)
    monkeypatch.setattr(module, 'source_fingerprint', lambda root: 'closed source')
    monkeypatch.setattr(module, 'runtime_root', lambda root: root)
    monkeypatch.setattr(module, 'commit_checkpoint', lambda payload: None)
    monkeypatch.setattr(module.time, 'sleep', lambda delay: None)
    return instance


def pressure():
    return ResourceOutcome(dict(kind='host_pressure_requeue', capacity_wait=True))


def test_pending_response_survives_restart_without_spending_another_turn(driver):
    calls = []
    def refresh():
        calls.append('check')
        if len(calls) == 3:
            raise pressure()
    def agent(*args):
        calls.append('provider')
        return {'summary': 'retained response'}
    driver.refresh, driver.agents = refresh, agent
    driver.apply = lambda task, result: calls.append('apply:' + result['summary'])
    assert driver.step()['status'] == 'active'
    assert driver.state['sequence'] == 1 and not driver.state['history']
    assert driver.state['pending_compute_response']['response']['summary'] == 'retained response'
    driver.state = json.loads(driver.path.read_text())
    driver.step()
    assert calls.count('provider') == 1 and calls.count('apply:retained response') == 1
    assert driver.state['sequence'] == 1 and len(driver.state['history']) == 1
    assert 'pending_compute_response' not in driver.state and 'capacity_wait' not in driver.state


def test_wait_before_agent_does_not_spend_sequence_or_halt(driver):
    def refresh():raise pressure()
    driver.refresh = refresh
    driver.agents = lambda *args: pytest.fail('provider must wait')
    driver.step()
    assert driver.state['sequence'] == 0 and driver.state['status'] == 'active'
    assert driver.state['capacity_wait']['capacity_wait']


def test_changed_candidate_cannot_reuse_pending_response(driver):
    driver.state['pending_compute_response'] = dict(task={'role': 'audit'},stage='setup',
        response={'summary':'stale approval'},work_fingerprint='obsolete')
    driver.refresh = lambda: None
    driver.agents = lambda *args: {'summary':'new response'}
    applied=[]
    driver.apply=lambda task,result: applied.append(result)
    driver.step()
    assert applied == [{'summary':'new response'}]


def test_performance_measurement_deferral_reuses_provider_decision(driver, monkeypatch):
    from trellis.modeling import performance_loop as loop
    driver.state['next'] = {'role': 'performance'}
    monkeypatch.setattr(loop, '_current', lambda root, state: None)
    monkeypatch.setattr(loop.performance, 'current_basis', lambda root: 'basis')
    monkeypatch.setattr(loop.performance, 'harness_fingerprint', lambda root: 'harness')
    calls=[]
    driver.agents=lambda *args: calls.append('provider') or {'action':'measure'}
    def apply(root, state, raw):
        calls.append('measure')
        if calls.count('measure')==1:raise pressure()
    monkeypatch.setattr(loop, 'apply', apply)
    driver.step()
    assert driver.state['status']=='active' and driver.state['sequence']==1
    assert not driver.state['history']
    driver.state=json.loads(driver.path.read_text())
    driver.step()
    assert calls==['provider','measure','measure']
    assert driver.state['sequence']==1 and len(driver.state['history'])==1
    assert 'pending_performance_response' not in driver.state

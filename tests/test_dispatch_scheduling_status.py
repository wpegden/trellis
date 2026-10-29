import copy
from contextlib import nullcontext
from types import SimpleNamespace

import pytest

from trellis.dispatch.driver import ProductionDriver
from trellis.dispatch.scheduling_status import SchedulingStatus


def files(tmp_path):
    paths = [tmp_path / name for name in ('state', 'metadata', 'config')]
    for p in paths:
        p.write_text('{}')
    return paths


def test_coalesces_same_state_with_independent_nested_results(tmp_path):
    paths, calls = files(tmp_path), []
    cache = SchedulingStatus()
    value = {'tasks': {'1': {'status': 'reserved'}}}
    def fetch():
        calls.append(True)
        return copy.deepcopy(value)
    first = cache.read(fetch, paths)
    first['tasks']['1']['status'] = 'running'
    second = cache.read(fetch, paths)
    second['tasks'].clear()
    assert cache.read(fetch, paths) == value
    assert len(calls) == 1 and cache.hits == 2


@pytest.mark.parametrize('change', ['replace', 'in_place', 'metadata', 'config', 'source', 'expiry'])
def test_reloads_after_input_change_or_bounded_age(tmp_path, change):
    paths, now, source, calls = files(tmp_path), [1], [1], []
    cache = SchedulingStatus(clock=lambda: now[0])
    def fetch():
        calls.append(True)
        return {'revision': len(calls)}
    def poll():
        return cache.read(fetch, paths, inputs=lambda: source[0])
    assert poll() == {'revision': 1}
    if change == 'replace':
        other = tmp_path / 'replacement'
        other.write_text('{}')
        other.replace(paths[0])
    elif change in ('in_place', 'metadata', 'config'):
        paths[{'in_place': 0, 'metadata': 1, 'config': 2}[change]].write_text('{"changed":true}')
    elif change == 'source':
        source[0] += 1
    else:
        now[0] += 30
    assert poll() == {'revision': 2}


def test_missing_input_and_failed_native_read_never_cached(tmp_path):
    paths, calls = files(tmp_path), []
    cache = SchedulingStatus()
    def fetch():
        calls.append(True)
        if len(calls) == 1:
            raise RuntimeError('native guard refused')
        return {'revision': len(calls)}
    with pytest.raises(RuntimeError, match='native guard'):
        cache.read(fetch, paths)
    paths[0].unlink()
    assert cache.read(fetch, paths)['revision'] == 2
    assert cache.read(fetch, paths)['revision'] == 3


def test_state_changed_inside_native_read_is_not_cached(tmp_path):
    paths, calls = files(tmp_path), []
    cache = SchedulingStatus()
    def fetch():
        calls.append(True)
        paths[0].write_text(str(len(calls)))
        return len(calls)
    assert cache.read(fetch, paths) == 1
    assert cache.read(fetch, paths) == 2


def driver_fixture(tmp_path):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.runtime = driver.repo = driver.root = tmp_path
    driver.config_path = tmp_path / 'trellis.config.json'
    for name in ('protocol_state.json', 'runtime_metadata.json', 'trellis.config.json'):
        (tmp_path / name).write_text('{}')
    calls, prepared, pause = [], [], [False]
    view = dict(stage='Reviewer', tasks={}, verifiers={'jobs': {}}, reviewer_runnable=True)
    def call(payload):
        calls.append(payload['action'])
        return {} if payload['action'] == 'dispatch_verifier_inputs' else copy.deepcopy(view)
    def recovered():
        if (tmp_path / 'pending').exists():
            raise RuntimeError('pending publication')
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=recovered,
                                   _call=call, measurements=None)
    driver.require_no_halt = lambda: None
    driver.pausing = lambda: pause[0]
    driver._prepare_verifiers = lambda status: prepared.append(True) or False
    driver.measurements = SimpleNamespace(sample=lambda *a, **kw: None)
    return driver, calls, prepared, pause, view


def test_cached_projection_keeps_pause_pending_guard_and_preparation_live(tmp_path):
    driver, calls, prepared, pause, _ = driver_fixture(tmp_path)
    assert driver._snapshot()['reviewer_runnable']
    assert driver._snapshot()['reviewer_runnable']
    assert len(prepared) == 2  # Retry preparation even though state did not change.
    pause[0] = True
    assert driver._snapshot()['pausing']
    pause[0] = False
    assert driver._snapshot()['reviewer_runnable']  # Pause did not contaminate cache.
    assert calls.count('dispatch_control') == 1
    (tmp_path / 'pending').touch()
    with pytest.raises(RuntimeError, match='pending publication'):
        driver._snapshot()


def test_cached_projection_does_not_skip_halt_or_new_human_gate_response(tmp_path):
    driver, calls, _, _, view = driver_fixture(tmp_path)
    view['stage'] = 'HumanGate'
    driver._snapshot()
    events = []
    driver.native.kernel = lambda p: events.append(p) or {'status': 'ok'}
    (tmp_path / 'human_gate_response.json').write_text('{}')
    driver._snapshot()
    assert len(events) == 1 and events[0]['action'] == 'step'
    def halt():
        raise RuntimeError('halt marker')
    driver.require_no_halt = halt
    with pytest.raises(RuntimeError, match='halt marker'):
        driver._snapshot()


def test_slow_fetch_coalesces_queued_poll_and_expires_after_completion(tmp_path):
    paths, now, calls = files(tmp_path), [100], []
    cache = SchedulingStatus(clock=lambda: now[0])
    def fetch():
        calls.append(True)
        now[0] += 80  # Longer than the entire diagnostic TTL.
        return len(calls)
    assert cache.read(fetch, paths) == 1
    assert now[0] == 180
    assert cache.read(fetch, paths) == 1  # Queued caller reuses validated state.
    now[0] = 209
    assert cache.read(fetch, paths) == 1
    now[0] = 210  # Hits do not slide the original completion deadline.
    assert cache.read(fetch, paths) == 2
    assert len(calls) == 2 and cache.hits == 2


def test_clock_rollback_rejects_cached_and_inflight_results(tmp_path):
    paths, now, calls = files(tmp_path), [100], []
    cache = SchedulingStatus(clock=lambda: now[0])
    def fetch():
        calls.append(True)
        return len(calls)
    assert cache.read(fetch, paths) == 1
    now[0] -= 1
    assert cache.read(fetch, paths) == 2
    def rollback():
        now[0] -= 1
        return fetch()
    paths[0].write_text('changed')
    assert cache.read(rollback, paths) == 3
    assert cache.read(fetch, paths) == 4


def test_slow_fetch_changed_source_does_not_publish_cache(tmp_path):
    paths, now, source, calls = files(tmp_path), [100], [1], []
    cache = SchedulingStatus(clock=lambda: now[0])
    def fetch():
        calls.append(True)
        now[0] += 80
        source[0] += 1
        return len(calls)
    assert cache.read(fetch, paths, inputs=lambda: source[0]) == 1
    assert cache.read(fetch, paths, inputs=lambda: source[0]) == 2
    assert cache.hits == 0


def test_validation_completion_includes_postread_scan_and_copy(tmp_path, monkeypatch):
    paths, now = files(tmp_path), [100]
    cache = SchedulingStatus(clock=lambda: now[0])
    def inputs():
        now[0] += 3
        return 'stable'
    deepcopy = copy.deepcopy
    def slow_copy(value):
        now[0] += 5
        return deepcopy(value)
    monkeypatch.setattr(copy, 'deepcopy', slow_copy)
    assert cache.read(lambda: {'valid': True}, paths, inputs=inputs) == {'valid': True}
    assert cache.started == 111


def test_external_configured_goal_edit_invalidates_cached_projection(tmp_path):
    import json
    driver, calls, _, _, _ = driver_fixture(tmp_path)
    goal = tmp_path / 'outside-repo-goal.md'
    goal.write_text('first goal')
    driver.config_path.write_text(json.dumps({'goal_file': str(goal)}))
    driver._snapshot()
    driver._snapshot()
    assert calls.count('dispatch_control') == 1
    goal.write_text('changed goal')
    driver._snapshot()
    assert calls.count('dispatch_control') == 2


def test_source_stat_inputs_cover_native_surfaces_and_ignore_generated_trees(tmp_path):
    import stat
    from trellis.dispatch.scheduling_status import source_inputs
    included = ['Tablet/A.lean', 'reference/evidence.json', 'arbitrary-root.txt',
                '.trellis/artifacts/replay.json', '.trellis/scripts/check.py']
    excluded = ['.git/objects/item', '.lake/build/one.olean', '.trellis-history/state.json',
                '.trellis/logs/log', '.trellis/runtime/code.py', '.trellis/chats/output',
                'reference/target/build', 'Tablet/__pycache__/cache', '.trellis-stop-after-checkpoint']
    for name in included + excluded:
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('first')
    first = source_inputs(tmp_path)
    assert {row[0] for row in first if stat.S_ISREG(row[3])} == set(included)
    for name in excluded:
        (tmp_path / name).write_text('changed excluded content')
    assert source_inputs(tmp_path) == first
    for name in included:
        before = source_inputs(tmp_path)
        (tmp_path / name).write_text('changed input')
        assert source_inputs(tmp_path) != before


def test_source_stat_inputs_notice_same_size_restored_mtime_rename_and_symlinks(tmp_path):
    import os
    from trellis.dispatch.scheduling_status import source_inputs
    path = tmp_path / 'input'
    path.write_text('old')
    before, stamp = source_inputs(tmp_path), path.stat()
    path.write_text('new')
    os.utime(path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    assert source_inputs(tmp_path) != before
    before = source_inputs(tmp_path)
    path.rename(tmp_path / 'renamed')
    assert source_inputs(tmp_path) != before
    path.symlink_to(tmp_path / 'renamed')
    assert source_inputs(tmp_path) is None


def test_uncacheable_source_inputs_force_native_refresh(tmp_path):
    paths, calls = files(tmp_path), []
    cache = SchedulingStatus()
    def fetch():
        calls.append(True)
        return len(calls)
    assert cache.read(fetch, paths, inputs=lambda: None) == 1
    assert cache.read(fetch, paths, inputs=lambda: None) == 2


def test_non_utf8_source_names_disable_cache(tmp_path):
    import os
    from trellis.dispatch.scheduling_status import source_inputs
    fd = os.open(os.fsencode(tmp_path) + b'/bad-\xff', os.O_CREAT | os.O_WRONLY, 0o600)
    os.close(fd)
    assert source_inputs(tmp_path) is None


def test_unreadable_optional_goal_disables_cache_without_inventing_native_failure(tmp_path, monkeypatch):
    from pathlib import Path
    driver, calls, _, _, _ = driver_fixture(tmp_path)
    original = Path.read_bytes
    def read_bytes(path):
        if path.name == 'GOAL.md':
            raise PermissionError('unreadable optional goal')
        return original(path)
    monkeypatch.setattr(Path, 'read_bytes', read_bytes)
    assert driver._snapshot()['reviewer_runnable']
    assert driver._snapshot()['reviewer_runnable']
    assert calls.count('dispatch_control') == 2


def test_coordinator_snapshot_passes_own_cleanup_and_diagnostics_do_not_gate_work(tmp_path):
    driver, calls, _, _, _ = driver_fixture(tmp_path)
    own_collector = object()
    driver._build_cleanup = own_collector
    seen = []
    def sample(*args, **kwargs):
        seen.append(kwargs['cleanup'])
        raise RuntimeError('telemetry sink unavailable')
    driver.measurements = SimpleNamespace(sample=sample)
    assert driver._snapshot()['reviewer_runnable']
    assert driver._snapshot()['reviewer_runnable']
    assert seen == [own_collector, own_collector]
    assert calls.count('dispatch_control') == 1


def test_resolved_verifier_retirement_tries_once_per_checkpoint(tmp_path, monkeypatch):
    driver,calls,_,_,view=driver_fixture(tmp_path)
    view['resolved_verifier_requests_retirable']=True
    head=['a'*40]
    monkeypatch.setattr('trellis.dispatch.driver.subprocess.check_output',lambda *a,**k: head[0]+'\n')
    driver._snapshot();driver._snapshot()
    assert calls.count('dispatch_retire_verifier_requests')==1
    head[0]='b'*40
    driver._snapshot()
    assert calls.count('dispatch_retire_verifier_requests')==2


def test_failed_verifier_retirement_does_not_consume_checkpoint_attempt(tmp_path, monkeypatch):
    driver,calls,_,_,view=driver_fixture(tmp_path)
    view['resolved_verifier_requests_retirable']=True
    monkeypatch.setattr('trellis.dispatch.driver.subprocess.check_output',lambda *a,**k:'a'*40+'\n')
    normal=driver.native._call
    def failing(payload):
        if payload['action']=='dispatch_retire_verifier_requests':raise RuntimeError('native refused')
        return normal(payload)
    driver.native._call=failing
    with pytest.raises(RuntimeError,match='native refused'):driver._snapshot()
    assert not hasattr(driver,'_verifier_retirement_attempted_checkpoint')
    driver.native._call=normal
    driver._snapshot()
    assert calls.count('dispatch_retire_verifier_requests')==1


def test_worker_archive_already_covers_verifiers_and_frozen_generation_skips(tmp_path,monkeypatch):
    driver,calls,_,_,view=driver_fixture(tmp_path)
    view.update(resolved_verifier_requests_retirable=True,tasks={'1':{'id':1,'request_archivable':True,'status':{'status':'resolved'}}})
    def forbidden(*a,**k):raise AssertionError('unexpected Git lookup')
    monkeypatch.setattr('trellis.dispatch.driver.subprocess.check_output',forbidden)
    driver._snapshot()
    assert calls.count('dispatch_archive_requests')==1
    view['pv_revision_ready']=True
    (tmp_path/'protocol_state.json').write_text('{"new":"generation"}')
    driver._snapshot()
    assert calls.count('dispatch_archive_requests')==1

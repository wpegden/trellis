import json
from pathlib import Path

import pytest

from trellis.dispatch.storage import collect, prune


def test_execution_discovery_includes_legacy_acceptance_and_worker_turns(tmp_path):
    from trellis.dispatch.storage import execution_directories
    for name in ('execution', 'acceptance/1/execution', 'turns/1/execution', 'turns/2/execution', 'acceptance/1/finalization/1/execution', 'acceptance/1/finalization/2/execution'):
        (tmp_path / name).mkdir(parents=True)
    assert {str(path.relative_to(tmp_path)) for path in execution_directories(tmp_path)} == {
        'execution', 'acceptance/1/execution', 'turns/1/execution', 'turns/2/execution',
        'acceptance/1/finalization/1/execution', 'acceptance/1/finalization/2/execution'}
    (tmp_path / 'turns/3').symlink_to(tmp_path / 'turns/2')
    with pytest.raises(ValueError, match='symlink'):
        execution_directories(tmp_path)


def test_only_resolved_attempt_build_caches_are_pruned(tmp_path):
    for id in (1, 2):
        root = tmp_path / str(id)
        for name in ['repo/.lake/build/A.olean', 'repo/.trellis/supervisor/repo/.lake/build/A.olean',
                     'repo/.lake/packages/mathlib/.lake/build/Mathlib.olean',
                     'repo/Tablet/A.lean', 'repo/.trellis/artifacts/A.olean',
                     'candidate/candidate.json', 'acceptance/1/repo/.lake/build/A.olean']:
            p = root / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text('retained evidence or disposable cache')
    state = dict(dispatch=dict(tasks={'1': {'status': {'status': 'resolved'}},
        '2': {'status': {'status': 'checking'}}}, verifiers={'jobs': {}}))
    collect(tmp_path, state)
    collect(tmp_path, state)
    assert not (tmp_path / '1/repo/.lake/build').exists()
    assert not (tmp_path / '1/repo/.lake').exists()
    assert not (tmp_path / '1/acceptance/1/repo/.lake/build').exists()
    assert (tmp_path / '2/repo/.lake/build/A.olean').is_file()
    for name in ['repo/Tablet/A.lean', 'repo/.trellis/artifacts/A.olean', 'candidate/candidate.json']:
        assert (tmp_path / '1' / name).is_file()


def test_pruning_requires_process_cleanup_and_never_follows_symlinks(tmp_path, monkeypatch):
    import trellis.dispatch.storage as storage
    execution = tmp_path / 'execution'
    execution.mkdir()
    identity = {'pid': 123}
    (execution / 'process.json').write_text(json.dumps(identity))
    monkeypatch.setattr(storage, 'alive', lambda _: False)
    with pytest.raises(RuntimeError, match='acknowledged process cleanup'):
        prune(tmp_path)
    (execution / 'cleanup.json').write_text(json.dumps(identity))
    outside = tmp_path / 'evidence'
    outside.mkdir()
    (outside / 'proof').write_text('keep')
    (tmp_path / 'repo').mkdir(parents=True)
    (tmp_path / 'repo/.lake').symlink_to(outside)
    with pytest.raises(RuntimeError, match='symlink'):
        prune(tmp_path)
    assert (outside / 'proof').read_text() == 'keep'


def test_old_prune_marker_upgrades_but_issue_pin_retains_builds(tmp_path):
    root = tmp_path / 'attempt'
    build = root / 'repo/.lake/packages/mathlib/.lake/build'
    build.mkdir(parents=True)
    (build / 'Mathlib.olean').write_text('private cache')
    marker = root / 'build-caches-pruned.json'
    marker.write_text('{"schema":1,"removed":["repo/.lake/build"]}')
    pin = root / 'retain-builds.json'
    pin.write_text('{"reason":"open checker investigation"}')
    prune(root)
    assert build.exists()
    pin.unlink()
    prune(root)
    assert not (root / 'repo/.lake').exists()
    assert json.loads(marker.read_text())['schema'] == 5


def test_correction_process_cleanup_and_external_package_cache_are_preserved(tmp_path, monkeypatch):
    import trellis.dispatch.storage as storage
    root = tmp_path / 'attempt'
    execution = root / 'turns/2/execution'
    execution.mkdir(parents=True)
    identity = {'pid': 123}
    (execution / 'process.json').write_text(json.dumps(identity))
    monkeypatch.setattr(storage, 'alive', lambda _: False)
    outside = tmp_path / 'shared-package-cache'
    outside.mkdir()
    (outside / 'Mathlib.olean').write_text('shared')
    lake = root / 'repo/.lake'
    lake.mkdir(parents=True)
    (lake / 'packages').symlink_to(outside)
    with pytest.raises(RuntimeError, match='acknowledged process cleanup'):
        prune(root)
    assert lake.exists()
    (execution / 'cleanup.json').write_text(json.dumps(identity))
    prune(root)
    assert not lake.exists()
    assert (outside / 'Mathlib.olean').read_text() == 'shared'



def terminal_state(*ids):
    return dict(dispatch=dict(tasks={str(id): {'status': {'status': 'resolved'}} for id in ids},
                              verifiers={'jobs': {}}))


def finish_batch(collector):
    # A single wake now drains the whole catalog, including durable fsyncs.
    # Slow filesystems must not turn the old two-success timing into a quota.
    assert collector.wait_idle(30)
    collector._thread.join(5)
    assert not collector._thread.is_alive()


def test_background_cleanup_drains_and_retries_and_honors_queued_pin(tmp_path):
    from trellis.dispatch.storage import BackgroundCollector
    for id in (1, 2, 3):
        lake = tmp_path / str(id) / 'repo/.lake'
        lake.mkdir(parents=True)
        (lake / 'cache').write_text('disposable')
    state = terminal_state(1, 2, 3)
    state['dispatch']['tasks']['3']['status']['status'] = 'checking'
    collector = BackgroundCollector(tmp_path)
    pin = tmp_path / '2/retain-builds.json'
    pin.write_text('{}')
    assert collector.schedule(state)
    finish_batch(collector)
    assert not (tmp_path / '1/repo/.lake').exists()
    assert (tmp_path / '2/repo/.lake').exists(), 'priority cannot bypass a pin'
    assert collector.schedule(state)
    finish_batch(collector)
    assert (tmp_path / '2/repo/.lake').exists()
    assert (tmp_path / '3/repo/.lake').exists(), 'live work never queued'
    pin.unlink()
    assert collector.schedule(state)
    finish_batch(collector)
    assert not (tmp_path / '2/repo/.lake').exists()
    assert (tmp_path / '3/repo/.lake').exists()


def test_background_cleanup_drains_metadata_catalog_without_turn_cap(tmp_path, monkeypatch):
    from trellis.dispatch import storage
    seen = []
    monkeypatch.setattr(storage, 'prune', lambda directory, **kwargs: seen.append(directory.name) or False)
    collector = storage.BackgroundCollector(tmp_path)
    state = terminal_state(*range(1, 74))
    for expected in (73, 146, 219):
        collector.schedule(state)
        finish_batch(collector)
        assert len(seen) == expected
    assert seen[:73] == [str(id) for id in range(1, 74)]


def test_background_cleanup_failure_rotates_and_restart_retries_partial_tree(tmp_path, monkeypatch, caplog):
    from trellis.dispatch import storage
    for id in (1, 2):
        for repo in ('repo', 'acceptance/1/repo'):
            lake = tmp_path / str(id) / repo / '.lake'
            lake.mkdir(parents=True)
            (lake / 'cache').write_text('disposable')
        (tmp_path / str(id) / 'build-caches-pruned.json').write_text('{"schema":1}')
    original = storage.shutil.rmtree
    failed = []
    def interrupted(path):
        if path == tmp_path / '1/acceptance/1/repo/.lake' and not failed:
            failed.append(True)
            raise OSError('interrupted deletion')
        original(path)
    monkeypatch.setattr(storage.shutil, 'rmtree', interrupted)
    collector = storage.BackgroundCollector(tmp_path)
    collector.schedule(terminal_state(1, 2))
    finish_batch(collector)
    assert 'Private build cleanup deferred' in caplog.text
    assert not (tmp_path / '1/repo/.lake').exists()
    assert (tmp_path / '1/acceptance/1/repo/.lake').exists()
    assert json.loads((tmp_path / '1/build-caches-pruned.json').read_text())['schema'] == 1
    assert not (tmp_path / '2/repo/.lake').exists(), 'one failure must not starve later attempts'
    restarted = storage.BackgroundCollector(tmp_path)
    restarted.schedule(terminal_state(1, 2))
    finish_batch(restarted)
    assert not (tmp_path / '1/acceptance/1/repo/.lake').exists()
    assert json.loads((tmp_path / '1/build-caches-pruned.json').read_text())['schema'] == 5


def test_background_collectors_do_not_overlap_across_handoff(tmp_path, monkeypatch):
    import threading
    from trellis.dispatch import storage
    started, release = threading.Event(), threading.Event()
    calls = []
    def blocked(directory, **kwargs):
        calls.append(directory)
        started.set()
        assert release.wait(5)
        return True
    monkeypatch.setattr(storage, 'prune', blocked)
    first, second = storage.BackgroundCollector(tmp_path), storage.BackgroundCollector(tmp_path)
    try:
        first.schedule(terminal_state(1))
        assert started.wait(1)
        assert not first.schedule(terminal_state(1)), 'busy collector must not queue'
        second.schedule(terminal_state(1))
        finish_batch(second)
        assert len(calls) == 1, 'separate cleanup lock excludes an overlapping coordinator'
    finally:
        release.set()
        finish_batch(first)


def test_attempt_root_symlink_and_mid_cleanup_pin_preserve_remaining_builds(tmp_path, monkeypatch):
    import trellis.dispatch.storage as storage
    outside = tmp_path / 'retained'
    outside.mkdir()
    alias = tmp_path / 'alias'
    alias.symlink_to(outside)
    with pytest.raises(RuntimeError, match='symlinked attempt'):
        prune(alias)
    for repo in ('repo', 'acceptance/1/repo'):
        (outside / repo / '.lake').mkdir(parents=True)
    original = storage.shutil.rmtree
    def pin_after_first(path):
        original(path)
        (outside / 'retain-builds.json').write_text('{}')
    monkeypatch.setattr(storage.shutil, 'rmtree', pin_after_first)
    assert prune(outside)
    assert not (outside / 'repo/.lake').exists()
    assert (outside / 'acceptance/1/repo/.lake').exists()
    assert not (outside / 'build-caches-pruned.json').exists()



def test_background_thread_start_failure_is_optional_and_retryable(tmp_path, monkeypatch, caplog):
    from trellis.dispatch import storage
    collector = storage.BackgroundCollector(tmp_path)
    original = storage.threading.Thread.start
    def fail(_):
        raise RuntimeError('cannot allocate thread')
    monkeypatch.setattr(storage.threading.Thread, 'start', fail)
    assert not collector.schedule(terminal_state(1))
    assert collector._thread is None
    assert 'cleanup could not start' in caplog.text
    monkeypatch.setattr(storage.threading.Thread, 'start', original)
    assert collector.schedule(terminal_state(1))
    finish_batch(collector)


def test_background_native_receipts_include_only_resolved_workers_verifiers_and_reviews(tmp_path, monkeypatch):
    from trellis.dispatch import storage
    state = terminal_state(1)
    state['dispatch']['tasks']['2'] = {'status': {'status': 'stopping'}}
    state['dispatch']['verifiers']['jobs'] = {
        '3': {'status': {'status': 'resolved'}}, '4': {'status': {'status': 'running'}}}
    state['dispatch']['receipts'] = {'5': {}}
    seen = []
    monkeypatch.setattr(storage, 'prune', lambda directory, **kwargs: seen.append(directory.name) or False)
    collector = storage.BackgroundCollector(tmp_path)
    collector.schedule(state)
    finish_batch(collector)
    assert set(seen) == {'1', 'verifier-3', 'review-5'}
    assert len(seen) == 3


@pytest.mark.parametrize('process_state', ['live', 'unacknowledged', 'wrong_cleanup'])
def test_nested_finalizer_process_blocks_every_private_build_deletion(tmp_path, monkeypatch, process_state):
    from trellis.dispatch import storage
    root = tmp_path / '1'
    lakes = [root / name / '.lake' for name in ('repo', 'turns/2/repo',
        'acceptance/1/repo', 'acceptance/1/finalization/1/repo')]
    for lake in lakes:
        lake.mkdir(parents=True)
        (lake / 'proof.olean').write_text('private build')
    execution = root / 'acceptance/1/finalization/1/execution'
    execution.mkdir()
    identity = {'pid': 321, 'start_ticks': 42}
    (execution / 'process.json').write_text(json.dumps(identity))
    if process_state != 'unacknowledged':
        (execution / 'cleanup.json').write_text(json.dumps(identity if process_state == 'live' else {'pid': 999}))
    monkeypatch.setattr(storage, 'alive', lambda value: process_state == 'live')
    with pytest.raises(RuntimeError, match='acknowledged process cleanup'):
        collect(tmp_path, terminal_state(1))
    assert all((lake / 'proof.olean').is_file() for lake in lakes)
    assert not (root / 'build-caches-pruned.json').exists()


def test_resolved_nested_layout_migrates_old_marker_preserving_all_evidence(tmp_path, monkeypatch):
    from trellis.dispatch import storage
    root = tmp_path / '1'
    attempts = [root, root / 'turns/1', root / 'acceptance/1', root / 'acceptance/1/finalization/1',
                root / 'acceptance/1/finalization/2']
    keep = []
    for attempt in attempts:
        for filename in ['repo/.lake/packages/cache.olean', 'repo/.trellis/supervisor/repo/.lake/build/cache.olean',
                         'repo/Tablet/A.lean', 'repo/.trellis/artifacts/A.olean', 'staged-completed.json',
                         'execution/response.json', 'runtime/staged-context.json']:
            path = attempt / filename
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('evidence')
            if '/.lake/' not in str(path):
                keep.append(path)
        identity = {'pid': len(keep), 'start_ticks': 7}
        for filename in ('process.json', 'cleanup.json'):
            path = attempt / 'execution' / filename
            path.write_text(json.dumps(identity)); keep.append(path)
    marker = root / 'build-caches-pruned.json'
    marker.write_text('{"schema":2,"removed":["repo/.lake"]}')
    monkeypatch.setattr(storage, 'alive', lambda _: False)
    assert prune(root)
    for attempt in attempts:
        assert not (attempt / 'repo/.lake').exists()
        assert not (attempt / 'repo/.trellis/supervisor/repo/.lake').exists()
    assert all(path.is_file() for path in keep)
    assert json.loads(marker.read_text())['schema'] == 5
    assert len(json.loads(marker.read_text())['removed']) == 10
    assert not prune(root), 'completed cleanup is idempotent'


@pytest.mark.parametrize('pin_parent', ['', 'acceptance/1/finalization/1'])
def test_nested_private_cleanup_respects_retention_pins(tmp_path, pin_parent):
    root = tmp_path / '1'
    lake = root / 'acceptance/1/finalization/1/repo/.lake'
    lake.mkdir(parents=True)
    pin = root / pin_parent / 'retain-builds.json'
    pin.write_text('{"reason":"investigation"}')
    assert not prune(root)
    assert lake.is_dir()
    assert not (root / 'build-caches-pruned.json').exists()


@pytest.mark.parametrize('alias', ['acceptance/1/finalization', 'acceptance/1/finalization/1',
                                 'acceptance/1/finalization/1/repo'])
def test_nested_cleanup_refuses_symlink_roots_before_removing_anything(tmp_path, alias):
    root = tmp_path / 'attempt'
    (root / 'repo/.lake').mkdir(parents=True)
    outside = tmp_path / 'outside'
    (outside / '.lake').mkdir(parents=True)
    target = root / alias
    target.parent.mkdir(parents=True, exist_ok=True)
    target.symlink_to(outside, target_is_directory=True)
    with pytest.raises((ValueError, RuntimeError), match='symlink'):
        prune(root)
    assert (root / 'repo/.lake').is_dir()
    assert (outside / '.lake').is_dir()


def test_cleanup_verifies_absence_before_writing_success_marker(tmp_path, monkeypatch):
    from trellis.dispatch import storage
    lake = tmp_path / 'acceptance/1/finalization/1/repo/.lake'
    lake.mkdir(parents=True)
    monkeypatch.setattr(storage.shutil, 'rmtree', lambda _: None)
    with pytest.raises(RuntimeError, match='left its target present'):
        prune(tmp_path)
    assert lake.is_dir()
    assert not (tmp_path / 'build-caches-pruned.json').exists()


def test_terminal_provider_catalog_cleanup_preserves_home_and_live_attempt(tmp_path):
    homes = []
    for id in ('1', '2'):
        home = tmp_path / id / 'runtime/burst-homes/worker-task' / '.codex'
        catalog = home / 'cache/remote_plugin_catalog'
        catalog.mkdir(parents=True)
        (catalog / 'catalog.json').write_text('{"plugins":[]}')
        for name in ('auth.json', 'sessions/transcript.jsonl', 'cache/other.json'):
            path = home / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('retained')
        homes.append(home)
        (tmp_path / id / 'build-caches-pruned.json').write_text('{"schema":3}')
    state = terminal_state(1)
    state['dispatch']['tasks']['2'] = {'status': {'status': 'checking'}}
    collect(tmp_path, state)
    assert not (homes[0] / 'cache/remote_plugin_catalog').exists()
    assert (homes[1] / 'cache/remote_plugin_catalog/catalog.json').is_file()
    for home in homes:
        for name in ('auth.json', 'sessions/transcript.jsonl', 'cache/other.json'):
            assert (home / name).read_text() == 'retained'
    assert json.loads((tmp_path / '1/build-caches-pruned.json').read_text()) == {
        'schema': 5, 'removed': ['runtime/burst-homes/worker-task/.codex/cache/remote_plugin_catalog']}
    assert not prune(tmp_path / '1')


def test_provider_catalog_respects_nested_pin_and_process_cleanup(tmp_path, monkeypatch):
    from trellis.dispatch import storage
    turn = tmp_path / 'turns/2'
    catalog = turn / 'runtime/burst-homes/worker-task/.codex/cache/remote_plugin_catalog'
    catalog.mkdir(parents=True)
    (catalog / 'catalog.json').write_text('{}')
    execution = turn / 'execution'
    execution.mkdir()
    identity = {'pid': 123}
    (execution / 'process.json').write_text(json.dumps(identity))
    monkeypatch.setattr(storage, 'alive', lambda _: False)
    with pytest.raises(RuntimeError, match='acknowledged process cleanup'):
        prune(tmp_path)
    assert catalog.is_dir()
    (execution / 'cleanup.json').write_text(json.dumps(identity))
    pin = turn / 'retain-builds.json'
    pin.write_text('{}')
    assert not prune(tmp_path)
    assert catalog.is_dir()
    pin.unlink()
    assert prune(tmp_path)
    assert not catalog.exists()


def test_terminal_provider_downloads_upgrade_old_marker_preserving_shared_and_unique_data(tmp_path):
    import os
    root = tmp_path / '1'
    home = root / 'turns/2/runtime/burst-homes/worker/.codex'
    caches = ('cache/codex_apps_tools', 'plugins/cache', 'skills/.system')
    shared = tmp_path / 'shared-download'
    shared.write_text('downloaded bytes')
    for name in caches:
        target = home / name
        target.mkdir(parents=True)
        os.link(shared, target / 'download')
        (target / 'internal-symlink').symlink_to(shared)
    (home / 'models_cache.json').write_text('{}')
    kept = ('auth.json', 'config.toml', 'state_5.sqlite', 'state_5.sqlite-wal',
            'thread_history_1.sqlite', 'sessions/transcript.jsonl',
            'skills/custom/SKILL.md', 'cache/unknown.json')
    for name in kept:
        path = home / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('unique retained data')
    (root / 'build-caches-pruned.json').write_text('{"schema":4}')
    collect(tmp_path, terminal_state(1))
    for name in (*caches, 'models_cache.json'):
        assert not (home / name).exists()
    for name in kept:
        assert (home / name).read_text() == 'unique retained data'
    assert shared.read_text() == 'downloaded bytes'
    assert shared.stat().st_nlink == 1


def test_provider_download_tracked_by_enclosing_git_is_preserved(tmp_path):
    import subprocess
    root = tmp_path / '1'
    home = root / 'runtime/burst-homes/worker/.codex'
    home.mkdir(parents=True)
    target = home / 'models_cache.json'
    target.write_text('tracked cache')
    subprocess.run(['git', 'init', '-q', str(root)], check=True)
    subprocess.run(['git', '-C', str(root), 'add', '.'], check=True)
    collect(tmp_path, terminal_state(1))
    assert target.read_text() == 'tracked cache'


@pytest.mark.parametrize('relative', ['models_cache.json', 'plugins/cache', 'skills/.system'])
def test_provider_download_alias_is_rejected_before_any_deletion(tmp_path, relative):
    root = tmp_path / '1'
    home = root / 'runtime/burst-homes/worker/.codex'
    target = home / relative
    target.parent.mkdir(parents=True)
    shared = tmp_path / 'shared'
    shared.write_text('preserved')
    target.symlink_to(shared)
    build = root / 'repo/.lake'
    build.mkdir(parents=True)
    with pytest.raises(RuntimeError, match='symlink'):
        collect(tmp_path, terminal_state(1))
    assert build.is_dir()
    assert shared.read_text() == 'preserved'


@pytest.mark.parametrize('alias', ['runtime', 'runtime/burst-homes',
                                  'runtime/burst-homes/worker-task',
                                  'runtime/burst-homes/worker-task/.codex/cache/remote_plugin_catalog'])
def test_provider_catalog_alias_refused_before_any_deletion(tmp_path, alias):
    root = tmp_path / 'attempt'
    (root / 'repo/.lake').mkdir(parents=True)
    relative = Path('runtime/burst-homes/worker-task/.codex/cache/remote_plugin_catalog')
    target = root / alias
    outside = tmp_path / 'outside'
    catalog = outside / relative.relative_to(alias)
    catalog.mkdir(parents=True)
    (catalog / 'catalog.json').write_text('shared cache')
    target.parent.mkdir(parents=True, exist_ok=True)
    target.symlink_to(outside, target_is_directory=True)
    with pytest.raises(RuntimeError, match='symlink'):
        prune(root)
    assert (root / 'repo/.lake').is_dir()
    assert (catalog / 'catalog.json').read_text() == 'shared cache'


def test_fresh_and_old_builds_both_drain_in_one_wake(tmp_path):
    from trellis.dispatch.storage import BackgroundCollector
    for id in range(1, 61):
        root = tmp_path / str(id)
        catalog = root / 'runtime/burst-homes/worker/.codex/cache/remote_plugin_catalog'
        catalog.mkdir(parents=True)
        (catalog / 'catalog.json').write_text('{}')
        (root / 'build-caches-pruned.json').write_text('{"schema":3}')
    latest = tmp_path / '71'
    build = latest / 'acceptance/1/finalization/1/repo/.lake/build'
    build.mkdir(parents=True)
    (build / 'proof.olean').write_text('disposable')
    evidence = latest / 'acceptance/1/finalization/1/staged-completed.json'
    evidence.write_text('retained exact completion')
    collector = BackgroundCollector(tmp_path)
    collector.schedule(terminal_state(*range(1, 61), 71))
    finish_batch(collector)
    assert not build.exists(), 'new worker must not wait behind 60 old catalog upgrades'
    assert evidence.read_text() == 'retained exact completion'
    upgraded = [id for id in range(1, 61)
                if json.loads((tmp_path / str(id) / 'build-caches-pruned.json').read_text())['schema'] == 5]
    assert upgraded == list(range(1, 61)), 'one drain also finishes the historical backlog'


def test_continuous_new_completions_do_not_starve_old_cleanup(tmp_path):
    from trellis.dispatch.storage import BackgroundCollector
    def add(id):
        lake = tmp_path / str(id) / 'repo/.lake'
        lake.mkdir(parents=True)
        (lake / 'cache').write_text('disposable')
    for id in range(1, 5):
        add(id)
        (tmp_path / str(id) / 'retain-builds.json').write_text('{}')
    collector = BackgroundCollector(tmp_path)
    collector.schedule(terminal_state(*range(1, 5)))
    finish_batch(collector)
    for id in range(1, 5):
        (tmp_path / str(id) / 'retain-builds.json').unlink()
    ids = list(range(1, 5))
    for old, fresh in zip(range(1, 5), range(100, 104)):
        add(fresh)
        ids.append(fresh)
        collector.schedule(terminal_state(*ids))
        finish_batch(collector)
        assert not (tmp_path / str(fresh) / 'repo/.lake').exists()
        assert not (tmp_path / str(old) / 'repo/.lake').exists()


def test_cleanup_drains_full_scan_and_revisits_refused_entries(tmp_path, monkeypatch):
    from trellis.dispatch import storage
    seen = []
    monkeypatch.setattr(storage, 'prune', lambda p, **kwargs: seen.append(p.name) or False)
    collector = storage.BackgroundCollector(tmp_path)
    state = terminal_state(*range(1, 75))
    collector.schedule(state)
    finish_batch(collector)
    assert len(seen) == 74
    assert seen == [str(id) for id in range(1, 75)]
    for _ in range(3):
        before = len(seen)
        collector.schedule(state)
        finish_batch(collector)
        assert len(seen) - before == 74
    assert set(seen) == {str(id) for id in range(1, 75)}
    assert seen.count('1') > 1, 'pins/refusals stay eligible for a later fair scan'


def test_multiple_fresh_workers_keep_priority_until_each_is_attempted(tmp_path):
    from trellis.dispatch.storage import BackgroundCollector
    for id in range(1, 61):
        root = tmp_path / str(id)
        catalog = root / 'runtime/burst-homes/worker/.codex/cache/remote_plugin_catalog'
        catalog.mkdir(parents=True)
        (catalog / 'catalog.json').write_text('{}')
        (root / 'build-caches-pruned.json').write_text('{"schema":3}')
    for id in range(71, 75):
        lake = tmp_path / str(id) / 'acceptance/1/finalization/1/repo/.lake'
        lake.mkdir(parents=True)
        (lake / 'cache').write_text('disposable')
    state = terminal_state(*range(1, 61), *range(71, 75))
    collector = BackgroundCollector(tmp_path)
    for wake in range(1, 5):
        collector.schedule(state)
        finish_batch(collector)
        remaining = [id for id in range(71, 75)
                     if (tmp_path / str(id) / 'acceptance/1/finalization/1/repo/.lake').exists()]
        assert remaining == []
        upgraded = [id for id in range(1, 61)
                    if json.loads((tmp_path / str(id) / 'build-caches-pruned.json').read_text())['schema'] == 5]
        assert upgraded == list(range(1, 61)), 'all historical catalogs finish in the first drain'


def test_busy_cleanup_lock_does_not_consume_fresh_priority(tmp_path):
    import fcntl
    from trellis.dispatch.storage import BackgroundCollector
    for id in (1, 71):
        (tmp_path / str(id) / 'repo/.lake').mkdir(parents=True)
    collector = BackgroundCollector(tmp_path)
    with (tmp_path / 'build-cleanup.lock').open('a+b') as held:
        fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        collector.schedule(terminal_state(1, 71))
        finish_batch(collector)
        assert collector.summary['owners'] == 0
        assert (tmp_path / '71/repo/.lake').exists()
    collector.schedule(terminal_state(1, 71))
    finish_batch(collector)
    assert not (tmp_path / '71/repo/.lake').exists()
    assert not (tmp_path / '1/repo/.lake').exists()

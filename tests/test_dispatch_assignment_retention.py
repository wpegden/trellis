from contextlib import contextmanager
import json
from pathlib import Path

import pytest

from test_dispatch_terminal_retention import setup, put
from trellis.dispatch import assignment_retention as assignments
from trellis.dispatch import retained_files as retained
from trellis.dispatch.acceptance_receipts import regular, regular_digest


def assignment(directory):
    path = directory / 'assignment.json'
    path.write_bytes(b'{"history":"' + b'large repeated input ' * 70000 + b'"}')
    return path


def test_committed_assignment_roundtrip_restart_and_lock_scope(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    path = assignment(directory)
    original = path.read_bytes(); before = regular_digest(path)
    held = False; calls = []
    original_lock = collector.retirement_lock
    @contextmanager
    def lock():
        nonlocal held
        with original_lock() as acquired:
            held = acquired; calls.append(acquired)
            try: yield acquired
            finally: held = False
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    original_chunks = retained._archived_chunks
    def chunks(p):
        assert not held
        yield from original_chunks(p)
    monkeypatch.setattr(retained, '_archived_chunks', chunks)
    original_unlink = Path.unlink
    def unlink(p, *args, **kwargs):
        if p == path: assert held
        return original_unlink(p, *args, **kwargs)
    monkeypatch.setattr(Path, 'unlink', unlink)
    original_sync = retained._sync_dir
    def sync(p):
        assert not held
        return original_sync(p)
    monkeypatch.setattr(retained, '_sync_dir', sync)
    assert assignments.collect(collector, directory, observed, apply=False)['status'] == 'eligible'
    result = assignments.collect(collector, directory, observed)
    assert result['archived'] == 1 and result['removed_bytes'] > 1000000
    assert result['removed_allocated_bytes'] > 1000000
    assert not path.exists() and regular(path) == original and regular_digest(path) == before
    assert calls == [True]
    assert assignments.collect(collector, directory, observed)['archived'] == 0


@pytest.mark.parametrize('failure', ['active', 'uncommitted', 'pin', 'live', 'cleanup', 'tracked', 'wrong_repo', 'pending'])
def test_initial_guards_preserve_plain_assignment(setup, monkeypatch, failure):
    collector, directory, observed, _, state = setup
    path = assignment(directory)
    if failure == 'active': observed = {}
    elif failure == 'uncommitted': observed = {**observed, '1': 'different'}
    elif failure == 'pin': put(directory / 'retain-builds.json', {})
    elif failure in ('live', 'cleanup'):
        put(directory / 'execution/process.json', {'pid': 123})
        if failure == 'live': put(directory / 'execution/cleanup.json', {'pid':123})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: failure == 'live')
    elif failure == 'tracked': monkeypatch.setattr(retained, '_untracked', lambda _: False)
    elif failure == 'wrong_repo': put(directory / 'prepared.json', {'source_repo':str(directory)})
    elif failure == 'pending': monkeypatch.setattr(collector, '_owner_pending', lambda _: True)
    try: result = assignments.collect(collector, directory, observed)
    except (ValueError, FileNotFoundError): pass
    else: assert result['archived'] == 0
    assert path.exists() and not retained._paths(path)[0].exists()


@pytest.mark.parametrize('failure', ['pin', 'checkpoint', 'pending', 'tracked', 'signature'])
def test_changes_during_compression_keep_raw_and_can_resume(setup, monkeypatch, failure):
    collector, directory, observed, _, _ = setup
    path = assignment(directory)
    original = retained._archived_chunks
    def chunks(p):
        yield from original(p)
        if failure == 'pin': put(directory / 'retain-builds.json', {})
        elif failure == 'checkpoint': monkeypatch.setattr(collector, 'checkpoint_current', lambda _: False)
        elif failure == 'pending': monkeypatch.setattr(collector, '_owner_pending', lambda _: True)
        elif failure == 'tracked': monkeypatch.setattr(retained, '_untracked', lambda _: False)
        elif failure == 'signature': path.write_bytes(path.read_bytes() + b' ')
    monkeypatch.setattr(retained, '_archived_chunks', chunks)
    with pytest.raises(ValueError): assignments.collect(collector, directory, observed)
    assert path.exists()


def test_busy_lock_is_restartable_without_overwriting_archive(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    path = assignment(directory); original_lock = collector.retirement_lock
    @contextmanager
    def busy(): yield False
    monkeypatch.setattr(collector, 'retirement_lock', busy)
    assert assignments.collect(collector, directory, observed)['status'] == 'coordinator_busy'
    assert path.exists()
    compressed = retained._paths(path)[0].read_bytes()
    monkeypatch.setattr(collector, 'retirement_lock', original_lock)
    assert assignments.collect(collector, directory, observed)['archived'] == 1
    assert retained._paths(path)[0].read_bytes() == compressed


def test_actual_committed_batch_recovery_reads_archived_assignment(project, monkeypatch):
    from test_dispatch_batch_cache import make_batch
    from trellis.dispatch import batch_storage, terminal_retention
    from test_dispatch_terminal_retention import git
    driver, batch_path, workers = make_batch(project)
    driver._materialize_batch(workers, path=batch_path, committed=False)
    directory = driver.root / '1'
    # Grow the fixture's immutable captured assignment, preserving its real
    # preparation and seal readers without allocating a production-size state.
    from trellis.dispatch.candidate import CandidateError, canonical_json
    workers = {key: workers[key] for key in workers}
    workers['1']['assignment']['retention_fixture'] = 'repeated context ' * 1000
    (directory / 'assignment.json').write_bytes(canonical_json(workers['1']['assignment']))
    batch_storage.seal_capture(directory)
    task = dict(id=1, review_id=7, reserved_at_ms=12, assignment=dict(workers['1']['assignment'], assignment_id='worker-one'),
                status=dict(status='resolved', outcome='accepted'))
    state = dict(dispatch=dict(tasks={'1':task}, verifiers=dict(jobs={})))
    put(driver.repo / terminal_retention.HISTORY, dict(state=state))
    git(driver.repo, 'add', terminal_retention.HISTORY)
    git(driver.repo, 'commit', '-qm', 'Terminal worker')
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    put(driver.root / 'coordinator.lock', {})
    collector = terminal_retention.Collector(driver.root, driver.repo)
    assert assignments.collect(collector, directory, terminal_retention.terminal_tasks(state))['archived'] == 1
    assert not (directory / 'assignment.json').exists()
    batch_storage.validate_capture_seal(directory)
    batch_storage.adopt_capture(directory, workers['1'])
    driver._materialize_batch(workers, path=batch_path, committed=True)
    archive = retained._paths(directory / 'assignment.json')[0]
    archive.write_bytes(archive.read_bytes()[:-8] + b'corrupt!')
    with pytest.raises(CandidateError, match="compressed stream is corrupt"):
        batch_storage.validate_capture_seal(directory)


from test_dispatch_workspace import project


@pytest.mark.parametrize('tracked_name', ['assignment.json.gz', 'assignment.json.retained.json', 'assignment.json.retained-skip.json'])
def test_tracked_output_is_never_written(setup, monkeypatch, tracked_name):
    collector, directory, observed, _, _ = setup
    path = assignment(directory)
    original = retained._untracked
    monkeypatch.setattr(retained, '_untracked', lambda p: Path(p).name != tracked_name and original(p))
    assert assignments.collect(collector, directory, observed)['status'] == 'tracked'
    assert path.exists() and not retained._paths(path)[0].exists()


def test_queued_real_lock_cancellation_preserves_assignment(setup, monkeypatch):
    import fcntl
    import threading
    collector, directory, observed, _, _ = setup
    path = assignment(directory)
    collector.wait_for_lock = True
    collector.stop_event = threading.Event()
    attempted = threading.Event()
    original_lock = collector.retirement_lock
    @contextmanager
    def lock():
        attempted.set()
        with original_lock() as acquired:
            yield acquired
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    result = []; errors = []
    def collect():
        try: result.append(assignments.collect(collector, directory, observed))
        except Exception as error: errors.append(error)
    with (collector.root / 'coordinator.lock').open('a+b') as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        thread = threading.Thread(target=collect)
        thread.start()
        assert attempted.wait(10)
        collector.stop_event.set()
    thread.join(5)
    assert not thread.is_alive()
    assert not errors and result[0]['status'] == 'cancelled'
    assert path.exists()


def test_background_archives_assignment_and_reports_net_savings(setup):
    from trellis.dispatch.storage import BackgroundCollector
    original, directory, _, private_paths, state = setup
    path = assignment(directory)
    raw = path.read_bytes()
    allocated = path.stat().st_blocks * 512
    private_allocated = sum(p.stat().st_blocks * 512 for p in private_paths)
    collector = BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    assert collector.schedule(state) and collector.wait_idle(15)
    assert collector.summary['assignment_archives'] == 1
    assert collector.summary['errors'] == 0
    remaining = sum(p.stat().st_blocks * 512 for p in retained._paths(path))
    assert collector.summary['removed_allocated_bytes'] == private_allocated + allocated - remaining
    assert not path.exists() and regular(path) == raw


def test_background_cancellation_before_assignment_preserves_plain_bytes(setup, monkeypatch):
    from trellis.dispatch import framework_retention
    from trellis.dispatch.storage import BackgroundCollector
    original, directory, _, _, state = setup
    path = assignment(directory)
    collector = BackgroundCollector(original.root, repo=original.repo)
    collector._review_snapshots = None
    def cancel(*args, **kwargs):
        collector.close()
        return {'removed': 0}
    monkeypatch.setattr(framework_retention, 'retire', cancel)
    assert collector.schedule(state) and collector.wait_idle(15)
    assert collector.summary['assignment_archives'] == 0
    assert path.exists() and not retained._paths(path)[0].exists()

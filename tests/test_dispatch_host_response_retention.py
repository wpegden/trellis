from contextlib import contextmanager
import asyncio
import json
from pathlib import Path
from types import SimpleNamespace

import pytest
from test_dispatch_terminal_retention import setup, put
from trellis.dispatch import host_response_retention as hosts, retained_files as retained
from trellis.dispatch.acceptance_receipts import regular, regular_digest


def host(directory, relative='acceptance/1'):
    attempt = directory / relative
    put(attempt / 'prepared.json', json.loads((directory / 'prepared.json').read_text()))
    path = put(attempt / 'execution/response.json', {'facts': 'repeated exact evidence ' * 60000})
    return path


def test_host_round_and_finalization_archive_only_with_exact_hashes(setup):
    collector, directory, observed, _, _ = setup
    paths = [host(directory), host(directory, 'acceptance/1/finalization/1')]
    excluded = [host(directory, 'turns/1'), host(directory, 'turns/1/transports/1')]
    originals = {p: p.read_bytes() for p in paths + excluded}
    assert hosts.collect(collector, directory, observed, apply=False)['eligible'] == 2
    result = hosts.collect(collector, directory, observed)
    assert result['archived'] == 2 and result['removed_allocated_bytes'] > 2000000
    assert all(not p.exists() and regular(p) == originals[p] for p in paths)
    assert all(p.read_bytes() == originals[p] for p in excluded)
    assert hosts.collect(collector, directory, observed)['archived'] == 0
    for p in paths:
        assert retained.restore(p)
        assert p.read_bytes() == originals[p]


@pytest.mark.parametrize('failure', ['active', 'uncommitted', 'pending', 'pin', 'host_pin', 'live', 'unreaped', 'tracked', 'tracked_output', 'wrong_project', 'alias', 'hardlink'])
def test_guard_refusals_preserve_host_bytes(setup, monkeypatch, failure):
    collector, directory, observed, _, _ = setup
    path = host(directory)
    original = path.read_bytes()
    if failure == 'active': observed = {}
    elif failure == 'uncommitted': observed = {'1': 'different'}
    elif failure == 'pending': monkeypatch.setattr(collector, '_owner_pending', lambda _: True)
    elif failure == 'pin': put(directory / 'retain-builds.json', {})
    elif failure == 'host_pin': put(path.parents[1] / 'retain-builds.json', {})
    elif failure in ('live','unreaped'):
        put(path.parent / 'process.json', {'pid': 12})
        if failure == 'live': put(path.parent / 'cleanup.json', {'pid': 12})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: failure == 'live')
    elif failure == 'tracked': monkeypatch.setattr(retained, '_untracked', lambda _: False)
    elif failure == 'tracked_output': monkeypatch.setattr(retained, '_untracked', lambda p: not str(p).endswith('.gz'))
    elif failure == 'wrong_project': put(path.parents[1] / 'prepared.json', {'source_repo': str(directory)})
    elif failure == 'alias':
        other = path.with_name('original');path.rename(other);path.symlink_to(other)
    elif failure == 'hardlink': path.with_name('alias').hardlink_to(path)
    try: result = hosts.collect(collector, directory, observed)
    except (ValueError, FileNotFoundError): pass
    else: assert result['archived'] == 0
    assert path.read_bytes() == original
    assert not retained._paths(path)[0].exists()


def test_archive_preparation_is_offlock_and_final_pin_change_preserves_raw(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    path = host(directory); held = False
    original_lock = collector.retirement_lock
    @contextmanager
    def lock():
        nonlocal held
        with original_lock() as acquired:
            held = acquired
            try: yield acquired
            finally: held = False
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    chunks = retained._archived_chunks
    def verify(p):
        assert not held
        yield from chunks(p)
        put(path.parents[1] / 'retain-builds.json', {})
    monkeypatch.setattr(retained, '_archived_chunks', verify)
    with pytest.raises(ValueError): hosts.collect(collector, directory, observed)
    assert path.exists()


def test_interrupted_final_fence_resumes_exact_archive(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    path = host(directory); original = path.read_bytes(); lock = collector.retirement_lock
    @contextmanager
    def busy(): yield False
    monkeypatch.setattr(collector, 'retirement_lock', busy)
    assert hosts.collect(collector, directory, observed)['status'] == 'coordinator_busy'
    assert path.exists()
    compressed = retained._paths(path)[0].read_bytes()
    monkeypatch.setattr(collector, 'retirement_lock', lock)
    assert hosts.collect(collector, directory, observed)['archived'] == 1
    assert retained._paths(path)[0].read_bytes() == compressed and regular(path) == original


def test_real_host_wait_adopts_archived_response_and_checks_original_digest(setup, monkeypatch):
    from trellis.dispatch.driver import ProductionDriver
    from trellis.dispatch.candidate import digest
    collector, directory, observed, _, _ = setup
    path = host(directory); data = path.read_bytes()
    result = {'returncode': 0, 'response_sha256': digest(data)}
    process = SimpleNamespace(root=path.parent, result=lambda: result)
    assert hosts.collect(collector, directory, observed)['archived'] == 1
    driver = ProductionDriver.__new__(ProductionDriver)
    assert asyncio.run(driver._wait(process, {}, host_check=True)) == json.loads(data)
    result['response_sha256'] = 'bad'
    with pytest.raises(RuntimeError, match='completion receipt'):
        asyncio.run(driver._wait(process, {}, host_check=True))
    # Ordinary worker/provider responses retain their existing raw semantics.
    with pytest.raises(FileNotFoundError):
        asyncio.run(driver._wait(process, {}, host_check=False))


def test_background_owner_hook_archives_and_reports_net_savings(setup, monkeypatch):
    from trellis.dispatch.storage import BackgroundCollector
    collector, directory, observed, _, state = setup
    path = host(directory)
    # Remove unrelated tiny synthetic source input fixtures; the production
    # scheduling/terminal guard and host archive are exercised unchanged.
    monkeypatch.setattr('trellis.dispatch.auxiliary_retention.retire', lambda *a, **k: {'removed':0})
    monkeypatch.setattr('trellis.dispatch.worker_source_retention.retire', lambda *a, **k: {'removed':0})
    background = BackgroundCollector(collector.root, repo=collector.repo, git_maintenance_threshold_bytes=None)
    try:
        assert background.schedule(state)
        assert background.wait_idle(15)
        assert not path.exists()
        assert background.summary['host_response_archives'] == 1
        assert background.summary['removed_allocated_bytes'] > 1000000
        assert background.summary['errors'] == 0
    finally: background.close()


def test_restore_cli_restores_host_family_and_preserves_operator_pin(setup):
    import subprocess
    import sys
    collector, directory, observed, _, _ = setup
    paths = [host(directory), host(directory, 'acceptance/1/finalization/1')]
    before = {p:p.read_bytes() for p in paths}
    assert hosts.collect(collector, directory, observed)['archived'] == 2
    pin = put(directory / 'retain-builds.json', {'reason':'operator investigation'})
    original_pin = pin.read_bytes()
    command = [sys.executable, '-m', 'trellis.dispatch.retained_files', '--runtime',str(collector.root.parent), '--task','1','--restore']
    result = subprocess.run(command, capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    assert len(json.loads(result.stdout)['restored']) == 2
    assert pin.read_bytes() == original_pin
    assert all(p.read_bytes() == data for p,data in before.items())
    again = subprocess.run(command, capture_output=True, text=True)
    assert again.returncode == 0 and json.loads(again.stdout)['restored'] == []


def test_prepared_change_while_queued_never_removes_plain_response(setup, monkeypatch):
    collector, directory, observed, _, _ = setup
    path = host(directory)
    chunks = retained._archived_chunks
    def changed(p):
        yield from chunks(p)
        put(path.parents[1] / 'prepared.json', {'source_repo':str(collector.repo),'changed':True})
    monkeypatch.setattr(retained, '_archived_chunks', changed)
    with pytest.raises(ValueError, match='preparation changed'):
        hosts.collect(collector, directory, observed)
    assert path.exists()


def test_resolved_owner_adoption_never_reads_archived_host_inputs(setup, monkeypatch):
    from contextlib import nullcontext
    from trellis.dispatch.driver import ProductionDriver
    from trellis.dispatch import driver as module
    collector, directory, observed, _, state = setup
    path = host(directory)
    assert hosts.collect(collector, directory, observed)['archived'] == 1
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = collector.root
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None)
    driver.state = lambda: state
    monkeypatch.setattr(module, 'read', lambda _: pytest.fail('resolved host evidence must not be read'))
    assert driver._restore_acceptance_reservations() == {}
    assert driver._prepare_acceptance(1, 1, 'candidate') is None


def test_cancellation_after_archive_verification_keeps_plain_response(setup, monkeypatch):
    import threading
    collector, directory, observed, _, _ = setup
    collector.stop_event = threading.Event()
    path = host(directory)
    chunks = retained._archived_chunks
    def cancel(p):
        yield from chunks(p)
        collector.stop_event.set()
    monkeypatch.setattr(retained, '_archived_chunks', cancel)
    assert hosts.collect(collector, directory, observed)['status'] == 'cancelled'
    assert path.exists()

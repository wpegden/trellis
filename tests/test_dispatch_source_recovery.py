from contextlib import contextmanager
import gzip
import hashlib
import json
import os
from pathlib import Path
import stat

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_worker_source_retention import worker_setup
from trellis.dispatch import source_recovery as recovery, recoverable_source_retention as copies
from trellis.dispatch import terminal_retention as terminal, handoffs, workspace, candidate


@pytest.fixture
def failed(worker_setup):
    collector, directory, _, data, state = worker_setup
    task = state['dispatch']['tasks']['1']
    task['status'] = dict(status='resolved', outcome=dict(kind='stuck', reason='unfinished'), resolved_at_ms=100)
    task['assignment']['scope'] = dict(nodes=['A', 'Unique', 'Deleted'])
    task['pending_retry'] = dict(attempt=1, outcome_kind='Stuck')
    for relative in ['Tablet/Deleted.lean', 'reference/deleted.txt']:
        p = collector.repo / relative; p.parent.mkdir(parents=True, exist_ok=True); p.write_text('Originally present')
    git(collector.repo, 'add', '.'); git(collector.repo, 'commit', '-qm', 'Original deleted files')
    prepared = json.loads((directory/'prepared.json').read_text())
    prepared['source_head'] = git(collector.repo, 'rev-parse', 'HEAD')
    put(directory/'prepared.json', prepared)
    source = directory/'repo/Tablet/A.lean'; source.chmod(0o600)
    os.utime(source, ns=(1000000000, 2000000000))
    (directory/'repo/Tablet/Unique.lean').write_text('Unique unfinished proof')
    base = json.loads((directory/'base-manifest.json').read_text())
    base['Tablet/Deleted.lean'] = candidate.digest(b'Originally present')
    put(directory/'base-manifest.json', base)
    process = dict(pid=2147483647, start_ticks=1, boot_id='finished')
    put(directory/'execution/process.json', process); put(directory/'execution/cleanup.json', process)
    put(collector.repo/terminal.HISTORY, {'state': state})
    put(collector.root.parent/'protocol_state.json', state)
    git(collector.repo, 'add', terminal.HISTORY); git(collector.repo, 'commit', '-qm', 'Terminal stuck witness')
    return collector, directory, terminal.terminal_tasks(state), state


def snapshot(root):
    return {str(p.relative_to(root)): (p.read_bytes(), stat.S_IMODE(p.stat().st_mode), p.stat().st_mtime_ns)
            for p in root.rglob('*') if p.is_file()}


def copied_sources(directory, target):
    for surface in ('Tablet', 'reference'):
        workspace._copy_tree(directory/'repo'/surface, target/surface)
    recovery.Sources(directory).materialize(target)


def test_exact_retry_tree_and_full_future_handoff_preserve_deletions_and_unique_bytes(failed, tmp_path):
    collector, directory, observed, state = failed
    before = tmp_path/'before'; copied_sources(directory, before)
    task = state['dispatch']['tasks']['1']
    reviewer_before = tmp_path/'reviewer-before'
    metadata = handoffs.worker_handoffs(collector.root, reviewer_before, [task])
    evidence = {str(p.relative_to(reviewer_before)): p.read_bytes() for p in reviewer_before.rglob('*') if p.is_file()}
    finding = handoffs._findings(directory, task)
    assert any(s.get('deleted') for s in json.loads(finding)['sources'])
    (directory/'unaccepted-findings.json').unlink()
    result = copies.retire(collector, directory, observed, apply=True)
    assert result['removed'] == 2 and result['new_recovery_index_allocated_bytes'] > 0
    assert not (directory/'repo/Tablet/A.lean').exists()
    assert (directory/'repo/Tablet/Unique.lean').read_text() == 'Unique unfinished proof'
    after = tmp_path/'after'; copied_sources(directory, after)
    assert snapshot(after) == snapshot(before)
    assert not (after/'Tablet/Deleted.lean').exists() and not (after/'reference/deleted.txt').exists()
    reviewer_after = tmp_path/'reviewer-after'
    assert handoffs.worker_handoffs(collector.root, reviewer_after, [task]) == metadata
    assert {str(p.relative_to(reviewer_after)): p.read_bytes() for p in reviewer_after.rglob('*') if p.is_file()} == evidence
    assert handoffs._findings(directory, task) == finding
    receipt = json.loads((directory/recovery.RECEIPT).read_text())
    assert receipt['completed'] and receipt['files'] == [] and receipt['recovery_sha256']
    assert copies.retire(collector, directory, observed, apply=True)['removed'] == 0


def test_actual_workspace_prepare_previous_reconstructs_retired_sources(failed, monkeypatch):
    collector, directory, observed, _ = failed
    config = put(collector.repo/'trellis.config.json', {'repo_path': str(collector.repo)})
    put(collector.root.parent/'runtime_metadata.json', {'repo_path': str(collector.repo), 'config_path': str(config)})
    before = directory.parent/'before'; copied_sources(directory, before)
    copies.retire(collector, directory, observed, apply=True)
    destination = collector.root/'99'
    workspace.prepare(collector.root.parent, collector.repo, config, destination,
        {'in_flight_request': {'id': 99, 'kind': 'Worker'}},
        expected_manifest=candidate.source_manifest(collector.repo), previous=directory, seed_packages=False)
    recovered = destination/'repo/last_invalid'
    for surface in ('Tablet', 'reference'):
        assert snapshot(recovered/surface) == snapshot(before/surface)
    assert json.loads((recovered/'metadata.json').read_text())['preserved_source_surfaces'] == {'Tablet': True, 'reference': True}


def test_partial_unlink_restart_uses_one_durable_index(failed, monkeypatch, tmp_path):
    collector, directory, observed, _ = failed
    before = tmp_path/'before'; copied_sources(directory, before)
    original = Path.unlink; fired = False
    def unlink(path, *args, **kwargs):
        nonlocal fired
        if path == directory/'repo/reference/context.txt' and not fired:
            fired = True; raise OSError('interrupted unlink')
        return original(path, *args, **kwargs)
    monkeypatch.setattr(Path, 'unlink', unlink)
    with pytest.raises(OSError, match='interrupted'):
        copies.retire(collector, directory, observed, apply=True)
    index = (directory/recovery.INDEX).read_bytes()
    partial = tmp_path/'partial'; copied_sources(directory, partial)
    assert snapshot(partial) == snapshot(before)
    assert copies.retire(collector, directory, observed, apply=True)['removed'] == 1
    assert (directory/recovery.INDEX).read_bytes() == index


@pytest.mark.parametrize('field,value', [('mode', 0o777), ('mtime_ns', 4), ('atime_ns', 'bad'), ('bytes', True)])
def test_orphan_index_metadata_change_refused_before_any_unlink(failed, monkeypatch, field, value):
    collector, directory, observed, _ = failed
    from trellis.dispatch import auxiliary_retention as engine
    original = engine.atomic
    def stop_receipt(path, data):
        if path == directory/recovery.RECEIPT:
            raise OSError('before receipt')
        return original(path, data)
    monkeypatch.setattr(engine, 'atomic', stop_receipt)
    with pytest.raises(OSError, match='before receipt'):
        copies.retire(collector, directory, observed, apply=True)
    path = directory/recovery.INDEX
    manifest = json.loads(gzip.decompress(path.read_bytes()))
    manifest['files']['Tablet/A.lean'][field] = value
    path.write_bytes(gzip.compress(candidate.canonical_json(manifest), mtime=0))
    with pytest.raises(ValueError):
        copies.retire(collector, directory, observed, apply=True)
    assert (directory/'repo/Tablet/A.lean').exists() and (directory/'repo/reference/context.txt').exists()


@pytest.mark.parametrize('mutation', ['index_missing', 'index_changed', 'prepared', 'blob_missing'])
def test_reader_fails_closed_after_retirement(failed, mutation):
    collector, directory, observed, _ = failed
    copies.retire(collector, directory, observed, apply=True)
    path = directory/recovery.INDEX
    if mutation == 'index_missing':path.unlink()
    elif mutation == 'index_changed':path.write_bytes(gzip.compress(b'{}'))
    elif mutation == 'prepared':
        value = json.loads((directory/'prepared.json').read_text()); value['source_head'] = 'f'*40
        put(directory/'prepared.json', value)
    else:
        entry = json.loads(gzip.decompress(path.read_bytes()))['files']['Tablet/A.lean']
        (collector.repo/'.git/objects'/entry['oid'][:2]/entry['oid'][2:]).unlink()
    with pytest.raises((ValueError, FileNotFoundError)):
        recovery.Sources(directory).read('Tablet/A.lean', 1024)


@pytest.mark.parametrize('guard', ['active', 'accepted', 'pending_publication', 'pin', 'live', 'cleanup', 'cancel', 'shallow', 'graft'])
def test_scope_and_existing_guards(failed, monkeypatch, guard):
    collector, directory, observed, _ = failed
    if guard == 'active':observed = {}
    elif guard == 'accepted':observed['1']['status']['outcome'] = dict(kind='accepted', event_id=1)
    elif guard == 'pending_publication':monkeypatch.setattr(collector, '_owner_pending', lambda _: True)
    elif guard == 'pin':put(directory/'retain-builds.json', {})
    elif guard == 'live':monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    elif guard == 'cleanup':(directory/'execution/cleanup.json').unlink()
    elif guard == 'cancel':monkeypatch.setattr(collector, 'cancelled', lambda: True)
    elif guard == 'shallow':(collector.repo/'.git/shallow').write_text('x')
    else:
        path=collector.repo/'.git/info/grafts';path.parent.mkdir(exist_ok=True);path.write_text('x')
    if guard in ('pin', 'live', 'cleanup', 'shallow', 'graft'):
        with pytest.raises((ValueError, FileNotFoundError)):
            copies.retire(collector, directory, observed, apply=True)
    else:assert copies.retire(collector, directory, observed, apply=True)['removed'] == 0
    assert (directory/'repo/Tablet/A.lean').exists()


def test_limits_aliases_and_raw_recreated_file_precedence(failed, tmp_path):
    collector, directory, observed, _ = failed
    copies.retire(collector, directory, observed, apply=True)
    sources = recovery.Sources(directory)
    with pytest.raises(ValueError, match='size limit'):sources.read('Tablet/A.lean', 1)
    assert sources.read('Tablet/A.lean', 1, truncate=True) == (b'C', True)
    with pytest.raises(ValueError):sources.read('../outside', 100)
    target=tmp_path/'alias';target.mkdir();(target/'Tablet').symlink_to(tmp_path, target_is_directory=True)
    with pytest.raises(ValueError, match='alias'):sources.materialize(target)
    (directory/'repo/Tablet/A.lean').write_text('Operator restored modified bytes')
    copied=tmp_path/'copied';copied_sources(directory,copied)
    assert (copied/'Tablet/A.lean').read_text()=='Operator restored modified bytes'
    assert copies.retire(collector,directory,observed,apply=True)['removed']==0


def test_cleanup_default_does_not_enable_new_representation(tmp_path):
    from trellis.dispatch.storage import BackgroundCollector
    collector=BackgroundCollector(tmp_path)
    assert collector.retire_failed_sources is False
    collector.close()
    with pytest.raises(ValueError,match='boolean'):BackgroundCollector(tmp_path,retire_failed_sources=1)


@pytest.mark.parametrize('enabled', [False, True])
def test_real_background_scope_requires_explicit_reader_rollout_opt_in(failed, enabled):
    from trellis.dispatch.storage import BackgroundCollector
    collector,directory,observed,state=failed
    prepared=json.loads((directory/'prepared.json').read_text())
    git(directory/'repo','fetch','-q',str(collector.repo),prepared['source_head'])
    git(directory/'repo','update-ref','HEAD',prepared['source_head'])
    background=BackgroundCollector(collector.root,repo=collector.repo,
        git_maintenance_threshold_bytes=None,retire_failed_sources=enabled)
    try:
        assert background.schedule(state) and background.wait_idle(30)
        assert background.summary['errors']==0
        assert background.summary['recoverable_source_files']==(2 if enabled else 0)
        assert (directory/'repo/Tablet/A.lean').exists() is not enabled
        if enabled:
            assert background.summary['recovery_index_allocated_bytes']>0
            assert recovery.Sources(directory).read('Tablet/A.lean',1024)[0]==b'Canonical theorem\n'
    finally:background.close()


@pytest.mark.parametrize('change', ['pin', 'cancel', 'next_file'])
def test_mid_root_pin_cancel_and_changed_next_file_remain_safe(failed, monkeypatch, change):
    collector,directory,observed,_=failed
    original=Path.unlink; stopped=[]
    def unlink(path,*args,**kwargs):
        result=original(path,*args,**kwargs)
        if path==directory/'repo/Tablet/A.lean':
            if change=='pin':put(directory/'retain-builds.json',{})
            elif change=='cancel':stopped.append(True)
            else:(directory/'repo/reference/context.txt').write_text('New unique bytes')
        return result
    monkeypatch.setattr(Path,'unlink',unlink)
    if change=='cancel':
        monkeypatch.setattr(collector,'cancelled',lambda:bool(stopped))
        assert copies.retire(collector,directory,observed,apply=True)['status']=='cancelled'
    else:
        with pytest.raises(ValueError):copies.retire(collector,directory,observed,apply=True)
    assert (directory/'repo/reference/context.txt').exists()
    assert recovery.Sources(directory).read('Tablet/A.lean',1024)[0]==b'Canonical theorem\n'


def test_manifest_reader_has_decompression_bound_and_index_alias_is_rejected(failed, monkeypatch):
    collector,directory,observed,_=failed
    copies.retire(collector,directory,observed,apply=True)
    index=directory/recovery.INDEX
    saved=index.read_bytes();index.write_bytes(gzip.compress(b' '*1025))
    monkeypatch.setattr(recovery,'MAX_INDEX_BYTES',1024)
    with pytest.raises(ValueError,match='size limit'):recovery.Sources(directory)
    index.write_bytes(saved);renamed=index.with_name('saved.gz');index.rename(renamed);index.symlink_to(renamed.name)
    with pytest.raises(ValueError,match='symlink'):recovery._index(index)


def test_orphan_index_file_limit_applies_on_restart(failed, monkeypatch):
    collector,directory,observed,_=failed
    from trellis.dispatch import auxiliary_retention as engine
    def stop_receipt(*_):raise OSError('before receipt')
    monkeypatch.setattr(engine,'atomic',stop_receipt)
    with pytest.raises(OSError):copies.retire(collector,directory,observed,apply=True)
    monkeypatch.setattr(recovery,'MAX_FILE_BYTES',1)
    with pytest.raises(ValueError,match='metadata is invalid'):
        copies.retire(collector,directory,observed,apply=True)
    assert (directory/'repo/Tablet/A.lean').exists()


def test_partial_restart_requires_receipt_digest_and_pin_for_reader(failed, monkeypatch):
    collector,directory,observed,_=failed
    original=Path.unlink
    def stop(path,*args,**kwargs):
        if path==directory/'repo/reference/context.txt':raise OSError('interruption')
        return original(path,*args,**kwargs)
    monkeypatch.setattr(Path,'unlink',stop)
    with pytest.raises(OSError):copies.retire(collector,directory,observed,apply=True)
    receipt=json.loads((directory/recovery.RECEIPT).read_text());receipt.pop('recovery_sha256')
    put(directory/recovery.RECEIPT,receipt)
    with pytest.raises(ValueError,match='receipt-bound'):
        copies.retire(collector,directory,observed,apply=True)
    assert (directory/'repo/reference/context.txt').exists()


def test_recovery_pin_cannot_be_removed_or_redirected(failed):
    from trellis.dispatch.git_retention import PIN_PREFIX
    collector,directory,observed,_=failed
    copies.retire(collector,directory,observed,apply=True)
    head=json.loads((directory/'prepared.json').read_text())['source_head']
    git(collector.repo,'update-ref','-d',PIN_PREFIX+head)
    with pytest.raises(ValueError):recovery.Sources(directory)


def test_xattrs_are_preserved_by_refusing_unsupported_source_metadata(failed):
    collector,directory,observed,_=failed
    path=directory/'repo/Tablet/A.lean'
    try:os.setxattr(path,'user.recovery-test',b'unique metadata')
    except OSError:pytest.skip('filesystem has no user xattrs')
    with pytest.raises(ValueError,match='unsupported'):
        copies.retire(collector,directory,observed,apply=True)
    assert path.exists() and os.getxattr(path,'user.recovery-test')==b'unique metadata'


def test_nonworker_identity_is_not_interpreted(failed):
    collector,_,_,_=failed
    directory=collector.root/'verifier-2';directory.mkdir()
    assert copies.retire(collector,directory,{'verifier-2':object()},apply=True)['removed']==0

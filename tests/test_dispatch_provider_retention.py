from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys

import pytest

from test_dispatch_terminal_retention import setup, put, git
from test_dispatch_nonworker_retention import owner_setup
from trellis.dispatch import provider_retention as providers
from trellis.dispatch import retained_files as retained
from trellis.dispatch import terminal_retention as terminal


@pytest.fixture
def provider_setup(setup):
    collector, directory, observed, paths, state = setup
    prepared = json.loads((directory / 'prepared.json').read_text())
    prepared['source_head'] = git(collector.repo, 'rev-parse', 'HEAD')
    put(directory / 'prepared.json', prepared)
    return collector, directory, observed, paths, state


def files(directory):
    home = directory / 'runtime/burst-homes/worker/.codex'
    home.mkdir(parents=True)
    data = {'config.toml': b'example_setting = "private-value"\n' * 40000,
            'state_5.sqlite': b'state database\0' * 100000,
            'thread_history_2.sqlite': b'thread database\0' * 100000,
            'state_5.sqlite-wal': b'last small WAL bytes',
            'state_5.sqlite-shm': b'last small SHM bytes'}
    for name, raw in data.items():
        (home / name).write_bytes(raw); (home / name).chmod(0o600)
    for name in ('auth.json', 'sessions/transcript.jsonl', 'skills/custom/SKILL.md', 'goals_1.sqlite', 'config.toml.bak'):
        path = home / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(b'unique evidence')
    return home, data


def test_exact_roundtrip_export_includes_small_companions_and_preserves_other_evidence(provider_setup, tmp_path):
    collector, directory, observed, _, _ = provider_setup
    home, data = files(directory)
    before = {p:p.read_bytes() for p in home.rglob('*') if p.is_file()}
    result = providers.collect(collector, directory, observed)
    assert result['archived'] == 3 and result['removed_bytes'] > 3000000
    assert providers.collect(collector, directory, observed)['archived'] == 0
    assert set(providers.paths(directory)) == {home/name for name in data}
    for path, raw in before.items():
        assert retained.read_bytes(path) == raw
    pin = put(directory / 'retain-builds.json', {'operator': 'keep restored evidence'})
    pin_before = pin.read_bytes()
    target = tmp_path / 'export'
    result = providers.export(collector, directory, observed, target)
    assert result['files'] == len(data) and pin.read_bytes() == pin_before
    for name, raw in data.items():
        exported = target / (home/name).relative_to(directory)
        assert exported.read_bytes() == raw and exported.stat().st_mode & 0o777 == 0o600
    assert not list(target.rglob('auth.json')) and not list(target.rglob('transcript.jsonl'))
    manifest = json.loads((target/'export-manifest.json').read_text())
    assert 'not a complete resumable home' in manifest['scope']
    assert not (home/'config.toml').exists()  # export never restores into live source


@pytest.mark.parametrize('failure', ['active', 'uncommitted', 'pin', 'live', 'cleanup', 'wrong_repo', 'pending', 'prepared_head'])
def test_guards_preserve_plain_provider_inputs(provider_setup, monkeypatch, failure):
    collector, directory, observed, _, _ = provider_setup
    home, data = files(directory)
    if failure == 'active': observed = {}
    elif failure == 'uncommitted': observed = {directory.name: 'changed identity'}
    elif failure == 'pin': put(directory/'retain-builds.json', {})
    elif failure in ('live','cleanup'):
        put(directory/'execution/process.json', {'pid':123})
        if failure == 'live': put(directory/'execution/cleanup.json', {'pid':123})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _:failure=='live')
    elif failure == 'pending': monkeypatch.setattr(collector, '_owner_pending', lambda _:True)
    else:
        p = directory/'prepared.json';v=json.loads(p.read_text())
        v['source_repo' if failure=='wrong_repo' else 'source_head'] = str(directory) if failure=='wrong_repo' else '0'*40
        put(p,v)
    try: result=providers.collect(collector,directory,observed)
    except (ValueError,FileNotFoundError,subprocess.CalledProcessError): pass
    else: assert result['archived']==0
    assert all((home/name).read_bytes()==raw for name,raw in data.items())


@pytest.mark.parametrize('failure', ['pin','checkpoint','pending','tracked','signature','prepared'])
def test_final_fence_rechecks_after_compression(provider_setup, monkeypatch, failure):
    collector,directory,observed,_,_=provider_setup
    home,_=files(directory);path=home/'config.toml';original=retained._archived_chunks
    def chunks(p):
        yield from original(p)
        if failure=='pin':put(directory/'retain-builds.json',{})
        elif failure=='checkpoint':monkeypatch.setattr(collector,'checkpoint_current',lambda _:False)
        elif failure=='pending':monkeypatch.setattr(collector,'_owner_pending',lambda _:True)
        elif failure=='tracked':monkeypatch.setattr(retained,'_untracked',lambda _:False)
        elif failure=='signature':path.write_bytes(path.read_bytes()+b'changed')
        else:
            p=directory/'prepared.json';v=json.loads(p.read_text());v['history']['cycle']+=1;put(p,v)
    monkeypatch.setattr(retained,'_archived_chunks',chunks)
    with pytest.raises(ValueError):providers.collect(collector,directory,observed)
    assert path.exists()


def test_encoding_and_verification_are_outside_coordinator_lock(provider_setup,monkeypatch):
    collector,directory,observed,_,_=provider_setup;files(directory)
    held=False;original_lock=collector.retirement_lock
    @contextmanager
    def lock():
        nonlocal held
        with original_lock()as acquired:
            held=acquired
            try:yield acquired
            finally:held=False
    monkeypatch.setattr(collector,'retirement_lock',lock)
    original=retained._archived_chunks
    def chunks(p):
        assert not held
        yield from original(p)
    monkeypatch.setattr(retained,'_archived_chunks',chunks)
    assert providers.collect(collector,directory,observed)['archived']==3


@pytest.mark.parametrize('kind',['raw','archive','manifest','skip'])
def test_tracked_source_or_output_never_written(provider_setup,monkeypatch,kind):
    collector,directory,observed,_,_=provider_setup;home,_=files(directory)
    path=home/'config.toml';blocked={'raw':path,'archive':retained._paths(path)[0],'manifest':retained._paths(path)[1],'skip':retained._skip_path(path)}[kind]
    original=retained._untracked
    monkeypatch.setattr(retained,'_untracked',lambda p:Path(p)!=blocked and original(p))
    assert providers.collect(collector,directory,observed)['archived']==2
    assert path.exists()and not retained._paths(path)[0].exists()


def test_hardlinks_and_symlinks_are_not_archived(provider_setup,tmp_path):
    collector,directory,observed,_,_=provider_setup;home,_=files(directory)
    os.link(home/'config.toml',tmp_path/'other-config')
    (home/'state_5.sqlite').unlink();(home/'state_5.sqlite').symlink_to(home/'thread_history_2.sqlite')
    assert providers.collect(collector,directory,observed)['archived']==1
    assert (home/'config.toml').exists()and(home/'state_5.sqlite').is_symlink()
    with pytest.raises(ValueError):providers.export(collector,directory,observed,tmp_path/'export')


@pytest.mark.parametrize('changed',['corrupt','missing_manifest','existing_destination','new_family_member','changed_raw'])
def test_failed_export_does_not_publish_or_modify_source(provider_setup,tmp_path,monkeypatch,changed):
    collector,directory,observed,_,_=provider_setup;home,data=files(directory)
    providers.collect(collector,directory,observed);target=tmp_path/'export'
    if changed=='corrupt':retained._paths(home/'state_5.sqlite')[0].write_bytes(b'corrupt')
    elif changed=='missing_manifest':retained._paths(home/'state_5.sqlite')[1].unlink()
    elif changed=='existing_destination':target.mkdir();(target/'keep').write_text('preserve')
    else:
        original=retained.chunks
        def chunks(path):
            yield from original(path)
            if changed=='new_family_member':(home/'state_6.sqlite-wal').write_bytes(b'new')
            else:(home/'state_5.sqlite-wal').write_bytes(b'changed')
        monkeypatch.setattr(retained,'chunks',chunks)
    with pytest.raises((ValueError,OSError)):providers.export(collector,directory,observed,target)
    assert (target/'keep').read_text()=='preserve' if changed=='existing_destination' else not target.exists()
    assert not list(tmp_path.glob('.provider-export-*'))
    assert not(home/'config.toml').exists()


def test_destination_created_at_publish_is_not_overwritten(provider_setup,tmp_path,monkeypatch):
    collector,directory,observed,_,_=provider_setup;files(directory);target=tmp_path/'export'
    original=providers._publish
    def publish(temporary,destination):
        destination.mkdir();(destination/'preserve').write_text('foreign')
        original(temporary,destination)
    monkeypatch.setattr(providers,'_publish',publish)
    with pytest.raises(FileExistsError):providers.export(collector,directory,observed,target)
    assert(target/'preserve').read_text()=='foreign'


def test_nonworker_export_and_retention(owner_setup,tmp_path):
    collector,directory,observed,_,_=owner_setup;home,data=files(directory)
    assert providers.collect(collector,directory,observed)['archived']==3
    if directory.name.startswith('review-'):
        put(collector.root/'reviews/2/retain-state.json',{})
    providers.export(collector,directory,observed,tmp_path/'export')
    assert all((tmp_path/'export'/(home/name).relative_to(directory)).read_bytes()==raw for name,raw in data.items())


def test_actual_export_cli_preserves_private_source(provider_setup,tmp_path):
    collector,directory,observed,_,_=provider_setup;home,data=files(directory)
    providers.collect(collector,directory,observed)
    put(directory/'retain-builds.json',{})
    target=tmp_path/'export'
    result=subprocess.run([sys.executable,'-m','trellis.dispatch.provider_retention','--runtime',str(collector.root.parent),'--repo',str(collector.repo),'--owner',directory.name,'--export',str(target)],capture_output=True,text=True)
    assert result.returncode==0,result.stderr
    assert json.loads(result.stdout)['files']==5
    assert 'private-value'not in result.stdout+result.stderr
    assert not(home/'config.toml').exists()


def test_background_provider_archive_counters(provider_setup):
    from trellis.dispatch.storage import BackgroundCollector
    original,directory,_,_,state=provider_setup;files(directory)
    collector=BackgroundCollector(original.root,repo=original.repo);collector._review_snapshots=None
    assert collector.schedule(state)and collector.wait_idle(30)
    assert collector.summary['provider_archives']==3 and collector.summary['errors']==0


def test_export_rejects_dotdot_destination_inside_runtime(provider_setup):
    collector,directory,observed,_,_=provider_setup;files(directory)
    sibling=collector.root.parent.parent/'outside';sibling.mkdir()
    destination=sibling/'..'/collector.root.parent.name/'exported'
    assert not destination.is_relative_to(collector.root.parent)
    with pytest.raises(ValueError,match='outside the live runtime'):
        providers.export(collector,directory,observed,destination)
    assert not(collector.root.parent/'exported').exists()


def test_export_waits_for_entire_cleanup_lease(provider_setup,tmp_path):
    import fcntl
    import threading
    collector,directory,observed,_,_=provider_setup;files(directory)
    target=tmp_path/'export';started=threading.Event();finished=threading.Event();errors=[]
    def run():
        started.set()
        try:providers.export(collector,directory,observed,target)
        except Exception as error:errors.append(error)
        finally:finished.set()
    with(collector.root/'build-cleanup.lock').open('a+b')as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        worker=threading.Thread(target=run,daemon=True);worker.start()
        assert started.wait(5)
        assert not finished.wait(.05) and not target.exists()
    worker.join(10)
    assert not worker.is_alive() and not errors and target.exists()


def test_busy_archive_and_cancelled_retry_keep_raw(provider_setup,monkeypatch):
    collector,directory,observed,_,_=provider_setup;home,data=files(directory)
    @contextmanager
    def busy():yield False
    monkeypatch.setattr(collector,'retirement_lock',busy)
    assert providers.collect(collector,directory,observed)['status']=='coordinator_busy'
    monkeypatch.setattr(collector,'cancelled',lambda:True)
    assert providers.collect(collector,directory,observed)['status']=='cancelled'
    assert all((home/name).read_bytes()==raw for name,raw in data.items())


def test_real_sqlite_wal_family_exports_committed_rows(provider_setup,tmp_path):
    import shutil
    import sqlite3
    collector,directory,observed,_,_=provider_setup
    home=directory/'runtime/burst-homes/worker/.codex';home.mkdir(parents=True)
    original=tmp_path/'state_9.sqlite';database=sqlite3.connect(original)
    try:
        database.execute('PRAGMA journal_mode=WAL')
        database.execute('CREATE TABLE history(id INTEGER PRIMARY KEY, content TEXT)')
        database.execute('INSERT INTO history(content) VALUES (?)',('retained context '*100000,))
        database.commit();database.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        database.execute('INSERT INTO history(content) VALUES (?)',('last committed WAL row',));database.commit()
        snapshot={}
        for suffix in ('','-wal','-shm'):
            source=Path(str(original)+suffix);snapshot[source.name]=source.read_bytes()
            shutil.copyfile(source,home/source.name)
    finally:database.close()
    assert len(snapshot['state_9.sqlite'])>=retained.MIN_BYTES
    assert 0<len(snapshot['state_9.sqlite-wal'])<retained.MIN_BYTES
    result=providers.collect(collector,directory,observed)
    assert result['archived']==1 and(home/'state_9.sqlite-wal').exists()
    target=tmp_path/'export';providers.export(collector,directory,observed,target)
    exported_home=target/home.relative_to(directory)
    assert all((exported_home/name).read_bytes()==raw for name,raw in snapshot.items())
    with sqlite3.connect(exported_home/'state_9.sqlite')as restored:
        assert restored.execute('SELECT count(*) FROM history').fetchone()==(2,)
        assert restored.execute('SELECT content FROM history WHERE id=2').fetchone()==('last committed WAL row',)

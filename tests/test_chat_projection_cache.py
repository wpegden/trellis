import json
import os
from pathlib import Path
import shutil
import subprocess
import types

import pytest

from trellis import chat_history as chats, chat_projection_cache as cache


def put(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(data)
    return path


def event(path, *requests):
    return put(path, json.dumps({'commands':[{'command':'issue_request','request':{'id':id,'cycle':cycle}} for id,cycle in requests]})+'\n')


@pytest.fixture
def project(tmp_path,monkeypatch):
    repo=tmp_path/'project';root=repo/'.trellis/chats';root.mkdir(parents=True)
    monkeypatch.setattr(chats,'ensure_chat_repo',lambda _:root)
    events=repo/'.trellis-history/event-log'
    event(events/'cycle-000001.jsonl',(1,1),(2,1))
    event(events/'cycle-000002.jsonl',(3,2))
    for id in (1,2,3):
        put(root/'live'/f'trellis_worker_{id}_result'/'prompt.md',f'prompt{id}\n')
        put(root/'live'/f'trellis_worker_{id}_result'/'nested/output.log',f'output{id}\n')
    return repo,root,events


def contents(root):
    return {str(p.relative_to(root)):p.read_bytes() for p in root.rglob('*') if p.is_file() and '.git' not in p.parts}


def test_warm_checkpoint_parses_no_history_and_copies_no_unchanged_artifacts(project,monkeypatch):
    repo,root,_=project
    first=chats.rebuild_cycle_chat_dirs(repo);before=contents(root)
    def fail(*a,**kw):raise AssertionError('unexpected full read or copy')
    monkeypatch.setattr(chats,'_request_cycles_from_event_log',fail)
    monkeypatch.setattr(shutil,'copytree',fail)
    assert chats.rebuild_cycle_chat_dirs(repo)==first
    assert contents(root)==before
    assert (root.parent/'chat-projection-cache.json').exists()
    assert not (root/'chat-projection-cache.json').exists()


@pytest.mark.parametrize('change',['append','replace_same_size_mtime','remove_event','delete_artifact','source_change','destination_change','source_extra','destination_extra','destination_missing'])
def test_incremental_changes_match_exact_prechange_rebuilder(project,monkeypatch,change):
    repo,root,events=project;chats.rebuild_cycle_chat_dirs(repo)
    if change=='append':
        with (events/'cycle-000002.jsonl').open('a') as f:f.write(json.dumps({'commands':[{'command':'issue_request','request':{'id':1,'cycle':2}}]})+'\n')
    elif change=='replace_same_size_mtime':
        p=events/'cycle-000001.jsonl';stamp=p.stat();text=p.read_text().replace('"cycle": 1','"cycle": 3');p.write_text(text);os.utime(p,ns=(stamp.st_atime_ns,stamp.st_mtime_ns))
    elif change=='remove_event':(events/'cycle-000002.jsonl').unlink()
    elif change=='delete_artifact':shutil.rmtree(root/'live/trellis_worker_1_result')
    else:
        base=root/('live' if change.startswith('source') else 'cycle-0001')/'trellis_worker_1_result'
        p=base/'nested/output.log'
        if change.endswith('change'):
            stamp=p.stat();p.write_bytes(b'x'*stamp.st_size);os.utime(p,ns=(stamp.st_atime_ns,stamp.st_mtime_ns))
        elif change.endswith('extra'):put(base/'unique-extra.md','new tree member')
        else:shutil.rmtree(base)
    # Preserve the modified state, then compare both production algorithms.
    oracle=repo.parent/'oracle';shutil.copytree(repo,oracle)
    try:
        source=subprocess.check_output(['git','show','62f223f09716c3ed9c9118a60dd4523609329f29:trellis/chat_history.py'],text=True,stderr=subprocess.DEVNULL)
    except subprocess.CalledProcessError:
        pytest.skip('prechange implementation unavailable in shallow checkout')
    old=types.ModuleType('trellis.chat_history_previous');old.__package__='trellis';exec(compile(source,'previous_chat_history.py','exec'),old.__dict__)
    old.ensure_chat_repo=lambda _:oracle/'.trellis/chats'
    expected=old.rebuild_cycle_chat_dirs(oracle)
    assert chats.rebuild_cycle_chat_dirs(repo)==expected
    assert contents(root)==contents(oracle/'.trellis/chats')


@pytest.mark.parametrize('bad',['json','shape','digest','valid_digest_invalid_request'])
def test_bad_cache_falls_back_to_full_parse(project,monkeypatch,bad):
    repo,root,_=project;chats.rebuild_cycle_chat_dirs(repo);path=root.parent/'chat-projection-cache.json'
    if bad=='json':path.write_text('not JSON')
    elif bad=='shape':path.write_text(json.dumps({'data':[]}))
    elif bad=='digest':
        value=json.loads(path.read_text());value['data']['events'].clear();path.write_text(json.dumps(value))
    else:
        value=cache.load(path);next(iter(value['events'].values()))['requests']={'x':'bad'};cache.save(path,value)
    calls=[];old=chats._request_cycles_from_event_log
    def parse(*a,**kw):calls.append(kw.get('files'));return old(*a,**kw)
    monkeypatch.setattr(chats,'_request_cycles_from_event_log',parse)
    assert chats.rebuild_cycle_chat_dirs(repo)=={1:['trellis_worker_1_result','trellis_worker_2_result'],2:['trellis_worker_3_result']}
    assert calls


def test_event_changed_during_parse_is_not_cached(project,monkeypatch):
    repo,root,events=project;old=chats._request_cycles_from_event_log;changed=[False]
    def parse(*a,**kw):
        result=old(*a,**kw)
        if not changed[0]:
            changed[0]=True
            with kw['files'][0].open('a') as f:f.write(json.dumps({'commands':[{'command':'issue_request','request':{'id':1,'cycle':2}}]})+'\n')
        return result
    monkeypatch.setattr(chats,'_request_cycles_from_event_log',parse)
    chats.rebuild_cycle_chat_dirs(repo)
    assert 'cycle-000001.jsonl' not in cache.load(root.parent/'chat-projection-cache.json')['events']
    assert chats.rebuild_cycle_chat_dirs(repo)[2]==['trellis_worker_1_result','trellis_worker_3_result']


def test_copy_race_does_not_mark_incomplete_artifact_reusable(project,monkeypatch):
    repo,root,_=project;old=shutil.copytree;changed=[False]
    def copy(source,destination,*a,**kw):
        result=old(source,destination,*a,**kw)
        if Path(source).name=='trellis_worker_1_result' and not changed[0]:
            changed[0]=True;put(Path(source)/'later.txt','added during copy')
        return result
    monkeypatch.setattr(shutil,'copytree',copy)
    chats.rebuild_cycle_chat_dirs(repo)
    assert 'cycle-0001/trellis_worker_1_result' not in cache.load(root.parent/'chat-projection-cache.json')['artifacts']
    chats.rebuild_cycle_chat_dirs(repo)
    assert (root/'cycle-0001/trellis_worker_1_result/later.txt').read_text()=='added during copy'


def test_cache_write_failure_does_not_fail_checkpoint(project,monkeypatch):
    repo,root,_=project
    monkeypatch.setattr(cache.tempfile,'mkstemp',lambda **kw:(_ for _ in ()).throw(OSError('cache unavailable')))
    assert chats.rebuild_cycle_chat_dirs(repo)
    assert (root/'cycle-0001/trellis_worker_1_result/prompt.md').exists()


def test_transient_event_read_failure_is_not_cached(project,monkeypatch):
    repo,root,events=project;original=Path.open;selected=events/'cycle-000001.jsonl'
    def read(path,*a,**kw):
        if path==selected:raise OSError('temporary read error')
        return original(path,*a,**kw)
    monkeypatch.setattr(Path,'open',read)
    chats.rebuild_cycle_chat_dirs(repo)
    assert selected.name not in cache.load(root.parent/'chat-projection-cache.json')['events']
    monkeypatch.setattr(Path,'open',original)
    assert 1 in chats.rebuild_cycle_chat_dirs(repo)


def test_only_changed_cycle_and_artifact_are_rebuilt(project,monkeypatch):
    repo,root,events=project;chats.rebuild_cycle_chat_dirs(repo)
    with (events/'cycle-000002.jsonl').open('a') as stream:stream.write('\n')
    put(root/'live/trellis_worker_3_result/prompt.md','new prompt')
    parsed=[];copied=[];parse=chats._request_cycles_from_event_log;copy=shutil.copytree
    def track_parse(*a,**kw):parsed.extend(kw['files']);return parse(*a,**kw)
    def track_copy(src,dst,*a,**kw):
        if Path(src).parent==root/'live':copied.append(Path(src).name)
        return copy(src,dst,*a,**kw)
    monkeypatch.setattr(chats,'_request_cycles_from_event_log',track_parse)
    monkeypatch.setattr(shutil,'copytree',track_copy)
    chats.rebuild_cycle_chat_dirs(repo)
    assert parsed==[events/'cycle-000002.jsonl']
    assert copied==['trellis_worker_3_result']


def test_real_chat_git_rewind_invalidates_cache_without_tracking_it(tmp_path):
    repo=tmp_path/'project';repo.mkdir();root=chats.ensure_chat_repo(repo)
    event(repo/'.trellis-history/event-log/cycle-000001.jsonl',(1,1))
    source=put(root/'live/trellis_worker_1_result/prompt.md','first')
    chats.rebuild_cycle_chat_dirs(repo);chats.commit_chat_checkpoint(repo,tag='cycle-1')
    source.write_text('second')
    chats.rebuild_cycle_chat_dirs(repo);chats.commit_chat_checkpoint(repo,tag='cycle-2')
    chats.rewind_chat_history(repo,tag='cycle-1')
    assert (root.parent/'chat-projection-cache.json').exists()
    source.write_text('third')
    chats.rebuild_cycle_chat_dirs(repo)
    assert (root/'cycle-0001/trellis_worker_1_result/prompt.md').read_text()=='third'
    assert 'chat-projection-cache' not in subprocess.check_output(['git','-C',str(root),'ls-files'],text=True)


def test_live_partial_tail_is_skipped_then_reparsed_after_completion(project):
    repo,root,events=project
    path=events/'cycle-000002.jsonl'
    record=json.dumps({'commands':[{'command':'issue_request','request':{'id':1,'cycle':2}}]})
    with path.open('a') as stream:stream.write('garbled\n'+record[:len(record)//2])
    assert chats.rebuild_cycle_chat_dirs(repo)[1]==['trellis_worker_1_result','trellis_worker_2_result']
    with path.open('a') as stream:stream.write(record[len(record)//2:]+'\n')
    assert chats.rebuild_cycle_chat_dirs(repo)=={2:['trellis_worker_1_result','trellis_worker_3_result'],1:['trellis_worker_2_result']}

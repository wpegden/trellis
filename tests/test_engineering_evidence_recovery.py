"""Retained engineering evidence survives interrupted state and receipt writes."""
import hashlib
import json
import os
from pathlib import Path

import pytest

from trellis import git_trust as gt
from trellis.agent_wrapper.protocol import EngineeringExecutionFailure
from trellis.modeling import driver, engineering as eng, engineering_transport as transport
from trellis.modeling.observations import read_json, write_json
from test_paper_engineering import root


def retained(root, *, consumed=True):
    attempt = root / eng.STORE / 'deliveries' / ('1-' + 'a'*32)
    home = attempt/'burst-homes/engineering-fixture'
    admissions = home.parent/'engineering-admissions'
    admissions.mkdir(parents=True)
    request = {'kind':'engineering', 'burst_role':'engineering', 'request_id':'engineering-1-plan',
               'cycle':1, 'work_dir':str(attempt/'view'), 'burst_home':str(home)}
    write_json(attempt/'request.json', request)
    request_sha = hashlib.sha256((attempt/'request.json').read_bytes()).hexdigest()
    binding = {'schema':'engineering-launch-v1', 'request':{'request':request,'sequence':1,'purpose':'plan'},
               'home':str(home),'work':request['work_dir'], 'argv':['/bin/true'],
               'files':{str(attempt/'request.json'):{'sha256':request_sha}}}
    path = admissions / ('b'*32 + '.json'); path.write_text(json.dumps(binding))
    identity = hashlib.sha256(path.read_bytes()).hexdigest()
    recording = path.with_suffix('.execution'); recording.mkdir()
    for filename, code, meta in [('ready.json','recording_ready',{}),('started.json','spawn_pending',{}),
                                  ('launch.json','launcher_exited',{'exit_code':0})]:
        gt._write_result(recording/filename, gt.engineering_receipt(code,identity,**meta),first=True)
    (recording/'claimed').mkdir()
    state = read_json(root/'.trellis/modeling/state.json'); state['sequence']=1
    write_json(attempt/'basis.json', {'fixture': True})
    if consumed:
        row={'sequence':1,'stage':'engineering','task':{'role':'planner','purpose':'plan'},
             'response':{'summary':'handled'},'feedback':''}
        row['engineering_delivery']={'attempt':attempt.relative_to(root).as_posix(),
            'request_sha256':request_sha,
            'basis_sha256':hashlib.sha256((attempt/'basis.json').read_bytes()).hexdigest(),
            'import_sha256':None,'outcome_sha256':eng.digest(row)}
        state['history'].append(row)
    write_json(root/'.trellis/modeling/state.json',state)
    return attempt, path, recording, identity


@pytest.mark.parametrize('change', ['missing_terminal','missing_started','missing_ready','missing_claim',
    'partial_terminal','stale_ready','stale_terminal','failed_terminal','terminal_alias','parent_alias',
    'claim_alias','unreadable_terminal','unreadable_directory','wrong_request','wrong_sequence',
    'wrong_request_hash','failure','partial_failure','orphan_recording','interrupted_binding'])
def test_unresolved_attempt_blocks_reconstruction_before_sequence(root, monkeypatch, change):
    attempt,path,recording,identity=retained(root)
    if change.startswith('missing_'):
        name={'terminal':'launch.json','started':'started.json','ready':'ready.json','claim':'claimed'}[change[8:]]
        target=recording/name
        target.rmdir() if target.is_dir() else target.unlink()
    elif change=='partial_terminal': (recording/'launch.json').write_text('{')
    elif change in {'stale_ready','stale_terminal'}:
        target=recording/('ready.json' if change=='stale_ready' else 'launch.json')
        data=read_json(target);data['identity']='c'*64;write_json(target,data)
    elif change=='failed_terminal':
        data=read_json(recording/'launch.json');data['exit_code']=1;write_json(recording/'launch.json',data)
    elif change in {'terminal_alias','parent_alias','claim_alias'}:
        target=recording/'launch.json' if change=='terminal_alias' else recording if change=='parent_alias' else recording/'claimed'
        saved=target.with_name(target.name+'-preserved');target.rename(saved);target.symlink_to(saved)
    elif change=='unreadable_terminal': (recording/'launch.json').chmod(0)
    elif change=='unreadable_directory': recording.chmod(0)
    elif change in {'wrong_request','wrong_sequence','wrong_request_hash'}:
        value=read_json(path)
        if change=='wrong_request': value['request']['request']['work_dir']='/different'
        elif change=='wrong_sequence': value['request']['sequence']=2
        else: value['files'][str(attempt/'request.json')]['sha256']='d'*64
        path.write_text(json.dumps(value))
    elif change=='failure':
        request=read_json(attempt/'request.json')
        sha=hashlib.sha256(json.dumps(request,sort_keys=True).encode()).hexdigest()
        write_json(attempt/'execution-failure.json',gt.engineering_receipt('execution_unknown',sha,sequence=1))
    elif change=='partial_failure': (attempt/'execution-failure.json').write_text('{')
    elif change=='orphan_recording': path.unlink()
    else: (path.parent/'.trellis-interrupted').write_text('{')
    before=(root/'.trellis/modeling/state.json').read_bytes()
    calls=[]
    monkeypatch.setattr(driver.Driver,'save',lambda *_: (_ for _ in ()).throw(OSError('synthetic state write failure')))
    monkeypatch.setattr(driver,'record_host_failure',lambda *a,**kw: None)
    try:
        for role in ['planner','performance']:
            instance=driver.Driver(root,agents=lambda *a: calls.append(a))
            instance.state['next']={'role':role}
            with pytest.raises(EngineeringExecutionFailure):
                instance.step()
            assert instance.state['sequence']==1 and not calls
        assert (root/'.trellis/modeling/state.json').read_bytes()==before
    finally:
        if change=='unreadable_directory': recording.chmod(0o700)
        if change=='unreadable_terminal': (recording/'launch.json').chmod(0o600)


def test_successful_execution_is_not_import_authority(root):
    attempt,path,recording,identity=retained(root)
    driver.check_halt(root)
    assert not (root/'engineering/plan.json').exists()
    state=read_json(root/'.trellis/modeling/state.json')
    with pytest.raises(driver.AgentResponseError,match='stale'):
        transport.import_result(root,state,{'purpose':'plan'},attempt,{}, {}, {'summary':'unsupported'})
    assert not (root/'engineering/plan.json').exists()
    assert (recording/'launch.json').read_bytes()


def test_preflight_and_role_receipts_remain_bound_to_their_context(root):
    attempt,path,recording,identity=retained(root)
    binding=read_json(path)
    binding['request']={'request':['/usr/bin/python3','--version'],'purpose':'provider_preflight','sequence':None}
    preflight=path.with_name('c'*32+'.json');preflight.write_text(json.dumps(binding))
    digest=hashlib.sha256(preflight.read_bytes()).hexdigest()
    receipts=preflight.with_suffix('.execution');receipts.mkdir()
    for name,code,extra in [('ready.json','recording_ready',{}),('started.json','spawn_pending',{}),
                           ('launch.json','launcher_exited',{'exit_code':0})]:
        gt._write_result(receipts/name,gt.engineering_receipt(code,digest,**extra),first=True)
    (receipts/'claimed').mkdir()
    driver.check_halt(root)
    binding['home']='/unrelated';preflight.write_text(json.dumps(binding))
    with pytest.raises(EngineeringExecutionFailure):driver.check_halt(root)


def test_halt_marker_publication_failure_keeps_disk_guard(root,monkeypatch):
    attempt,path,recording,identity=retained(root)
    (recording/'launch.json').unlink()
    monkeypatch.setattr(driver,'record_host_failure',lambda *a,**kw: (_ for _ in ()).throw(OSError('synthetic marker failure')))
    instance=driver.Driver(root,agents=lambda *_:pytest.fail('dispatch'))
    with pytest.raises(EngineeringExecutionFailure): instance.step()
    assert read_json(root/'.trellis/modeling/state.json')['status']=='halted'
    # A stale active state, independently of sticky marker publication, is insufficient.
    state=read_json(root/'.trellis/modeling/state.json');state['status']='active'
    write_json(root/'.trellis/modeling/state.json',state)
    with pytest.raises(EngineeringExecutionFailure): driver.check_halt(root)


@pytest.mark.parametrize('filename', ['ready.json','launch.json'])
@pytest.mark.parametrize('change', ['array','empty','extra','boolean_uid','string_exit','wrong_identity',
    'wrong_code','duplicate','oversize','symlink','ancestor_symlink','fifo'])
def test_receipt_validation_refuses_malformed_or_aliased_data(tmp_path,filename,change):
    binding=tmp_path/'binding.json'
    command=['/usr/bin/python3','-I','-S','/synthetic/engineering_exec.py','--binding',str(binding),
             '--binding-sha256','a'*64,'--','/bin/true']
    directory=gt.report_directory(command);directory.mkdir()
    path=directory/filename
    data=gt.engineering_receipt('recording_ready' if filename=='ready.json' else 'launcher_exited',
                                'a'*64,**({} if filename=='ready.json' else {'exit_code':0}))
    if change=='array': data=[]
    elif change=='empty': data={}
    elif change=='extra': data['payload']='synthetic forbidden field'
    elif change=='boolean_uid': data['uid']=True
    elif change=='string_exit': data['exit_code']='0'
    elif change=='wrong_identity': data['identity']='b'*64
    elif change=='wrong_code': data['code']='spawn_refused'  # exit zero can never mean success here.
    write_json(path,data)
    if change=='duplicate': path.write_text('{"code":"recording_ready","code":"launcher_exited"}')
    elif change=='oversize': path.write_text(' '*65537)
    elif change in {'symlink','ancestor_symlink'}:
        target=path if change=='symlink' else directory
        saved=target.with_name(target.name+'-preserved');target.rename(saved);target.symlink_to(saved)
    elif change=='fifo': path.unlink();os.mkfifo(path)
    if filename=='ready.json':
        marker=tmp_path/'payload-entered'
        assert gt._supervise_engineering(directory,'a'*64,['/usr/bin/touch',str(marker)])==125
        assert not marker.exists() and not (directory/'claimed').exists()
    else:
        result=gt.engineering_launch_status(command)
        assert result is None or result[0]!=0


def test_scan_is_bounded_and_read_only(tmp_path):
    root=tmp_path/'ordinary';root.mkdir()
    transport.check_unresolved_attempts(root)
    assert list(root.iterdir())==[]
    store=root/eng.STORE/'deliveries';store.mkdir(parents=True)
    for i in range(4097): (store/(str(i)+'-'+'a'*32)).mkdir()
    with pytest.raises(EngineeringExecutionFailure): transport.check_unresolved_attempts(root)

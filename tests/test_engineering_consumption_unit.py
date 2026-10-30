"""Controller publication boundaries with explicit simulated child receipts.

These tests isolate history ownership; the real launcher is covered separately.
"""
import hashlib
import json
from pathlib import Path
from types import SimpleNamespace
import uuid

import pytest

from trellis import git_trust as gt
from trellis.modeling import driver, engineering as eng, engineering_transport as transport
from trellis.modeling.observations import read_json, write_json
from trellis.agent_wrapper.protocol import EngineeringExecutionFailure
from test_paper_engineering import root


def register(root,state,task):
    owner=transport._dispatch_owner(root,state,task);owner.dispatched=True
    attempt=root/eng.STORE/'deliveries'/(str(state['sequence'])+'-'+uuid.uuid4().hex)
    owner.reserved=attempt
    view=attempt/'view';view.mkdir(parents=True)
    owner.bind_view(view,{})
    home=attempt/'burst-homes/fixture';ads=home.parent/'engineering-admissions';ads.mkdir(parents=True)
    purpose=task.get('purpose','performance')
    raw={'kind':'engineering','burst_role':'engineering','request_id':f"engineering-{state['sequence']}-{purpose}",
         'cycle':state['sequence'],'work_dir':str(attempt/'view'),'burst_home':str(home)}
    write_json(attempt/'request.json',raw);basis=transport.basis(root,state);write_json(attempt/'basis.json',basis)
    request=SimpleNamespace(to_dict=lambda:raw,work_dir=attempt/'view',cycle=state['sequence'],log_dir=None)
    owner.register(attempt,request)
    binding={'schema':'engineering-launch-v1','request':{'request':raw,'sequence':state['sequence'],'purpose':purpose},
        'home':str(home),'work':raw['work_dir'],'files':{str(attempt/'request.json'):{'sha256':hashlib.sha256((attempt/'request.json').read_bytes()).hexdigest()}}}
    path=ads/('b'*32+'.json');path.write_text(json.dumps(binding));identity=hashlib.sha256(path.read_bytes()).hexdigest()
    rec=path.with_suffix('.execution');rec.mkdir();(rec/'claimed').mkdir()
    for name,code,kw in [('ready.json','recording_ready',{}),('started.json','spawn_pending',{}),('launch.json','launcher_exited',{'exit_code':0})]:
        gt._write_result(rec/name,gt.engineering_receipt(code,identity,**kw),first=True)
    return attempt,basis


@pytest.mark.parametrize('phase',['before','after','after_unreadable'])
@pytest.mark.parametrize('error',[KeyboardInterrupt,OSError])
def test_atomic_commit_boundary_preserves_consumption_and_original_error(root,monkeypatch,phase,error):
    instance=driver.Driver(root);state=instance.state;task=state['next']
    original_write=transport.write_json;original_read=transport._read
    def write(path,data):
        if path.name=='state.json' and data['history'] and 'engineering_delivery' in data['history'][-1]:
            if phase=='before':raise error('synthetic interruption')
            original_write(path,data)
            if phase=='after_unreadable':
                def unavailable(path):
                    if path.name=='state.json':raise OSError('synthetic unreadable state')
                    return original_read(path)
                monkeypatch.setattr(transport,'_read',unavailable)
            raise error('synthetic interruption')
        original_write(path,data)
    monkeypatch.setattr(transport,'write_json',write)
    with pytest.raises(error,match='synthetic interruption'):
        with transport.request_owner(root,state,instance._halt_engineering):
            state['sequence']+=1;instance.save();attempt,basis=register(root,state,task)
            state['history'].append({'sequence':state['sequence'],'stage':'engineering','task':task,'response':{},'feedback':'revision'})
            transport.publish_outcome(root,state)
    saved=read_json(root/'.trellis/modeling/state.json')
    tagged=bool(saved['history'] and 'engineering_delivery' in saved['history'][-1])
    assert tagged==(phase!='before')
    assert saved['status']==('active' if phase=='after_unreadable' else 'halted')
    assert transport._OWNER.get() is None
    with pytest.raises((RuntimeError,EngineeringExecutionFailure)):driver.check_halt(root)


@pytest.mark.parametrize('purpose',['source_reading','performance'])
@pytest.mark.parametrize('imported',[False,True])
def test_handled_outcome_and_readiness_checkpoint_use_exact_tag(root,monkeypatch,purpose,imported):
    instance=driver.Driver(root);state=instance.state
    task={'role':'performance'} if purpose=='performance' else state['next']
    state['next']=task;instance.save()
    with transport.request_owner(root,state,instance._halt_engineering):
        state['sequence']+=1;instance.save();attempt,basis=register(root,state,task)
        if imported:write_json(attempt/'imported.json',{'basis':basis,'files':{},'mappings':[]})
        state['history'].append({'sequence':state['sequence'],'stage':'engineering','task':task,'response':{},'feedback':'ordinary handled revision'})
        if purpose=='performance':state['status']='engineering_ready'
        transport.publish_outcome(root,state)
        assert state==read_json(root/'.trellis/modeling/state.json')
        transport.publish_outcome(root,state)
    tag=state['history'][-1]['engineering_delivery']
    assert (tag['import_sha256'] is not None)==imported
    transport.check_unresolved_attempts(root)
    assert len(state['history'])==1


def test_combined_receipt_state_marker_failure_holds_on_request(root,monkeypatch):
    from trellis.agent_wrapper import executor
    instance=driver.Driver(root);state=instance.state;task=state['next']
    original_save=instance.save
    def save():
        if state['status']=='halted':raise OSError('state unavailable')
        original_save()
    monkeypatch.setattr(instance,'save',save)
    monkeypatch.setattr(driver,'record_host_failure',lambda *a,**k:(_ for _ in ()).throw(OSError('marker unavailable')))
    monkeypatch.setattr(executor,'engineering_execution_failure',lambda *a:(_ for _ in ()).throw(OSError('receipt unavailable')))
    with pytest.raises(KeyboardInterrupt):
        with transport.request_owner(root,state,instance._halt_engineering):
            state['sequence']+=1;instance.save();attempt,_=register(root,state,task)
            raise KeyboardInterrupt('synthetic cancel after child success')
    saved=read_json(root/'.trellis/modeling/state.json');assert saved['status']=='active'
    assert not (attempt/'execution-failure.json').exists()
    with pytest.raises(EngineeringExecutionFailure):transport.check_unresolved_attempts(root)


def test_retained_request_cannot_be_registered(root):
    from test_engineering_evidence_recovery import retained
    instance=driver.Driver(root);state=instance.state
    attempt,*_=retained(root,consumed=False)
    raw=read_json(attempt/'request.json')
    request=SimpleNamespace(to_dict=lambda:raw,cycle=1,work_dir=attempt/'view')
    with transport.request_owner(root,state,lambda *a,**k:None) as owner:
        owner.dispatched=True
        with pytest.raises(ValueError,match='ownership'):owner.register(attempt,request)


def test_repeated_cancel_during_halt_preserves_original_and_restores_handlers(root):
    import signal,os
    state=read_json(root/'.trellis/modeling/state.json');calls=[]
    prior={s:signal.getsignal(s) for s in (signal.SIGINT,signal.SIGTERM)}
    def halt(*a,**kw):
        calls.append(1)
        os.kill(os.getpid(),signal.SIGTERM)
        os.kill(os.getpid(),signal.SIGINT)
    with pytest.raises(KeyboardInterrupt,match='original'):
        with transport.request_owner(root,state,halt):raise KeyboardInterrupt('original')
    assert calls==[1]
    assert all(signal.getsignal(s)==v for s,v in prior.items())
    assert transport._OWNER.get() is None


def test_cancel_at_handler_restoration_halts_once(root,monkeypatch):
    import signal,os
    state=read_json(root/'.trellis/modeling/state.json');calls=[]
    prior={s:signal.getsignal(s) for s in (signal.SIGINT,signal.SIGTERM)}
    original=signal.signal
    def restore(sig,handler):
        if sig==signal.SIGINT and handler==prior[sig]:os.kill(os.getpid(),signal.SIGTERM)
        return original(sig,handler)
    monkeypatch.setattr(signal,'signal',restore)
    with pytest.raises(KeyboardInterrupt,match='cancelled'):
        with transport.request_owner(root,state,lambda *a,**kw:calls.append(1)):pass
    assert calls==[1] and transport._OWNER.get() is None
    assert all(signal.getsignal(s)==v for s,v in prior.items())


def test_no_dispatch_refresh_needs_no_delivery_roots(root):
    instance=driver.Driver(root)
    with transport.request_owner(root,instance.state,instance._halt_engineering):
        instance.state['sequence']+=1
        transport.publish_outcome(root,instance.state)
    assert read_json(root/'.trellis/modeling/state.json')==instance.state
    assert not (root/eng.STORE/'deliveries').exists()


@pytest.mark.parametrize('registered',[False,True])
def test_dispatched_missing_registration_or_pins_cannot_consume(root,registered):
    instance=driver.Driver(root);state=instance.state
    with pytest.raises(RuntimeError,match='boundary invalid'):
        with transport.request_owner(root,state,instance._halt_engineering) as owner:
            state['sequence']+=1;instance.save();owner.dispatched=True
            if registered:
                owner.dispatched=False
                register(root,state,state['next']);owner.roots.clear()
            transport.publish_outcome(root,state)
    assert not read_json(root/'.trellis/modeling/state.json')['history']


def test_postcommit_error_preserves_history_and_first_terminal(root):
    instance=driver.Driver(root);state=instance.state
    with pytest.raises(RuntimeError,match='synthetic postcommit'):
        with transport.request_owner(root,state,instance._halt_engineering):
            state['sequence']+=1;instance.save();attempt,_=register(root,state,state['next'])
            terminal=next(attempt.glob('burst-homes/engineering-admissions/*.execution/launch.json'))
            first=terminal.read_bytes()
            state['history'].append({'sequence':state['sequence'],'stage':'engineering','task':state['next'],'response':{},'feedback':''})
            transport.publish_outcome(root,state);tagged=state['history'][-1].copy()
            raise RuntimeError('synthetic postcommit')
    saved=read_json(root/'.trellis/modeling/state.json')
    assert saved['status']=='halted' and saved['history'][-1]==tagged
    assert terminal.read_bytes()==first
    driver.Driver(root,agents=lambda *a:pytest.fail('redispatch')).step()

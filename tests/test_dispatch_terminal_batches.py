import copy
from contextlib import contextmanager
import hashlib
import json
import zipfile

import pytest
from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch import terminal_batches as b, terminal_retention as terminal, batch_storage
from trellis.dispatch.native import NativeLedger
from trellis.dispatch.candidate import canonical_json

@pytest.fixture
def batch(setup):
 c,d,observed,paths,state=setup
 receipt=dict(review_id=2,response_sha256=hashlib.sha256(canonical_json({'assignments':[]})).hexdigest(),decisions=[dict(assignment_id='worker-one',task_id=1,rejection=None)])
 state['dispatch']['receipts']={'2':receipt};state['dispatch']['reviewer']=None
 put(c.repo/terminal.HISTORY,dict(state=state));git(c.repo,'add','.');git(c.repo,'commit','-qm','Commit batch')
 put(c.root.parent/'protocol_state.json',state)
 put(c.root/'review-2/prepared.json',dict(source_repo=str(c.repo),source_head=git(c.repo,'rev-parse','HEAD'),request_id=2))
 path=put(c.root/'reviews/2/batch.json',dict(base_sha256='b'*64,prepared=dict(event={'kind':'batch_prepared','review_id':2,'response_sha256':receipt['response_sha256'],'now_ms':1,'graph':['obsolete']*2000,'items':[]},receipt=receipt,workers={'1':{'assignment':{'old':'inputs'},'private_state':{'old':'state'}}})))
 return c,state,path

def current(state):return lambda:b.observation(state,2)
def stored(batch):
 c,state,path=batch
 value=json.loads(path.read_text());raw=json.dumps({'assignment':{},'private_state':{'large':['data']*4000}}).encode();sha=hashlib.sha256(raw).hexdigest()
 ref=dict(format=batch_storage.WORKER_FORMAT,sha256=sha,bytes=len(raw))
 value['prepared'].update(format=batch_storage.FORMAT,workers={'1':ref});value['build_cache_mode']=1;put(path,value)
 obj=path.parent/'workers'/f'{sha}.zip';obj.parent.mkdir()
 with zipfile.ZipFile(obj,'w',compression=zipfile.ZIP_DEFLATED) as z:z.writestr('worker.json',raw)
 return obj

def test_inline_compaction_and_native_idempotent_ack_no_materialization(batch):
 c,state,path=batch
 old=path.stat().st_size
 r=b.retire(c,2,current(state),apply=True)
 assert r['compacted']==1 and r['removed_bytes']==old and path.stat().st_size<old
 saved=json.loads(path.read_text());assert saved['format']==b.FORMAT
 calls=[];ledger=NativeLedger(c.root.parent,kernel=lambda _:calls.append('kernel'),materialize=lambda *a,**kw:calls.append('materialize'))
 ledger._require_recovered_publication=lambda:None
 answer=ledger._dispatch_batch(2,{'assignments':[]},[],{},state=state)
 assert answer==dict(format=b.FORMAT,terminal=True,receipt=state['dispatch']['receipts']['2'],task_ids=['1']) and not calls
 assert b.retire(c,2,current(state),apply=True)['status']=='unchanged'

def test_stored_objects_deleted_only_after_durable_ack_and_other_files_kept(batch,monkeypatch):
 c,state,path=batch;obj=stored(batch);unique=put(obj.parent/'operator.json',{'unique':True})
 real=type(obj).unlink
 def unlink(p,*a,**kw):
  if p==obj:assert json.loads(path.read_text())['format']==b.FORMAT
  return real(p,*a,**kw)
 monkeypatch.setattr(type(obj),'unlink',unlink)
 r=b.retire(c,2,current(state),apply=True)
 assert r['compacted']==1 and r['removed_objects']==1 and not obj.exists() and unique.exists()
 assert b.retire(c,2,current(state),apply=True)['removed_objects']==0

@pytest.mark.parametrize('change',['active','uncommitted','review_active','wrong_review','wrong_assignment','pin','review_pin','live','pending','alias','bad_zip'])
def test_uncertain_batches_preserved(batch,monkeypatch,change):
 c,state,path=batch;obj=stored(batch);before=path.read_bytes()
 if change=='active':state['dispatch']['tasks']['1']['status']['status']='running'
 elif change=='uncommitted':state['dispatch']['receipts']['2']['decisions'][0]['assignment_id']='changed'
 elif change=='review_active':state['dispatch']['reviewer']={'request_id':2}
 elif change=='wrong_review':state['dispatch']['tasks']['1']['review_id']=3
 elif change=='wrong_assignment':state['dispatch']['tasks']['1']['assignment']['assignment_id']='wrong'
 elif change=='pin':put(c.root/'1/retain-builds.json',{})
 elif change=='review_pin':put(path.parent/'retain-state.json',{})
 elif change=='live':
  put(c.root/'1/execution/process.json',{'pid':1});put(c.root/'1/execution/cleanup.json',{'pid':1});monkeypatch.setattr('trellis.dispatch.process.alive',lambda _:True)
 elif change=='pending':
  put(c.root/'publication/pending.json',{'identity':'a'*64});put(c.root/'publication'/('a'*64)/'record.json',{'task_id':1})
 elif change=='alias':obj.rename(obj.with_suffix('.saved'));obj.symlink_to(obj.with_suffix('.saved'))
 elif change=='bad_zip':obj.write_bytes(b'unique invalid bytes')
 try:r=b.retire(c,2,current(state),apply=True);assert r['compacted']==0
 except (ValueError,zipfile.BadZipFile):pass
 assert path.read_bytes()==before and obj.exists()

@pytest.mark.parametrize('change',['current','checkpoint','gate','prepared','cancel'])
def test_changes_while_waiting_for_lease_preserve_original(batch,monkeypatch,change):
 c,state,path=batch;before=path.read_bytes();gate_ok=True
 def gate():
  if not gate_ok:raise ValueError('reader incompatible')
 @contextmanager
 def lock():
  nonlocal gate_ok
  if change=='current':state['dispatch']['tasks']['1']['status']['status']='checking'
  elif change=='checkpoint':monkeypatch.setattr(c,'checkpoint_current',lambda _:False)
  elif change=='gate':gate_ok=False
  elif change=='prepared':put(c.root/'review-2/prepared.json',{'changed':True})
  else:monkeypatch.setattr(c,'cancelled',lambda:True)
  yield True
 monkeypatch.setattr(c,'retirement_lock',lock)
 with pytest.raises((ValueError,InterruptedError)):b.retire(c,2,current(state),apply=True,runtime_admission=gate)
 assert path.read_bytes()==before

def test_zero_accepted_workers_preserves_rejections_and_acks(batch):
 c,state,path=batch;receipt=state['dispatch']['receipts']['2'];receipt['decisions']=[dict(assignment_id='bad',task_id=None,rejection='illegal')]
 put(c.repo/terminal.HISTORY,dict(state=state));git(c.repo,'add','.');git(c.repo,'commit','-qm','Rejected batch')
 saved=json.loads(path.read_text());saved['prepared'].update(receipt=receipt,workers={});put(path,saved)
 assert b.retire(c,2,current(state),apply=True)['compacted']==1
 record=json.loads(path.read_text());assert b.acknowledge(record,state,2,receipt['response_sha256'])['task_ids']==[]
 assert record['receipt']['decisions'][0]['rejection']=='illegal'

def test_partial_object_unlink_restart_and_gate_rechecks(batch,monkeypatch):
 c,state,path=batch;obj=stored(batch);real=c.retirement_lock;calls=0
 @contextmanager
 def lock():
  nonlocal calls
  calls+=1
  with real() as acquired:yield acquired if calls==1 else False
 monkeypatch.setattr(c,'retirement_lock',lock)
 r=b.retire(c,2,current(state),apply=True);assert r['compacted']==1 and obj.exists()
 monkeypatch.setattr(c,'retirement_lock',real)
 assert b.retire(c,2,current(state),apply=True)['removed_objects']==1

def test_current_identity_missing_or_changed_cannot_ack(batch):
 c,state,path=batch;b.retire(c,2,current(state),apply=True);record=json.loads(path.read_text())
 del state['dispatch']['tasks']['1']
 with pytest.raises(RuntimeError,match='current'):b.acknowledge(record,state,2,record['receipt']['response_sha256'])

@pytest.mark.parametrize('enabled',[False,True])
def test_actual_background_hook_defaults_off_and_counts(batch,monkeypatch,enabled):
 from trellis.dispatch.storage import BackgroundCollector
 c,state,path=batch;obj=stored(batch);before=path.read_bytes();admissions=[]
 background=BackgroundCollector(c.root,repo=c.repo,compact_terminal_batches=enabled,
     terminal_batch_admission=lambda:admissions.append('checked'),git_maintenance_threshold_bytes=None)
 monkeypatch.setattr(background,'_collect_owner',lambda _:None)
 try:
  assert background.schedule(state) and background.wait_idle(20)
  assert background.summary['errors']==0
  assert background.summary['terminal_batch_compactions']==int(enabled)
  assert background.summary['terminal_batch_worker_objects']==int(enabled)
  assert bool(admissions)==enabled
  if enabled:assert not obj.exists() and json.loads(path.read_bytes())['format']==b.FORMAT
  else:assert obj.exists() and path.read_bytes()==before
 finally:background.close()

def test_wrong_response_and_corrupt_compact_scope_fail_closed(batch):
 c,state,path=batch;b.retire(c,2,current(state),apply=True);record=json.loads(path.read_text())
 with pytest.raises(RuntimeError,match='different terminal response'):b.acknowledge(record,state,2,'a'*64)
 record['objects']['999']=dict(format=batch_storage.WORKER_FORMAT,sha256='a'*64,bytes=1)
 with pytest.raises(ValueError,match='invalid compact'):b.acknowledge(record,state,2,record['receipt']['response_sha256'])

@pytest.mark.parametrize('change',['bytes','alias','pin','gate'])
def test_object_changes_after_compaction_are_not_unlinked(batch,monkeypatch,change):
 c,state,path=batch;obj=stored(batch);real=c.retirement_lock;calls=0;allowed=True
 def gate():
  if not allowed:raise ValueError('incompatible reader')
 @contextmanager
 def lock():
  nonlocal calls,allowed
  calls+=1
  if calls==2:
   if change=='bytes':obj.write_bytes(b'unique changed bytes')
   elif change=='alias':obj.rename(obj.with_suffix('.saved'));obj.symlink_to(obj.with_suffix('.saved'))
   elif change=='pin':put(c.root/'1/retain-builds.json',{})
   else:allowed=False
  with real() as acquired:yield acquired
 monkeypatch.setattr(c,'retirement_lock',lock)
 with pytest.raises(ValueError):b.retire(c,2,current(state),apply=True,runtime_admission=gate)
 assert obj.exists() and json.loads(path.read_text())['format']==b.FORMAT

@pytest.mark.parametrize('change',['event','response','worker_id','base','mode'])
def test_saved_preparation_binding_mismatch_preserved(batch,change):
 c,state,path=batch;saved=json.loads(path.read_text())
 if change=='event':saved['prepared']['event']['review_id']=9
 elif change=='response':saved['prepared']['event']['response_sha256']='0'*64
 elif change=='worker_id':saved['prepared']['workers']['9']=saved['prepared']['workers'].pop('1')
 elif change=='base':saved['base_sha256']='bad'
 else:saved['build_cache_mode']=True
 put(path,saved);before=path.read_bytes()
 with pytest.raises(ValueError):b.retire(c,2,current(state),apply=True)
 assert path.read_bytes()==before

def test_truncated_projection_preserves_original(batch):
 c,state,path=batch;path.write_bytes(path.read_bytes()[:-3]);before=path.read_bytes()
 with pytest.raises(ValueError):b.retire(c,2,current(state),apply=True)
 assert path.read_bytes()==before

def test_latest_terminal_transition_requeues_review_during_old_drain(batch,monkeypatch):
 from trellis.dispatch.storage import BackgroundCollector
 c,state,path=batch;bkg=BackgroundCollector(c.root,repo=c.repo,compact_terminal_batches=True,git_maintenance_threshold_bytes=None)
 # Exercise schedule admission without starting a filesystem cleanup thread.
 bkg._running=True
 active=copy.deepcopy(state);active['dispatch']['tasks']['1']['status']['status']='checking'
 assert not bkg.schedule(active);bkg._review_pending.clear()
 assert not bkg.schedule(state);assert list(bkg._review_pending)==['2']
 bkg.close()

@pytest.mark.parametrize('kind',['root','prepared','reference','compact'])
def test_unknown_metadata_keys_are_not_erased_by_projection(batch,kind):
 c,state,path=batch
 if kind=='reference':stored(batch)
 if kind=='compact':b.retire(c,2,current(state),apply=True)
 value=json.loads(path.read_text())
 target=(value['prepared']['workers']['1'] if kind=='reference' else value['prepared'] if kind=='prepared' else value)
 target['future_unknown_authority']={'large':'unknown'};put(path,value);before=path.read_bytes()
 with pytest.raises(ValueError):b.retire(c,2,current(state),apply=True)
 assert path.read_bytes()==before

@pytest.mark.parametrize('kind',['batch','zip'])
def test_hardlinked_originals_are_preserved_without_claimed_savings(batch,kind):
 import os
 c,state,path=batch;obj=stored(batch);target=path if kind=='batch' else obj;keeper=target.with_name('operator-keeper');os.link(target,keeper);before=target.read_bytes()
 with pytest.raises(ValueError,match='independent regular'):b.retire(c,2,current(state),apply=True)
 assert target.read_bytes()==before==keeper.read_bytes()

def test_inplace_receipt_change_during_lease_cannot_mutate_captured_authority(batch,monkeypatch):
 c,state,path=batch;before=path.read_bytes();real=c.retirement_lock
 @contextmanager
 def lock():
  state['dispatch']['receipts']['2']['response_sha256']='d'*64
  with real() as acquired:yield acquired
 monkeypatch.setattr(c,'retirement_lock',lock)
 with pytest.raises(ValueError,match='current state'):b.retire(c,2,current(state),apply=True)
 assert path.read_bytes()==before

def test_restart_requires_durable_compact_ack_before_any_object_unlink(batch,monkeypatch):
 c,state,path=batch;obj=stored(batch);real_lock=c.retirement_lock;calls=0
 @contextmanager
 def first_only():
  nonlocal calls
  calls+=1
  with real_lock() as acquired:yield acquired if calls==1 else False
 monkeypatch.setattr(c,'retirement_lock',first_only)
 b.retire(c,2,current(state),apply=True);assert obj.exists()
 monkeypatch.setattr(c,'retirement_lock',real_lock)
 real_sync=b._sync_dir
 def fail(parent):
  if parent==path.parent:raise OSError('durability failed')
  real_sync(parent)
 monkeypatch.setattr(b,'_sync_dir',fail)
 with pytest.raises(OSError,match='durability'):b.retire(c,2,current(state),apply=True)
 assert obj.exists()
 monkeypatch.setattr(b,'_sync_dir',real_sync)
 assert b.retire(c,2,current(state),apply=True)['removed_objects']==1

@pytest.mark.parametrize('phase', ['projection', 'object_verification'])
@pytest.mark.parametrize('transition', ['append', 'rewind'])
def test_checkpoint_moves_during_precompute_keep_ancestry_fence(batch, monkeypatch, phase, transition):
 c,state,path=batch;obj=stored(batch);before=path.read_bytes()
 witness_commit=git(c.repo,'rev-parse','HEAD')
 counts={'projection':0,'object_verification':0}
 real_projection=b._projection;real_verified=b._verified_object
 def move_checkpoint():
  if transition=='append':
   # An unrelated ordinary checkpoint leaves this batch's terminal facts intact.
   put(c.repo/'unrelated-checkpoint.json',{'next':True})
   git(c.repo,'add','.');git(c.repo,'commit','-qm','Unrelated later checkpoint')
  else:
   # Keep the supplied native observation unchanged: Git ancestry alone must
   # prevent retirement after the committed batch witness has been rewound.
   git(c.repo,'reset','--hard','HEAD^')
 def projection(*args,**kwargs):
  counts['projection']+=1
  value=real_projection(*args,**kwargs)
  if phase=='projection':move_checkpoint()
  return value
 def verified(*args,**kwargs):
  counts['object_verification']+=1
  value=real_verified(*args,**kwargs)
  if phase=='object_verification':move_checkpoint()
  return value
 monkeypatch.setattr(b,'_projection',projection)
 monkeypatch.setattr(b,'_verified_object',verified)
 if transition=='append':
  result=b.retire(c,2,current(state),apply=True)
  assert result['compacted']==1 and result['removed_objects']==1 and not obj.exists()
  # Authority remains the original validated checkpoint; no reparsing or renewed
  # deletion eligibility is needed merely because newer history was appended.
  assert json.loads(path.read_bytes())['checkpoint']==witness_commit
 else:
  with pytest.raises(ValueError,match='checkpoint changed'):
   b.retire(c,2,current(state),apply=True)
  assert path.read_bytes()==before and obj.exists()
 assert counts=={'projection':1,'object_verification':1}

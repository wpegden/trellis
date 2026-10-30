import io
import json
import pytest
from trellis.dispatch import batch_projection as p

def example():
 return dict(base_sha256='b'*64, prepared=dict(event={'graph':{'A':['B']}}, receipt={'review_id':1,'response_sha256':'a'*64,'decisions':[{'task_id':2,'assignment_id':'x','rejection':None}]},workers={'2':{'assignment':{'payload':'hello'},'private_state':{'sources':['long','strings']}}}))
def test_projection_preserves_metadata_not_inline_payload():
 x=example();r=p.read(io.StringIO(json.dumps(x)));assert r['prepared']['receipt']==x['prepared']['receipt'];assert r['prepared']['workers']=={'2':{'assignment':None,'private_state':None}};assert r['prepared']['event']=={'graph':None}
@pytest.mark.parametrize('chunk',[1,2,3,7,64])
def test_every_chunk_boundary_and_escaped_unicode(monkeypatch,chunk):
 monkeypatch.setattr(p,'_CHUNK',chunk);x=example();x['prepared']['workers']['2']['private_state']['value']='string\\\n"é😀\t'*70
 assert p.read(io.StringIO(json.dumps(x,ensure_ascii=False)))['prepared']['workers']=={'2':{'assignment':None,'private_state':None}}
def test_large_ignored_string_never_kept(monkeypatch):
 monkeypatch.setattr(p,'_MAX_TOKEN',100);x=example();x['prepared']['workers']['2']['private_state']['value']='x'*2000000
 assert p.read(io.StringIO(json.dumps(x)))['prepared']['workers']=={'2':{'assignment':None,'private_state':None}}
@pytest.mark.parametrize('raw',['{}x','{"x":1,"x":2}','{"x":01}','{"x":NaN}','{"x":[1,]}','{"x":"bad\\q"}','{"x":"bad\n"}','{"x":true false}','{"x":{"a":2,}}','{"x":"\\uZZZZ"}','[]'])
def test_invalid_discarded_content_rejected(raw):
 with pytest.raises(ValueError):p.read(io.StringIO(raw))
def test_new_worker_reference_kept():
 x=example();x['prepared']['format']='trellis-prepared-batch/1';x['prepared']['workers']['2']={'format':'trellis-prepared-worker/1','sha256':'c'*64,'bytes':99}
 assert p.read(io.StringIO(json.dumps(x)))['prepared']['workers']==x['prepared']['workers']
def test_cancelled_read_refuses():
 with pytest.raises(InterruptedError):p.read(io.StringIO('{}'),cancelled=lambda:True)

def test_metadata_and_live_key_memory_are_bounded(monkeypatch):
 monkeypatch.setattr(p,'_MAX_METADATA',512)
 with pytest.raises(ValueError,match='memory bound'):p.read(io.StringIO(json.dumps({'discard':{str(i):None for i in range(30)}})))
 with pytest.raises(ValueError,match='memory bound'):p.read(io.StringIO(json.dumps({'receipt':'x'*400})))

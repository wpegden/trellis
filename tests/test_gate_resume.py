import hashlib
import json
import pytest
from trellis.gate_resume import check

@pytest.mark.parametrize('choice', ['approve', 'feedback'])
@pytest.mark.parametrize('change', ['none', 'request', 'cycle', 'worker', 'deployment', 'quota', 'halt_marker', 'signal', 'bool_id', 'request_cycle'])
def test_gate_resume_only_current_bound_park(tmp_path, choice, change):
    runtime=tmp_path/'runtime'; repo=tmp_path/'repo';runtime.mkdir();repo.mkdir()
    state=dict(stage='HumanGate',cycle=72,in_flight_request=dict(kind='HumanGate',id=363,cycle=72))
    pause=dict(kind='gate_parked',armed_at_cycle=72)
    signal=dict(choice=choice,cycle=72,request_id=363)
    raw=json.dumps(signal).encode();sha=hashlib.sha256(raw).hexdigest()
    if change=='request':state['in_flight_request']['id']=364
    if change=='cycle':state['cycle']=73
    if change=='request_cycle':state['in_flight_request']['cycle']=71
    if change=='worker':state['stage']='Worker'
    if change in ('deployment','quota'):pause['kind']=change
    if change=='signal':raw+=b' '
    if change=='bool_id':state['in_flight_request']['id']=True
    (runtime/'protocol_state.json').write_text(json.dumps(state))
    (runtime/'pause_request.json').write_text(json.dumps(pause))
    (runtime/'human_gate_response.json').write_bytes(raw)
    marker=repo/'.trellis-stop-after-checkpoint'
    marker.write_text('unrelated stop' if change=='halt_marker' else '[trellis-pause] gate_parked: parked')
    before={p:p.read_bytes() for p in [marker,*runtime.iterdir()]}
    if change=='none':check(runtime,repo,sha)
    else:
        with pytest.raises(ValueError):check(runtime,repo,sha)
    assert all(p.read_bytes()==raw for p,raw in before.items())

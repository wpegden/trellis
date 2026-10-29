import copy
import json
from viewer.parallel_state import project


def test_retired_verifier_keeps_exact_display_scope_without_history_reads(tmp_path):
    job=dict(id=1,status={'status':'resolved','result_sha256':'a'*64},required=True,
             reviewer_requested=False,identity={'lane':'audit','compiler':'managed'},
             request={'scoped_audit':{'goals':['A','B']}})
    state={'dispatch':{'tasks':{},'receipts':{},'verifiers':{'jobs':{'1':job}}}}
    path=tmp_path/'protocol_state.json';path.write_text(json.dumps(state))
    before=project(path)
    retired=copy.deepcopy(job);retired['request']=None
    retired['retired_request']={'checkpoint':'unavailable-for-display','summary':{'audit_goals':['A','B']}}
    state['dispatch']['verifiers']['jobs']['1']=retired;path.write_text(json.dumps(state))
    assert project(path)==before


def test_active_request_display_takes_precedence_over_stale_summary(tmp_path):
    job=dict(id=1,status={'status':'checking'},identity={'lane':'audit'},
             request={'scoped_audit':{'goals':['current']}},
             retired_request={'summary':{'audit_goals':['old']}})
    path=tmp_path/'protocol_state.json';path.write_text(json.dumps({'dispatch':{'verifiers':{'jobs':{'1':job}}}}))
    assert project(path)['dispatch']['verifiers']['jobs']['1']['coarse_nodes']==['current']


def test_terminal_owner_identity_unchanged_by_request_retirement():
    from trellis.dispatch.terminal_retention import terminal_owners
    job=dict(id=1,status={'status':'resolved','result_sha256':'a'*64,'stale_reason':None},
             identity={'lane':'sound','compiler':'none','panel_sha256':'b'*64,'input_hashes':{'A.lean':'c'*64}},
             request={'large_evidence':'retained in Git'})
    state={'dispatch':{'tasks':{},'receipts':{},'verifiers':{'jobs':{'1':job}}}}
    before=terminal_owners(state)
    deferred=terminal_owners(state,defer_verifiers=True)['verifier-1'].resolve()
    job['request']=None;job['retired_request']={'summary':{},'checkpoint':'a'*40}
    assert terminal_owners(state)==before
    assert terminal_owners(state,defer_verifiers=True)['verifier-1'].resolve()==deferred

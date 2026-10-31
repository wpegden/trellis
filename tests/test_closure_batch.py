"""Explicit batches share reads but preserve per-owner collection and freshness."""
import hashlib
import threading
import time

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import admission, server as mod
from trellis.checker.closure_batch import collect
from trellis.checker.protocol import ProtocolError, validate_request
from test_semantic_replay_readiness import fixture


def request(names):
    return validate_request(dict(op='local_closure_axioms_batch', request_id=1,
        nodes=names, principal_names={n:n for n in names}, premise_owners=[], timeout_secs=20))


@pytest.mark.parametrize('fault', ['extra', 'duplicate', 'principal', 'module', 'repo', 'oversize', 'scan'])
def test_batch_registration_rejects_invalid_scope(fault):
    raw = dict(request(['A']).raw)
    if fault == 'extra': raw['principal_names']['B'] = 'B'
    if fault == 'duplicate': raw['nodes'] = ['A', 'A']
    if fault == 'principal': raw['principal_names']['A'] = '../escape'
    if fault == 'module': raw.update(module_owners=['A'], principal_names={})
    if fault == 'repo': raw['repo_path'] = '/tmp/other'
    if fault == 'oversize': raw['nodes'] = ['N'+str(i) for i in range(129)]
    if fault == 'scan': raw['scan_only'] = True
    with pytest.raises(ProtocolError): validate_request(raw)


@pytest.mark.parametrize('mutation', [None, 'artifact', 'source', 'script', 'collector_failure'])
def test_known_set_has_two_fresh_passes_despite_staggered_collectors(tmp_path, monkeypatch, mutation):
    server, names = fixture(tmp_path, count=6)
    repo = server.supervisor_repo
    cache = {p.name: {'sha256': hashlib.sha256(p.read_bytes()).hexdigest()}
             for p in (repo/'Tablet').glob('*.lean')}
    monkeypatch.setattr(mod, 'load_fingerprint_cache', lambda _: cache)
    monkeypatch.setattr(admission, 'ACTIVE', admission.Admission(2, readonly_slots=2))
    reads = []
    original_ready = server._semantic_replay_readiness
    original_keys = server._concurrent_closure_cache_keys
    monkeypatch.setattr(server, '_semantic_replay_readiness', lambda nodes:
        (reads.append(('ready', tuple(nodes))), original_ready(nodes))[1])
    monkeypatch.setattr(server, '_concurrent_closure_cache_keys', lambda items:
        (reads.append(('keys', tuple(n for n, *_ in items))), original_keys(items))[1])
    active = 0
    peak = 0
    finished = []
    lock = threading.Lock()
    def probe(req, *, prepared_replay, batch_keys):
        nonlocal active, peak
        assert prepared_replay and set(batch_keys) == set(names)
        assert len(reads) == 2, 'no per-owner readiness/key pass'
        with lock:
            active += 1
            peak = max(peak, active)
        time.sleep(.015 if req.node_name == names[0] else .003)
        with lock:
            active -= 1
            finished.append(req.node_name)
            if len(finished) == len(names):
                if mutation == 'artifact': obs._tablet_olean_path(repo, names[0]).write_bytes(b'changed')
                if mutation == 'source': (repo/f'Tablet/{names[0]}.lean').write_bytes(b'changed')
                if mutation == 'script': server._local_closure_script_path.write_bytes(b'changed')
        response = dict(node=req.node_name, status='ok', returncode=0, premise_owners=[])
        if mutation == 'collector_failure' and req.node_name == names[1]:
            response.update(status='internal_error', returncode=1, errors=['collector failed'])
        return response, response['returncode'], {}
    monkeypatch.setattr(server, '_handle_local_closure_axioms', probe)
    stores = []
    def store(*args, **kwargs):
        assert len(finished) == len(names) and len(reads) == 4
        stores.append(kwargs['node_name'])
    monkeypatch.setattr(mod, 'store_local_closure_axioms', store)
    output, _, log = collect(server, request(names))
    assert len(reads) == 4 and all(nodes == tuple(names) for _, nodes in reads)
    assert peak == 2 and set(finished) == set(names)
    assert log['closure_batch_read_passes'] == 2
    if mutation in ('artifact', 'source', 'script'):
        assert not stores
        assert all(r['status'] == 'internal_error' for r in output['nodes'].values())
    elif mutation == 'collector_failure':
        assert set(stores) == set(names) - {names[1]}
        assert output['nodes'][names[1]]['status'] == 'internal_error'
    else:
        assert set(stores) == set(names)
        assert all(r['status'] == 'ok' for r in output['nodes'].values())


def test_cold_batch_runs_no_collector_and_returns_no_evidence(tmp_path, monkeypatch):
    server, names = fixture(tmp_path, count=2)
    monkeypatch.setattr(server, '_semantic_replay_readiness', lambda _: dict.fromkeys(names, False))
    monkeypatch.setattr(server, '_concurrent_closure_cache_keys', lambda items: dict.fromkeys(items))
    monkeypatch.setattr(server, '_handle_local_closure_axioms', lambda *_a, **_k: pytest.fail('cold collector'))
    output, _, _ = collect(server, request(names))
    assert output['status'] == 'cold' and output['nodes'] == {}

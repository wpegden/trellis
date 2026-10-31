import json

import pytest

from trellis.dispatch.native import NativeLedger


def test_host_authority_inputs_are_bound_without_entering_worker_source_namespace(tmp_path):
    shared = {'Tablet/A.lean': 'source', 'trellis.config.json': 'config',
              '.trellis/scripts/compiler-command-guard': 'host-tool'}
    calls = []
    def kernel(payload):
        calls.append(payload['action'])
        if payload['action'] == 'dispatch_verifier_inputs':
            return dict(status='dispatch_ok', output=dict(shared))
        assert payload['action'] == 'dispatch_observe_sources'
        return dict(status='dispatch_ok', output=dict(
            input_hashes={'Tablet/A.lean': 'source'}, graph={}, manifest={}))
    ledger = NativeLedger(tmp_path, kernel=kernel)
    view = ledger.observe_sources({}, tmp_path)
    assert view['input_hashes'] == {'Tablet/A.lean': 'source', 'trellis.config.json': 'config'}
    assert view['authority_inputs'] == {'.trellis/scripts/compiler-command-guard': 'host-tool'}
    assert ledger.observe_sources({}, tmp_path) == view
    assert calls.count('dispatch_observe_sources') == 1
    shared['.trellis/scripts/compiler-command-guard'] = 'updated-tool'
    updated = ledger.observe_sources({}, tmp_path)
    assert updated['input_hashes'] == view['input_hashes']
    assert updated['authority_inputs'] != view['authority_inputs']
    assert calls.count('dispatch_observe_sources') == 2
    rows = ledger.measurements.snapshot()['rows']
    assert rows['phase:source_observation.key']['calls'] == 3
    assert rows['phase:source_observation.key']['failures'] == 0


def test_reviewer_start_rechecks_runnable_under_native_lock(tmp_path):
    calls = []
    def kernel(payload):
        calls.append(payload['event'])
        return dict(status='dispatch_ok', output=dict(reviewer=None, reviewer_runnable=False))
    ledger = NativeLedger(tmp_path / 'runtime', kernel=kernel)
    # A prior scheduler snapshot was runnable; a concurrent native transition
    # consumed that opportunity. No rejected event or reviewer state is written.
    assert ledger.start_review() is None
    assert calls == [None]
    assert not (ledger.root / 'reviews').exists()


@pytest.mark.parametrize('current_request, canonical, expected', [
    ({'id': 9}, 'must not parse canonical state again', 9),
    (None, '{"in_flight_request":{"id":9},"request_seq":40}', 9),
    (None, '{"in_flight_request":null,"request_seq":40}', 41),
])
def test_reviewer_start_uses_current_request_or_canonical_sequence(
        tmp_path, monkeypatch, current_request, canonical, expected):
    import trellis.dispatch.native as native
    events, captures = [], []
    def kernel(payload):
        event = payload['event']; events.append(event)
        status = (dict(reviewer=None, reviewer_runnable=True, request=current_request)
                  if event is None else dict(reviewer={'request_id': expected}))
        return dict(status='dispatch_ok', output=status)
    ledger = NativeLedger(tmp_path / 'runtime', kernel=kernel)
    source = ledger.runtime / 'protocol_state.json'; source.write_text(canonical)
    monkeypatch.setattr(native.review_snapshot_files, 'capture',
                        lambda path, value: captures.append((path, value)))
    assert ledger.start_review() == {'request_id': expected}
    assert events == [None, dict(kind='review_started', request_id=expected)]
    assert captures == [(ledger.root / 'reviews' / str(expected) / 'state.json', source)]


@pytest.mark.parametrize('status', ['reserved', 'running', 'checking', 'stopping', 'resolved'])
def test_worker_expiry_rechecks_current_status_under_native_lock(tmp_path, status):
    calls = []
    current = dict(tasks={'1': dict(id=1, status={'status': status})})
    event = dict(kind='attempt_stopping', task_id=1, outcome={'kind': 'timed_out'})
    def kernel(payload):
        calls.append(payload['event'])
        return dict(status='dispatch_ok', output=current)
    ledger = NativeLedger(tmp_path / 'runtime', kernel=kernel)
    # The scheduler observed Running, but CandidateReady may already have
    # committed before this timeout obtains the canonical lock.
    assert ledger.observe(event) == current
    assert calls == ([None, event] if status == 'running' else [None])


def test_unchanged_idle_retry_poll_skips_payload_but_new_state_is_revalidated(tmp_path, monkeypatch):
    import trellis.dispatch.native as native
    path = tmp_path / 'protocol_state.json'
    path.write_text('{"dispatch":{"tasks":{}}}')
    ledger = NativeLedger(tmp_path)
    reads = []
    load = json.load
    def counted(stream):
        reads.append(True)
        return load(stream)
    monkeypatch.setattr(native.json, 'load', counted)
    assert ledger.prepare_retries(tmp_path) == []
    assert ledger.prepare_retries(tmp_path) == []
    assert len(reads) == 1
    # Atomic replacement and same-inode writes both invalidate the hint.
    replacement = tmp_path / 'replacement'; replacement.write_text(path.read_text()); replacement.replace(path)
    assert ledger.prepare_retries(tmp_path) == []
    assert len(reads) == 2
    path.write_text('{"dispatch":{"tasks":{"1":{"status":{"status":"resolved"},"pending_retry":{}}}}}')
    def observe(*args):
        raise RuntimeError('fresh retry must run authoritative observation')
    ledger.observe_sources = observe
    for _ in range(2):
        with pytest.raises(RuntimeError, match='authoritative observation'):
            ledger.prepare_retries(tmp_path)
    # Failures cannot poison the negative cache.
    assert len(reads) == 4


@pytest.mark.parametrize('operation', ['snapshot', 'observe', 'complete_delivery', 'start_review', 'dispatch_batch', 'prepare_publication', 'prepare_retries'])
def test_recorded_publication_blocks_timeout_and_dispatch_until_recovery(tmp_path, operation):
    ledger = NativeLedger(tmp_path/'runtime', kernel=lambda _: pytest.fail('pending publication was bypassed'))
    pending = ledger.root/'publication/pending.json'
    pending.parent.mkdir()
    pending.write_text('{"identity":"recorded-decision"}')
    args = {
        'complete_delivery': ({'id': 1}, None, lambda: 0),
        'snapshot': (), 'observe': ({'kind': 'attempt_stopping', 'task_id': 1, 'outcome': {'kind': 'timed_out'}},),
        'start_review': (), 'dispatch_batch': (1, {'assignments': []}, [], {}),
        'prepare_publication': ({},), 'prepare_retries': (tmp_path,),
    }
    with pytest.raises(RuntimeError, match='pending candidate publication'):
        getattr(ledger, operation)(*args[operation])
    assert pending.exists(), 'only publication recovery may clear the durable decision'


def test_prepared_batch_is_revalidated_after_crash_before_reservation(tmp_path):
    root = tmp_path/'runtime'
    root.mkdir()
    state_path = root/'protocol_state.json'
    state_path.write_text(json.dumps(dict(version=1, dispatch={'receipts': {}})))
    prepares = []
    fail = [True]
    def kernel(payload):
        if payload['action'] == 'dispatch_prepare_batch':
            prepares.append(payload['state']['version'])
            return dict(status='dispatch_ok', output=dict(
                event={'base': payload['state']['version']}, workers={},
                receipt=dict(response_sha256=payload['response_sha256'], decisions=[])))
        if fail[0]:
            raise RuntimeError('simulated crash before reservation')
        return dict(status='dispatch_ok', output={})
    ledger = NativeLedger(root, kernel=kernel)
    with pytest.raises(RuntimeError, match='simulated crash'):
        ledger.dispatch_batch(1, {'assignments': []}, [], {})
    state_path.write_text(json.dumps(dict(version=2, dispatch={'receipts': {}})))
    fail[0] = False
    result = ledger.dispatch_batch(1, {'assignments': []}, [], {})
    assert result['event']['base'] == 2
    assert prepares == [1, 2]
    # Once reserved, an exact replay retrieves its saved private workers,
    # even if unrelated canonical state has advanced since reservation.
    state_path.write_text(json.dumps(dict(version=3, dispatch={'receipts': {'1': result['receipt']}})))
    assert ledger.dispatch_batch(1, {'assignments': []}, [], {}) == result
    assert prepares == [1, 2]
    with pytest.raises(RuntimeError, match='different prepared response'):
        ledger.dispatch_batch(1, {'assignments': ['changed']}, [], {})


def test_current_batch_observation_and_preparation_share_one_locked_state(tmp_path, monkeypatch):
    root = tmp_path / 'runtime'; root.mkdir()
    path = root / 'protocol_state.json'
    path.write_text(json.dumps(dict(version=1, dispatch=dict(receipts={}, config={'profiles': ['strong']}))))
    reads = []
    read_text = type(path).read_text
    def counted(p, *args, **kwargs):
        if p == path:
            reads.append(True)
        return read_text(p, *args, **kwargs)
    monkeypatch.setattr(type(path), 'read_text', counted)
    observed = []
    def kernel(payload):
        if payload['action'] == 'dispatch_prepare_batch':
            assert payload['state'] is observed[-1]
            return dict(status='dispatch_ok', output=dict(event={}, workers={},
                receipt=dict(response_sha256=payload['response_sha256'])))
        return dict(status='dispatch_ok', output={})
    ledger = NativeLedger(root, kernel=kernel)
    def observe(state, repo):
        observed.append(state)
        return dict(graph={}, input_hashes={}, manifest={})
    ledger.observe_sources = observe
    ledger.dispatch_current_batch(1, {'assignments': []}, tmp_path)
    assert len(reads) == 1
    path.write_text(json.dumps(dict(version=2, dispatch=dict(receipts={}, config={'profiles': ['strong']}))))
    ledger.dispatch_current_batch(2, {'assignments': []}, tmp_path)
    assert len(reads) == 2
    assert [s['version'] for s in observed] == [1, 2]


def test_only_one_coordinator_owns_a_run(tmp_path):
    a = NativeLedger(tmp_path)
    b = NativeLedger(tmp_path)
    with a.own_coordinator():
        with pytest.raises(RuntimeError, match='already has'):
            with b.own_coordinator():
                pytest.fail('second coordinator started')
    with b.own_coordinator():
        pass


def test_late_timeout_or_completion_cannot_replace_committed_outcome(tmp_path):
    current = dict(tasks={'1': dict(status={'status': 'stopping', 'outcome': {'kind': 'accepted'}})})
    def kernel(payload):
        assert payload['event'] is None, 'late observation tried to overwrite acceptance'
        return dict(status='dispatch_ok', output=current)
    ledger = NativeLedger(tmp_path, kernel=kernel)
    for event in [dict(kind='attempt_stopping', task_id=1, outcome={'kind': 'timed_out'}),
                  dict(kind='candidate_ready', task_id=1, candidate_sha256='a'*64, now_ms=42)]:
        assert ledger.observe(event) == current


def test_publication_callback_retains_native_identity_and_does_not_relock(tmp_path):
    calls = []
    def kernel(payload):
        calls.append(payload)
        return dict(status='dispatch_ok', output={'event_id': 3})
    ledger = NativeLedger(tmp_path, kernel=kernel)
    payload = {'task_id': 2, 'candidate_sha256': 'a' * 64}
    with ledger.locked():
        assert ledger.commit_publication(2, 'a' * 64, payload) == {'event_id': 3}
    assert calls == [dict(action='dispatch_commit_publication', root=str(tmp_path), payload=payload)]
    with pytest.raises(RuntimeError, match='different task'):
        ledger.commit_publication(1, 'a' * 64, payload)


def test_source_materialization_precedes_reservation_and_replays_after_commit(tmp_path):
    root = tmp_path / 'runtime'
    root.mkdir()
    state = dict(dispatch={'receipts': {}})
    path = root / 'protocol_state.json'
    path.write_text(json.dumps(state))
    stages = []
    prepared = {}
    def kernel(payload):
        if payload['action'] == 'dispatch_prepare_batch':
            prepared.update(event={'kind': 'batch_prepared'}, workers={'1': {'snapshot': 'captured'}},
                receipt=dict(response_sha256=payload['response_sha256'], decisions=[]))
            return dict(status='dispatch_ok', output=prepared)
        stages.append('reserve')
        assert stages[-2] == 'capture'
        state['dispatch']['receipts']['7'] = prepared['receipt']
        path.write_text(json.dumps(state))
        return dict(status='dispatch_ok', output={})
    def materialize(workers, *, committed=False):
        assert workers == {'1': {'snapshot': 'captured'}}
        stages.append('capture')
    ledger = NativeLedger(root, kernel=kernel, materialize=materialize)
    ledger.dispatch_batch(7, {'assignments': []}, [], {})
    ledger.dispatch_batch(7, {'assignments': []}, [], {})
    assert stages == ['capture', 'reserve', 'capture']


@pytest.mark.parametrize('review_id', [793, 4294967295])
def test_current_batch_allocates_legal_disjoint_helper_prefixes_by_position(tmp_path, review_id):
    from trellis.checker.protocol import _validate_node_name
    from trellis.incremental_check import _validate_node_name as advisory_node_name
    root = tmp_path / 'runtime'; root.mkdir()
    (root / 'protocol_state.json').write_text(json.dumps(dict(dispatch=dict(
        receipts={}, config={'profiles': ['strong']}))))
    captured = []
    def kernel(payload):
        if payload['action'] == 'dispatch_prepare_batch':
            captured.extend(payload['assignments'])
            return dict(status='dispatch_ok', output=dict(event={}, workers={},
                receipt=dict(response_sha256=payload['response_sha256'])))
        return dict(status='dispatch_ok', output={})
    ledger = NativeLedger(root, kernel=kernel)
    ledger.observe_sources = lambda *_: dict(graph={}, input_hashes={'Tablet/C.lean': 'full-source'},
        proof_body_rebase_inputs={'Tablet/C.lean': 'prefix'}, manifest={})
    # These labels would collide under hyphen-to-underscore replacement;
    # the maximum-length label also must not exhaust the node-name budget.
    ids = ['strict-grid-loss', 'strict_grid_loss', 'x' * 96] + [f'job-{i}' for i in range(61)]
    response = dict(assignments=[dict(id=id, profile='strong', compiler='managed',
        task_kind='ordinary', review={}) for id in ids])
    ledger.dispatch_current_batch(review_id, response, tmp_path)
    assert all(item['options']['input_hashes'] == {'Tablet/C.lean': 'full-source'}
               and item['options']['proof_body_rebase_inputs'] == {'Tablet/C.lean': 'prefix'} for item in captured)
    prefixes = [item['options']['helper_prefix'] for item in captured]
    assert prefixes == [f'P{review_id}_T{i}_' for i in range(1, 65)]
    assert [item['options']['assignment_id'] for item in captured] == ids
    for prefix in prefixes:
        assert len(prefix) <= 96
        node = prefix + 'grid_boundary_retained'
        assert _validate_node_name(node) == advisory_node_name(node) == node
        assert all(not other.startswith(prefix) for other in prefixes if other != prefix)
    # Replaying this unchanged response reuses its exact saved preparation.
    ledger.dispatch_current_batch(review_id, response, tmp_path)
    assert len(captured) == 64


def test_committed_batch_keeps_legacy_helper_prefix_on_recovery(tmp_path):
    root = tmp_path / 'runtime'; root.mkdir()
    state = dict(dispatch=dict(receipts={}, config={'profiles': ['strong']}))
    path = root / 'protocol_state.json'; path.write_text(json.dumps(state))
    prepares = []
    def kernel(payload):
        if payload['action'] == 'dispatch_prepare_batch':
            prepares.append(payload['assignments'])
            return dict(status='dispatch_ok', output=dict(
                event={'assignments': payload['assignments']}, workers={},
                receipt=dict(response_sha256=payload['response_sha256'])))
        return dict(status='dispatch_ok', output={})
    ledger = NativeLedger(root, kernel=kernel)
    ledger.observe_sources = lambda *_: dict(graph={}, input_hashes={}, manifest={})
    response = dict(assignments=[dict(id='strict-grid-loss', profile='strong',
        compiler='managed', task_kind='ordinary', review={})])
    legacy = [dict(options=dict(helper_prefix='P793_strict-grid-loss_'))]
    saved = ledger.dispatch_batch(793, response, legacy, {})
    state['dispatch']['receipts']['793'] = saved['receipt']
    path.write_text(json.dumps(state))
    assert ledger.dispatch_current_batch(793, response, tmp_path) == saved
    assert prepares == [legacy]


def test_capacity_blocked_retry_is_deferred_without_materialization_or_journal(tmp_path):
    path = tmp_path / 'protocol_state.json'
    state = dict(dispatch=dict(tasks={'1': dict(status={'status': 'resolved'}, pending_retry={})}))
    original = json.dumps(state)
    path.write_text(original)
    calls, materialized = [], []
    def kernel(payload):
        calls.append(payload['action'])
        assert payload['action'] == 'dispatch_prepare_retry'
        return dict(status='dispatch_ok', output=None)
    ledger = NativeLedger(tmp_path, kernel=kernel, materialize=lambda value: materialized.append(value))
    ledger.observe_sources = lambda *args: dict(graph={}, input_hashes={}, manifest={})
    assert ledger.prepare_retries(tmp_path) == []
    assert ledger.prepare_retries(tmp_path) == []
    assert calls == ['dispatch_prepare_retry']
    path.write_text(original + '\n')  # A capacity change invalidates the scheduling hint.
    assert ledger.prepare_retries(tmp_path) == []
    assert calls == ['dispatch_prepare_retry', 'dispatch_prepare_retry']
    assert materialized == []
    assert path.read_text() == original + '\n'
    assert not (ledger.root / 'retries/1/prepared.json').exists()


@pytest.mark.parametrize('status', ['reserved', 'running', 'submitted', 'checking', 'stopping', 'resolved'])
@pytest.mark.parametrize('failure', [None, {'kind': 'launch_failed', 'message': 'failed'}])
def test_delivery_rechecks_current_status_and_keeps_first_winner(tmp_path, status, failure):
    from trellis.dispatch.coordination import Completion
    calls = []
    current = dict(tasks={'1': dict(id=1, status={'status': status}, deadline_ms=2000)})
    def kernel(payload):
        calls.append(payload['event'])
        return dict(status='dispatch_ok', output=current)
    ledger = NativeLedger(tmp_path, kernel=kernel)
    ledger.complete_delivery({'id': 1}, Completion(1, 'a' * 64, failure), lambda: 1000)
    if status in ('checking', 'stopping', 'resolved'):
        assert calls == [None]
    else:
        expected = (dict(kind='attempt_stopping', task_id=1, outcome=failure) if failure else
                    dict(kind='candidate_ready', task_id=1, candidate_sha256='a' * 64, now_ms=1000))
        assert calls == [None, expected], 'one current-status read and one native event only'


@pytest.mark.parametrize('owned_clock', [False, True])
def test_delivery_samples_legacy_deadline_after_native_read_but_preserves_owned_clock(tmp_path, owned_clock):
    from trellis.dispatch.coordination import Completion
    calls, now = [], [1999]
    attempt = dict(turn=1, contract_sha256='contract') if owned_clock else None
    task = dict(id=1, status={'status': 'running'}, deadline_ms=2000, worker_attempt=attempt)
    def kernel(payload):
        calls.append(payload['event'])
        now[0] = 2000  # Deadline elapsed while acquiring current native status.
        return dict(status='dispatch_ok', output={'tasks': {'1': task}})
    ledger = NativeLedger(tmp_path, kernel=kernel)
    ledger.complete_delivery(task, Completion(1, 'a' * 64), lambda: now[0])
    assert len(calls) == 2
    assert calls[1] == (dict(kind='candidate_ready', task_id=1, candidate_sha256='a' * 64, now_ms=2000)
        if owned_clock else dict(kind='attempt_stopping', task_id=1, outcome={'kind': 'timed_out'}))


@pytest.mark.parametrize('failure', [None, {'kind': 'launch_failed'}])
def test_old_turn_delivery_cannot_stop_or_submit_repaired_turn(tmp_path, failure):
    from trellis.dispatch.coordination import Completion
    current = dict(tasks={'1': dict(id=1, status={'status': 'reserved'},
        worker_attempt=dict(turn=2, contract_sha256='contract'))})
    calls = []
    def kernel(payload):
        calls.append(payload['event'])
        return dict(status='dispatch_ok', output=current)
    ledger = NativeLedger(tmp_path, kernel=kernel)
    launched = dict(id=1, worker_attempt=dict(turn=1, contract_sha256='contract'))
    assert ledger.complete_delivery(launched, Completion(1, 'old', failure), lambda: 0) == current
    assert calls == [None]


def retry_preparation_ledger(tmp_path, *, blocked=(), refused=(), crash=None):
    """Use durable state changes at the native transaction boundary."""
    path = tmp_path / 'protocol_state.json'
    state = dict(revision=1, dispatch=dict(tasks={str(i): dict(
        status={'status': 'resolved'}, pending_retry={}) for i in (1, 2)}))
    path.write_text(json.dumps(state))
    prepared, committed, materialized = [], [], []
    def kernel(payload):
        if payload['action'] == 'dispatch_prepare_retry':
            predecessor = payload['predecessor']
            prepared.append((predecessor, payload['state']['revision']))
            if predecessor in blocked:
                return dict(status='dispatch_ok', output=None)
            return dict(status='dispatch_ok', output=dict(
                task_id=None if predecessor in refused else predecessor + 10,
                worker={'predecessor': predecessor}, event={'predecessor': predecessor}))
        assert payload['action'] == 'dispatch_control'
        if crash is not None and crash[0]:
            raise RuntimeError('crash before native reservation')
        predecessor = payload['event']['predecessor']
        committed.append(predecessor)
        current = json.loads(path.read_text())
        current['dispatch']['tasks'][str(predecessor)]['pending_retry'] = None
        current['revision'] += 1
        path.write_text(json.dumps(current))
        return dict(status='dispatch_ok', output={})
    ledger = NativeLedger(tmp_path, kernel=kernel, materialize=materialized.append)
    ledger.observe_sources = lambda *_: dict(graph={}, input_hashes={}, manifest={})
    return ledger, prepared, committed, materialized


@pytest.mark.parametrize('refused', [(), (1,)])
def test_retry_preparation_commits_only_one_durable_decision_per_call(tmp_path, refused):
    ledger, prepared, committed, materialized = retry_preparation_ledger(tmp_path, refused=refused)
    assert len(ledger.prepare_retries(tmp_path)) == 1
    assert prepared == [(1, 1)] and committed == [1]
    assert len(materialized) == (0 if refused else 1)
    assert len(ledger.prepare_retries(tmp_path)) == 1
    assert prepared == [(1, 1), (2, 2)] and committed == [1, 2]
    assert ledger.prepare_retries(tmp_path) == []


def test_capacity_miss_does_not_hide_later_eligible_retry(tmp_path):
    ledger, prepared, committed, materialized = retry_preparation_ledger(tmp_path, blocked=(1,))
    assert len(ledger.prepare_retries(tmp_path)) == 1
    assert prepared == [(1, 1), (2, 1)] and committed == [2]
    assert materialized == [{'12': {'predecessor': 2}}]


@pytest.mark.parametrize('state_changes', [False, True])
def test_retry_preparation_crash_preserves_workspace_and_revalidates_changed_state(tmp_path, state_changes):
    crash = [True]
    ledger, prepared, committed, materialized = retry_preparation_ledger(tmp_path, crash=crash)
    with pytest.raises(RuntimeError, match='crash before native reservation'):
        ledger.prepare_retries(tmp_path)
    assert committed == [] and len(materialized) == 1
    saved = ledger.root / 'retries/1/prepared.json'
    assert saved.exists()
    if state_changes:
        path = tmp_path / 'protocol_state.json'
        state = json.loads(path.read_text()); state['revision'] = 9
        path.write_text(json.dumps(state))
    crash[0] = False
    assert len(ledger.prepare_retries(tmp_path)) == 1
    assert committed == [1] and len(materialized) == 2
    assert prepared == ([(1, 1), (1, 9)] if state_changes else [(1, 1)])


def test_pause_after_materialization_retains_preparation_without_native_reservation(tmp_path):
    ledger, prepared, committed, materialized = retry_preparation_ledger(tmp_path)
    paused = [False]
    def materialize(value):
        materialized.append(value)
        paused[0] = True
    ledger.materialize = materialize
    assert ledger.prepare_retries(tmp_path, proceed=lambda: not paused[0]) == []
    assert prepared == [(1, 1)] and committed == []
    assert (ledger.root / 'retries/1/prepared.json').exists()
    assert ledger.prepare_retries(tmp_path, proceed=lambda: not paused[0]) == []
    assert len(materialized) == 1
    paused[0] = False
    ledger.materialize = materialized.append
    assert len(ledger.prepare_retries(tmp_path, proceed=lambda: not paused[0])) == 1
    assert prepared == [(1, 1)] and committed == [1]


@pytest.mark.parametrize('kind', ['new', 'existing', 'symlink', 'ancestor_symlink', 'directory'])
def test_assignment_evidence_inputs_capture_exact_bytes_or_absence_without_aliases(tmp_path, kind):
    from trellis.dispatch.candidate import digest
    repo = tmp_path / 'repo'; repo.mkdir()
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    (runtime / 'protocol_state.json').write_text(json.dumps(dict(dispatch=dict(config=dict(profiles={'p': {}})))))
    name = 'reference/model-evidence/probe.rs'
    path = repo / name; path.parent.mkdir(parents=True)
    if kind == 'existing':
        path.write_bytes(b'original evidence')
    elif kind == 'symlink':
        outside = tmp_path / 'outside'; outside.write_text('not authorized')
        path.symlink_to(outside)
    elif kind == 'ancestor_symlink':
        path.parent.rmdir(); path.parent.symlink_to(tmp_path / 'absent')
    elif kind == 'directory':
        path.mkdir()
    source = dict(input_hashes={'Tablet/A.lean': 'original'}, manifest={}, graph={})
    ledger = NativeLedger(runtime, kernel=lambda _: pytest.fail('no native operation needed'))
    ledger.observe_sources = lambda *args: source
    ledger._dispatch_batch = lambda rid, response, assignments, graph, **kw: assignments
    response = dict(assignments=[dict(id='evidence', profile='p', compiler='managed', task_kind='ordinary',
                                    evidence_paths=[name], review={})])
    prepared = ledger.dispatch_current_batch(1, response, repo)[0]
    if kind in ('new', 'existing'):
        assert prepared['preparation_error'] is None
        assert prepared['options']['evidence_paths'] == [name]
        assert prepared['options']['input_hashes'][name] == ('absent' if kind == 'new' else digest(b'original evidence'))
    else:
        assert prepared['preparation_error']
        assert 'evidence_paths' not in prepared['options']
    assert source['input_hashes'] == {'Tablet/A.lean': 'original'}, 'peer source observation remains unchanged'

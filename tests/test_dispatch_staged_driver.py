import asyncio
from contextlib import contextmanager
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis.dispatch import candidate, staged_driver as staged
from trellis.dispatch.publication import atomic


def put(path, value):
    atomic(path, candidate.canonical_json(value))


def test_verifier_executes_and_reaps_while_finalization_only_defers_its_verdict(tmp_path, monkeypatch):
    from trellis.dispatch import driver as driver_module
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    async def exercise():
        driver = driver_module.ProductionDriver.__new__(driver_module.ProductionDriver)
        driver.root = tmp_path
        driver.finalization_gate = asyncio.Lock()
        driver.pausing = lambda: False
        reaped = asyncio.Event()
        process = SimpleNamespace(stop_and_reap=lambda _: reaped.set())
        job = {'id': 1, 'status': {'status': 'running'}}
        driver._start_verifier = lambda _: (job, process, {})
        driver._record_transport = lambda *_: None
        async def wait(*_):
            return {'verdict': 'Pass'}
        driver._wait = wait
        events = []
        async def route(event):
            events.append(event['kind'])
            return {'verifiers': {'jobs': {'1': {'status': {'status': 'resolved'}}}}}
        driver._route = route
        await driver.finalization_gate.acquire()
        runner = asyncio.create_task(driver._verifier(job))
        await asyncio.wait_for(reaped.wait(), 1)
        assert events == ['verifier_candidate']
        assert not runner.done()
        driver.finalization_gate.release()
        await asyncio.wait_for(runner, 1)
        assert events == ['verifier_candidate', 'verifier_finished']
    asyncio.run(exercise())


def test_plan_reads_pure_merged_candidate_with_flat_before_hashes(tmp_path):
    repo = tmp_path / 'canonical'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('import Tablet.B\n-- BODY\nold A')
    (repo / 'Tablet/B.lean').write_text('current B')
    (repo / 'trellis.config.json').write_text('{}')
    before = candidate.source_manifest(repo)
    worker = tmp_path / 'worker'
    (worker / 'Tablet').mkdir(parents=True)
    for path in before:
        (worker / path).write_bytes((repo / path).read_bytes())
    (worker / 'Tablet/A.lean').write_text('import Tablet.B\nimport Tablet.A_Helper\n-- BODY\nnew A')
    (worker / 'Tablet/A_Helper.lean').write_text('helper proof')
    (worker / 'Tablet/A_Helper.tex').write_text('helper statement')
    root = tmp_path / 'runtime/dispatch'
    identity = candidate.seal(worker, root / '1/candidate', task_id=1, base_manifest=before,
                              inputs=before, response={'outcome': 'valid'})
    task = dict(id=1, status={'status': 'checking', 'candidate_sha256': identity},
                assignment={'scope': {'nodes': ['A'], 'helper_prefixes': ['A_']}})
    state = {'dispatch': {'tasks': {'1': task}}}
    graph = {'edges': {'A': ['B'], 'B': []}}
    source = dict(manifest=before, input_hashes=before, graph=graph)
    private = {'in_flight_request': {'id': 3}}
    locked = []
    @contextmanager
    def lock():
        locked.append(True)
        try: yield
        finally: locked.pop()
    def native(payload):
        assert locked
        if payload['action'] == 'dispatch_acceptance_state':
            return private
        assert payload['action'] == 'plan_staged_worker'
        assert payload['current_private'] == private and payload['source_graph'] == graph
        assert payload['before_snapshot'] == {Path(p).name: h for p, h in before.items()}
        merged = Path(payload['repo_path'])
        assert (merged / 'Tablet/A.lean').read_bytes() == (worker / 'Tablet/A.lean').read_bytes()
        assert (merged / 'Tablet/A_Helper.lean').is_file()
        assert (merged / 'Tablet/B.lean').read_text() == 'current B'
        assert not (merged / '.lake').exists()
        return dict(status='planned', read_nodes=['A', 'A_Helper', 'B'], write_nodes=['A', 'A_Helper'])
    driver = SimpleNamespace(repo=repo, root=root, state=lambda: state, require_no_halt=lambda: None,
        _submission_directory=lambda _: root / '1', native=SimpleNamespace(locked=lock,
        _require_recovered_publication=lambda: None, observe_sources=lambda *_: source, _call=native))
    assert staged.plan(driver, 1, identity)['read_nodes'] == ['A', 'A_Helper', 'B']
    assert candidate.source_manifest(repo) == before
    assert not locked


def test_actual_import_reads_must_fit_persisted_native_plan(tmp_path):
    put(tmp_path / 'canonical.json', {'staged_plan': {'read_nodes': ['A', 'B']}})
    staged._require_facts(tmp_path, {'status': 'facts', 'facts': {'read_nodes': ['A']}})
    with pytest.raises(candidate.CandidateError, match='exceed the admitted'):
        staged._require_facts(tmp_path, {'status': 'facts', 'facts': {'read_nodes': ['A', 'NewImport']}})


@pytest.mark.parametrize('fault', [None, 'same_implementation', 'source', 'receipt', 'reason', 'stage'])
def test_metadata_scope_retry_requires_updated_implementation_and_intact_submission(tmp_path, monkeypatch, fault):
    put(tmp_path / 'canonical.json', {'key': {'implementation_identity': 'old'}})
    checked = dict(stage='owned', status='cold', reason='unsupported staged structural or trust payload')
    if fault == 'reason': checked['reason'] = 'source changed'
    if fault == 'stage': checked['stage'] = 'finalize'
    monkeypatch.setattr(staged.receipts, 'implementation_identity', lambda: 'old' if fault == 'same_implementation' else 'new')
    monkeypatch.setattr(staged.stages, 'reusable', lambda *_: None if fault == 'receipt' else checked)
    monkeypatch.setattr(staged, '_sealed_authored_inputs_match', lambda *_: fault != 'source')
    assert staged._metadata_scope_retry(tmp_path, checked) is (fault is None)


def test_recomputed_plan_must_fit_retained_semantic_reservation(tmp_path):
    root = tmp_path / 'dispatch'
    put(root / '1/acceptance-admission.json', {'footprint': {'reads': ['B'], 'writes': ['A'], 'exclusive': False}})
    driver = SimpleNamespace(root=root)
    staged._within_reservation(driver, 1, {'read_nodes': ['A', 'B'], 'write_nodes': ['A']})
    with pytest.raises(candidate.CandidateError, match='exceed the retained'):
        staged._within_reservation(driver, 1, {'read_nodes': ['A', 'C'], 'write_nodes': ['A']})


@pytest.mark.parametrize('adopt', [False, True])
def test_current_context_retry_never_restarts_owned_check(tmp_path, monkeypatch, adopt):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    owned = root / '1/acceptance/1'
    identity = 'c' * 64
    native_plan = {'status': 'planned', 'read_nodes': ['A', 'B'], 'write_nodes': ['A']}
    calls = []
    gate_active = []
    class Gate:
        async def __aenter__(self):
            calls.append('gate-enter')
            gate_active.append(True)
        async def __aexit__(self, *_):
            gate_active.pop()
            calls.append('gate-exit')
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    def prepare(_driver, _task_id, _identity, directory, **kwargs):
        assert bool(gate_active) == ('finalization' in directory.parts)
        calls.append(('prepare', directory.relative_to(root).as_posix()))
        put(directory / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': native_plan})
        return {'status': 'prepared', 'directory': directory}
    monkeypatch.setattr(staged, '_prepare', prepare)
    monkeypatch.setattr(staged, 'plan', lambda *_: native_plan)
    async def execute(_driver, directory, _task_id, _identity, stage):
        calls.append(('execute', stage, directory.name))
        if stage == 'owned':
            assert not gate_active
            return {'status': 'facts', 'facts': {'read_nodes': ['A', 'B']}}
        assert gate_active
        return {'status': 'checked', 'output': {'response': {'outcome': 'Valid'}},
                'candidate_sha256': identity, 'checked_sources': {'current': directory.name}}
    monkeypatch.setattr(staged, '_execute', execute)
    published = []
    def publish(*args, **kwargs):
        assert gate_active and kwargs == {'staged': True}
        published.append(args[2])
        return len(published) == 2
    driver = SimpleNamespace(root=root, finalization_gate=Gate(), _publish=publish)
    if adopt:
        put(owned / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': native_plan})
        put(owned / 'finalization/1/canonical.json', {'key': {'candidate_sha256': identity}})
        put(owned / 'finalization/1/staged-donor.json', {'prepared': True})
    assert asyncio.run(staged.run(driver, 1, identity)) is True
    assert [row for row in calls if isinstance(row, tuple) and row[:2] == ('execute', 'owned')] == [('execute', 'owned', '1')]
    assert [row for row in calls if isinstance(row, tuple) and row[:2] == ('execute', 'finalize')] == [
        ('execute', 'finalize', '1'), ('execute', 'finalize', '2')]
    assert (owned / 'finalization/1/finalization-retry.json').exists()
    assert calls.count('gate-enter') == 1 and calls.count('gate-exit') == 1
    if adopt:
        assert ('prepare', '1/acceptance/1') not in calls
        assert ('prepare', '1/acceptance/1/finalization/1') not in calls


def test_cold_finalization_retains_facts_without_implicit_retry(tmp_path, monkeypatch):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    owned = root / '1/acceptance/1'
    identity = 'c' * 64
    put(owned / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': {'read_nodes': ['A']}})
    put(owned / 'finalization/1/canonical.json', {'key': {'candidate_sha256': identity}})
    put(owned / 'finalization/1/staged-donor.json', {'prepared': True})
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    calls = []
    async def execute(_driver, _directory, _task, _identity, stage):
        calls.append(stage)
        return ({'status': 'facts', 'facts': {'read_nodes': ['A']}} if stage == 'owned'
                else {'status': 'cold', 'reason': 'imported byte identity changed'})
    monkeypatch.setattr(staged, '_execute', execute)
    driver = SimpleNamespace(root=root, finalization_gate=asyncio.Lock(),
        _publish=lambda *_args, **_kwargs: pytest.fail('cold facts cannot publish'))
    with pytest.raises(RuntimeError, match='proof facts retained: imported byte identity changed'):
        asyncio.run(staged.run(driver, 1, identity))
    assert calls == ['owned', 'finalize']
    assert not (owned / 'finalization/1/finalization-retry.json').exists()


@pytest.mark.parametrize('exhausted', [False, True])
@pytest.mark.parametrize('failure', ['capture', 'owned_alias'])
def test_capture_infrastructure_retry_never_reexecutes_owned_stage(tmp_path, monkeypatch, exhausted, failure):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    owned = root / '1/acceptance/1'
    identity = 'c' * 64
    put(owned / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': {'read_nodes': ['A']}})
    put(owned / 'finalization/1/canonical.json', {'key': {'candidate_sha256': identity}})
    put(owned / 'finalization/1/staged-donor.json', {'prepared': True})
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    if failure == 'owned_alias':
        monkeypatch.setattr(staged, '_owned_alias_projection_retry', lambda *_: True)
    calls = []
    async def execute(_driver, directory, _task, _identity, stage):
        calls.append((stage, directory.name))
        if stage == 'owned':
            return {'status': 'facts', 'facts': {'read_nodes': ['A']}}
        if directory.name == '1':
            result = {'stage': 'finalize', 'status': 'cold',
                      'reason': 'current proof environment unavailable: import inventory unavailable or oversized'}
            if failure == 'capture':
                result['retry_class'] = 'finalization_environment_capture'
            return result
        return {'status': 'checked', 'output': {'response': {'outcome': 'Valid'}},
                'candidate_sha256': identity, 'checked_sources': {}}
    monkeypatch.setattr(staged, '_execute', execute)
    def prepare(_driver, _task, _identity, directory, **kwargs):
        assert kwargs['owned'] == owned
        put(directory / 'canonical.json', {'key': {'candidate_sha256': identity}})
        return {'status': 'prepared'}
    monkeypatch.setattr(staged, '_prepare', prepare)
    retries = []
    def retry(task, sha, directory, reason):
        retries.append(directory)
        if exhausted:
            raise RuntimeError('host check retry allowance exhausted; submission retained')
        put(directory / 'retry-authorized.json', {'bounded': True})
    driver = SimpleNamespace(root=root, finalization_gate=asyncio.Lock(), _retry_check=retry,
        _publish=lambda *_args, **_kwargs: True)
    if exhausted:
        with pytest.raises(RuntimeError, match='retry allowance exhausted'):
            asyncio.run(staged.run(driver, 1, identity))
        assert calls == [('owned', '1'), ('finalize', '1')]
    else:
        assert asyncio.run(staged.run(driver, 1, identity))
        assert calls == [('owned', '1'), ('finalize', '1'), ('finalize', '2')]
    assert retries == [owned / 'finalization/1']
    assert not (owned / 'retry-authorized.json').exists()


@pytest.mark.parametrize('fault', [None, 'same_producer', 'unknown', 'semantic', 'donation', 'foreign', 'owned'])
def test_legacy_b7_capture_retry_requires_exact_saved_failure_after_update(tmp_path, monkeypatch, fault):
    producer = next(iter(staged.stages.reviewed_owned_producers()))
    monkeypatch.setattr(staged.receipts, 'implementation_identity', lambda: producer if fault == 'same_producer' else 'current')
    directory = Path('/path/to/trellis/math/superharmonic-runtime/dispatch/74/acceptance/4/finalization/1')
    monkeypatch.setattr(staged, '_read', lambda _: {'key': {'implementation_identity': 'other' if fault == 'unknown' else producer}})
    missing = directory.parent.parent / ('repo/.trellis/supervisor/repo/.trellis/kernel-replay-environments/' + 'a' * 64 + '.json')
    if fault == 'foreign':
        missing = Path('/foreign/.trellis/kernel-replay-environments/' + 'a' * 64 + '.json')
    final = dict(stage='owned' if fault == 'owned' else 'finalize', status='cold',
        reason='current proof environment unavailable: import inventory unavailable or oversized',
        replay_donation=dict(status='cold', copied_nodes=[], covered_nodes=[], captures=[],
            reason=("[Errno 2] No such file or directory: '" + str(missing) + "'")[:240]))
    if fault == 'semantic':
        final['reason'] = 'staged owner source reads changed'
    if fault == 'donation':
        final['replay_donation']['captures'] = [{}]
    assert staged._retryable_finalization(directory, final) is (fault is None)


@pytest.mark.parametrize('fault', [None, 'producer', 'same_producer', 'stage', 'typed', 'reason',
                                  'failure_class', 'partial_donation', 'capture_count', 'capture_status',
                                  'completion', 'authored_source', 'missing_manifest', 'changed_binding'])
def test_owned_alias_retry_requires_exact_authenticated_retained_evidence(tmp_path, monkeypatch, fault):
    from trellis.checker import portable_replay as portable
    from trellis.dispatch import replay_donation as donation
    producer = 'trellis-worker-acceptance-v2:sha256:bb19564229a19e67703e65ebdcd7347713313ca3596b96f94812ad07a3c4f46f'
    directory = tmp_path / 'dispatch/76/acceptance/5/finalization/1'
    captured = {'key': {'implementation_identity': 'unknown' if fault == 'producer' else producer}}
    put(directory / 'canonical.json', captured)
    reason = 'saved owner imported environment changed: A'
    final = dict(stage='owned' if fault == 'stage' else 'finalize', status='cold',
        reason='current proof environment unavailable: import inventory unavailable or oversized; replay donation: ' + reason,
        replay_donation=dict(status='cold', failure_class='saved_owner_binding', reason=reason,
            copied_nodes=[], covered_nodes=[], captures=[dict(status='captured', environment_sha256='a' * 64)]))
    if fault == 'typed': final['retry_class'] = 'something_else'
    if fault == 'reason': final['reason'] = 'staged owner source reads changed'
    if fault == 'failure_class': final['replay_donation']['failure_class'] = 'another_failure'
    if fault == 'partial_donation': final['replay_donation']['copied_nodes'] = ['A']
    if fault == 'capture_count': final['replay_donation']['captures'] *= 2
    if fault == 'capture_status': final['replay_donation']['captures'][0]['status'] = 'unavailable'
    monkeypatch.setattr(staged.receipts, 'implementation_identity', lambda: producer if fault == 'same_producer' else 'new')
    monkeypatch.setattr(staged.stages, 'reusable', lambda *_: None if fault == 'completion' else final)
    monkeypatch.setattr(staged, '_sealed_authored_inputs_match', lambda *_: fault != 'authored_source')
    seen = []
    def manifest(repo, identity):
        assert repo == directory / 'repo/.trellis/supervisor/repo' and identity == 'a' * 64
        if fault == 'missing_manifest': raise FileNotFoundError('retained capture is absent')
        return {'exact_retained': True}
    monkeypatch.setattr(portable, 'environment_for', manifest)
    monkeypatch.setattr(portable, 'EnvironmentView', lambda value: value)
    def recover(runtime, view, *, historical_consumer):
        assert runtime == directory / 'runtime' and view == {'exact_retained': True}
        assert historical_consumer == producer
        seen.append(runtime)
        if fault == 'changed_binding': raise portable.Unavailable('saved owner bytes differ')
        return {'A': 'b' * 64}, {}
    monkeypatch.setattr(donation, 'recover_owned_alias', recover, raising=False)
    original = (directory / 'canonical.json').read_bytes()
    assert staged._retryable_finalization(directory, final) is (fault is None)
    assert seen == ([directory / 'runtime'] if fault in (None, 'changed_binding') else [])
    assert (directory / 'canonical.json').read_bytes() == original
    assert not (directory / 'retry-authorized.json').exists()  # Only native _retry_check grants retries.


@pytest.mark.parametrize('record', [None, 'process.json', 'permit.json', 'result.json', 'cleanup.json'])
def test_only_unlaunched_reviewed_finalizer_preparation_is_superseded(tmp_path, monkeypatch, record):
    producer = next(iter(staged.stages.reviewed_owned_producers()))
    monkeypatch.setattr(staged.receipts, 'implementation_identity', lambda: 'current')
    directory = tmp_path / 'dispatch/77/acceptance/1/finalization/1'
    capture = {'key': {'implementation_identity': producer}}
    put(directory / 'canonical.json', capture)
    put(directory / 'staged-donor.json', {'existing': True})
    if record:
        put(directory / 'execution' / record, {'original': True})
    original = {p: p.read_bytes() for p in directory.rglob('*.json')}
    assert staged._supersede_unlaunched_finalization(directory) is (record is None)
    assert all(path.read_bytes() == data for path, data in original.items())
    if record is None:
        marker = json.loads((directory / 'finalization-retry.json').read_text())
        assert marker['producer_identity'] == producer and marker['consumer_identity'] == 'current'
    else:
        assert not (directory / 'finalization-retry.json').exists()



@pytest.mark.parametrize('producer', ['current', 'unreviewed-intermediate'])
@pytest.mark.parametrize('record', [None, 'process.json', 'permit.json', 'result.json', 'cleanup.json', 'completed'])
def test_incomplete_finalizer_preparation_is_preserved_and_superseded(tmp_path, monkeypatch, producer, record):
    monkeypatch.setattr(staged.receipts, 'implementation_identity', lambda: 'current')
    directory = tmp_path / 'dispatch/74/acceptance/4/finalization/2'
    put(directory / 'canonical.json', {'key': {'implementation_identity': producer}})
    put(directory / 'prepared.json', {'original': True})
    if record == 'completed':
        put(directory / 'staged-completed.json', {'original': True})
    elif record:
        put(directory / 'execution' / record, {'original': True})
    original = {p: p.read_bytes() for p in directory.rglob('*.json')}
    assert staged._supersede_unlaunched_finalization(directory) is (record is None)
    assert all(path.read_bytes() == data for path, data in original.items())
    if record is None:
        marker = json.loads((directory / 'finalization-retry.json').read_text())
        assert marker['reason'] == 'finalization preparation did not finish before launch'
        assert marker['producer_identity'] == producer
    else:
        assert not (directory / 'finalization-retry.json').exists()


def test_incomplete_preparation_restarts_only_finalizer_against_current_state(tmp_path, monkeypatch):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    owned = root / '1/acceptance/1'
    identity = 'c' * 64
    put(owned / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': {'read_nodes': ['A']}})
    partial = owned / 'finalization/2'
    put(partial / 'canonical.json', {'key': {'implementation_identity': 'intermediate'}})
    original = (partial / 'canonical.json').read_bytes()
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    calls = []
    async def execute(_driver, directory, _task, _identity, stage):
        calls.append(('execute', stage, directory.name))
        if stage == 'owned':
            return {'status': 'facts', 'facts': {'read_nodes': ['A']}}
        assert directory.name == '3'
        return {'status': 'checked', 'output': {'response': {'outcome': 'Valid'}},
                'candidate_sha256': identity, 'checked_sources': {}}
    monkeypatch.setattr(staged, '_execute', execute)
    def prepare(_driver, _task, _identity, directory, **kwargs):
        calls.append(('prepare', directory.name))
        assert kwargs['owned'] == owned and kwargs['checked']['status'] == 'facts'
        put(directory / 'canonical.json', {'key': {'candidate_sha256': identity}})
        put(directory / 'staged-donor.json', {'freshly_bound': True})
        return {'status': 'prepared'}
    monkeypatch.setattr(staged, '_prepare', prepare)
    driver = SimpleNamespace(root=root, finalization_gate=asyncio.Lock(),
        _publish=lambda *_args, **_kwargs: True)
    assert asyncio.run(staged.run(driver, 1, identity))
    assert calls == [('execute', 'owned', '1'), ('prepare', '3'), ('execute', 'finalize', '3')]
    assert (partial / 'canonical.json').read_bytes() == original
    assert (partial / 'finalization-retry.json').exists()


@pytest.mark.parametrize('producer', ['current', 'unreviewed-intermediate'])
def test_complete_unlaunched_preparation_does_not_gain_producer_compatibility(tmp_path, monkeypatch, producer):
    monkeypatch.setattr(staged.receipts, 'implementation_identity', lambda: 'current')
    directory = tmp_path / 'dispatch/74/acceptance/4/finalization/2'
    put(directory / 'canonical.json', {'key': {'implementation_identity': producer}})
    put(directory / 'staged-donor.json', {'existing': True})
    assert not staged._supersede_unlaunched_finalization(directory)
    assert not (directory / 'finalization-retry.json').exists()


def test_legacy_host_round_must_be_adopted_before_staged_adapter(tmp_path, monkeypatch):
    from trellis.dispatch import driver as driver_module
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    root = tmp_path / 'dispatch'
    identity = 'c' * 64
    put(root / '1/acceptance/1/canonical.json', {'key': {'candidate_sha256': identity}})
    from contextlib import nullcontext
    monkeypatch.setattr(staged, '_current', lambda *_: ({}, {}, {}))
    monkeypatch.setattr(staged, '_plan', lambda *_: {'status': 'planned'})
    monkeypatch.setattr(staged, '_within_reservation', lambda *_: None)
    driver = SimpleNamespace(root=root, native=SimpleNamespace(locked=nullcontext),
        _prepare=lambda *_: pytest.fail('unadopted legacy work must not create a workspace'))
    assert asyncio.run(staged.run(driver, 1, identity)) is False
    assert not (root / '1/acceptance/2').exists()


def test_legacy_plan_stays_unsupported_until_authenticated_completion(tmp_path, monkeypatch):
    from contextlib import nullcontext
    root = tmp_path / 'dispatch'
    identity = 'c' * 64
    put(root / '1/acceptance/1/canonical.json', {'key': {'candidate_sha256': identity}})
    monkeypatch.setattr(staged, '_current', lambda *_: ({}, {}, {}))
    monkeypatch.setattr(staged, '_plan', lambda *_: pytest.fail('live legacy process cannot use an owned footprint'))
    driver = SimpleNamespace(root=root, native=SimpleNamespace(locked=nullcontext))
    assert staged.plan(driver, 1, identity)['status'] == 'unsupported'


def test_interrupted_legacy_donor_marker_cannot_silently_launch_a_cold_owned_stage(tmp_path, monkeypatch):
    root = tmp_path / 'dispatch'
    directory = root / '1/acceptance/2'
    identity = 'c' * 64
    put(directory / 'canonical.json', {'legacy_donor': {'donor_round': '1', 'donor_completion_sha256': 'bound'}})
    monkeypatch.setattr(staged, '_select_legacy', lambda *_: None)
    with pytest.raises(candidate.CandidateError, match='preparation is incomplete'):
        staged._ensure_legacy_donor(SimpleNamespace(root=root), directory, 1, identity)
    assert not (directory / 'legacy-owner-donor.json').exists()


def test_completed_legacy_donor_enters_only_new_owned_stage_after_readmission(tmp_path, monkeypatch):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    identity = 'c' * 64
    put(root / '1/acceptance/1/canonical.json', {'key': {'candidate_sha256': identity}})
    native_plan = {'status': 'planned', 'read_nodes': ['A'], 'write_nodes': ['A']}
    monkeypatch.setattr(staged, '_select_legacy', lambda *_: {'donor_round': '1', 'donor_completion_sha256': 'bound'})
    monkeypatch.setattr(staged, 'plan', lambda *_: native_plan)
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    prepared = []
    def prepare(_driver, _task, _identity, directory, **_kwargs):
        prepared.append(directory)
        put(directory / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': native_plan})
        return {'status': 'prepared'}
    monkeypatch.setattr(staged, '_prepare', prepare)
    executed = []
    async def execute(_driver, directory, _task, _identity, stage):
        executed.append((directory, stage))
        return ({'status': 'facts', 'facts': {'read_nodes': ['A']}} if stage == 'owned'
                else {'status': 'checked', 'output': {'response': {'outcome': 'Valid'}},
                      'candidate_sha256': identity, 'checked_sources': {}})
    monkeypatch.setattr(staged, '_execute', execute)
    driver = SimpleNamespace(root=root, finalization_gate=asyncio.Lock(),
                             _publish=lambda *_args, **_kwargs: True)
    assert asyncio.run(staged.run(driver, 1, identity))
    assert executed[0] == (root / '1/acceptance/2', 'owned')
    assert executed[1] == (root / '1/acceptance/2/finalization/1', 'finalize')
    assert root / '1/acceptance/1' not in prepared


@pytest.mark.parametrize('failed_stage', ['owned', 'finalize'])
def test_infrastructure_retry_uses_existing_budget_and_preserves_successful_proof(tmp_path, monkeypatch, failed_stage):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    identity = 'c' * 64
    native_plan = {'status': 'planned', 'read_nodes': ['A'], 'write_nodes': ['A']}
    owned = root / '1/acceptance/1'
    put(owned / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': native_plan})
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    monkeypatch.setattr(staged, 'plan', lambda *_: native_plan)
    def prepare(_driver, _task, _identity, directory, **_kwargs):
        put(directory / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': native_plan})
        return {'status': 'prepared'}
    monkeypatch.setattr(staged, '_prepare', prepare)
    calls = []
    failed = []
    async def execute(_driver, directory, _task, _identity, stage):
        calls.append((stage, directory))
        if stage == failed_stage and not failed:
            failed.append(True)
            raise driver_module.HostCheckFailure('host process exited with infrastructure error')
        return ({'status': 'facts', 'facts': {'read_nodes': ['A']}} if stage == 'owned'
                else {'status': 'checked', 'output': {'response': {'outcome': 'Valid'}},
                      'candidate_sha256': identity, 'checked_sources': {}})
    monkeypatch.setattr(staged, '_execute', execute)
    retries = []
    def retry(task, digest, directory, reason):
        retries.append(directory)
        put(directory / 'retry-authorized.json', {'existing_native_budget': 1})
    driver = SimpleNamespace(root=root, finalization_gate=asyncio.Lock(), _retry_check=retry,
                             _publish=lambda *_args, **_kwargs: True)
    driver._handle_check_failure = lambda *args: driver_module.ProductionDriver._handle_check_failure(driver, *args)
    assert asyncio.run(staged.run(driver, 1, identity))
    assert len(retries) == 1
    owner_calls = [directory for stage, directory in calls if stage == 'owned']
    if failed_stage == 'owned':
        assert owner_calls == [owned, owned.parent / '2']
    else:
        assert owner_calls == [owned]
        assert [p.name for stage, p in calls if stage == 'finalize'] == ['1', '2']


def test_independent_proofs_overlap_while_shared_consumer_finalization_serializes(tmp_path, monkeypatch):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    identity = 'c' * 64
    for task, owner in [(1, 'A'), (2, 'B')]:
        put(root / f'{task}/acceptance/1/canonical.json', {'key': {'candidate_sha256': identity},
            'staged_plan': {'read_nodes': [owner], 'write_nodes': [owner]}})
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    def prepare(_driver, task, _identity, directory, **_kwargs):
        put(directory / 'canonical.json', {'key': {'candidate_sha256': identity}})
        return {'status': 'prepared'}
    monkeypatch.setattr(staged, '_prepare', prepare)
    async def exercise():
        both_checking = asyncio.Event()
        proof_active = set()
        finalized = []
        final_active = []
        async def execute(_driver, _directory, task, _identity, stage):
            owner = 'A' if task == 1 else 'B'
            if stage == 'owned':
                proof_active.add(task)
                if len(proof_active) == 2:
                    both_checking.set()
                await both_checking.wait()
                return {'status': 'facts', 'facts': {'read_nodes': [owner]}}
            assert not final_active
            final_active.append(task)
            await asyncio.sleep(0)
            assert final_active == [task]
            final_active.pop()
            finalized.append(task)
            return {'status': 'checked', 'output': {'response': {'outcome': 'Valid', 'shared_consumer': 'Root'}},
                    'candidate_sha256': identity, 'checked_sources': {}}
        monkeypatch.setattr(staged, '_execute', execute)
        driver = SimpleNamespace(root=root, finalization_gate=asyncio.Lock(),
                                 _publish=lambda *_args, **_kwargs: True)
        assert await asyncio.wait_for(asyncio.gather(staged.run(driver, 1, identity), staged.run(driver, 2, identity)), 2) == [True, True]
        assert set(finalized) == {1, 2}
    asyncio.run(exercise())


@pytest.mark.parametrize('change', ['unsupported_policy', 'new_import', 'resolved'])
def test_new_owner_preparation_rechecks_current_inputs_before_any_mutation(tmp_path, monkeypatch, change):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    identity = 'c' * 64
    put(root / '1/acceptance-admission.json', {
        'footprint': {'reads': ['H'], 'writes': ['A'], 'exclusive': False}})
    task = {'id': 1, 'status': {'status': 'checking', 'candidate_sha256': identity}}
    state = {'dispatch': {'tasks': {'1': task}}, 'policy': 'previously-supported'}
    locked = []
    @contextmanager
    def lock():
        locked.append(True)
        try:
            yield
        finally:
            locked.pop()
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    # The earlier admission has already happened. A policy/import/status
    # change before preparation must not use that previous support decision.
    state['policy'] = change
    if change == 'resolved':
        task['status'] = {'status': 'resolved'}
    current_source = {'graph': {'edges': {'A': ['NewImport'] if change == 'new_import' else ['H']}}}
    def native(payload):
        assert locked and payload['state'] is state
        assert payload['action'] == 'dispatch_acceptance_state'
        return {'policy': payload['state']['policy']}
    def plan(_driver, _task, source, private, _identity):
        assert locked and private['policy'] == change and source == current_source
        if change == 'unsupported_policy':
            return {'status': 'unsupported', 'reason': 'current policy excludes staging'}
        return {'status': 'planned', 'read_nodes': ['A', 'NewImport'], 'write_nodes': ['A']}
    monkeypatch.setattr(staged, '_plan', plan)
    driver = SimpleNamespace(root=root, repo=tmp_path / 'repo', state=lambda: state,
        require_no_halt=lambda: None, native=SimpleNamespace(locked=lock,
            _require_recovered_publication=lambda: None, observe_sources=lambda *_: current_source,
            _call=native), _prepare=lambda *_: pytest.fail('stale admission must not create a workspace'))
    if change == 'new_import':
        with pytest.raises(candidate.CandidateError, match='exceed the retained source reservation'):
            asyncio.run(staged.run(driver, 1, identity))
    else:
        assert asyncio.run(staged.run(driver, 1, identity)) is (change == 'resolved')
    assert not (root / '1/acceptance').exists()
    assert not locked


@pytest.mark.parametrize('exhausted', [False, True])
def test_owned_capture_retry_uses_existing_budget_and_fresh_owned_plan(tmp_path, monkeypatch, exhausted):
    from trellis.dispatch import driver as driver_module
    root = tmp_path / 'dispatch'
    identity = 'c' * 64
    owned = root / '1/acceptance/1'
    plan = {'read_nodes': ['A'], 'write_nodes': ['A'], 'owner_nodes':['A']}
    put(owned / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': plan})
    async def inline(function, *args, **kwargs): return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    calls = []
    def prepare(_driver, _task, _identity, directory, **kwargs):
        calls.append(('prepare', directory))
        put(directory / 'canonical.json', {'key': {'candidate_sha256': identity}, 'staged_plan': plan})
        return {'status':'prepared'}
    monkeypatch.setattr(staged, '_prepare', prepare)
    async def execute(_driver, directory, _task, _identity, stage):
        calls.append((stage, directory))
        if directory == owned:
            return {'stage':'owned', 'status':'cold', 'reason':'capture failed', 'retry_class':'owned_environment_capture'}
        if stage == 'owned': return {'status':'facts', 'facts':{'read_nodes':['A']}}
        return {'status':'checked', 'output':{'response':{'outcome':'Valid'}},
                'candidate_sha256':identity, 'checked_sources':{}}
    monkeypatch.setattr(staged, '_execute', execute)
    monkeypatch.setattr(staged, '_retryable_owned', lambda directory, output: directory == owned)
    def retry(task, sha, directory, reason):
        assert task == 1 and sha == identity and directory == owned
        calls.append(('retry', directory))
        if exhausted: raise RuntimeError('host check retry allowance exhausted; submission retained')
        put(directory / 'retry-authorized.json', {'bounded':True})
    driver = SimpleNamespace(root=root, finalization_gate=asyncio.Lock(), _retry_check=retry,
                             _publish=lambda *a, **k: True)
    if exhausted:
        with pytest.raises(RuntimeError, match='retry allowance exhausted'):
            asyncio.run(staged.run(driver, 1, identity))
        assert calls == [('owned', owned), ('retry', owned)]
    else:
        assert asyncio.run(staged.run(driver, 1, identity))
        assert [p for kind, p in calls if kind == 'owned'] == [owned, owned.parent / '2']
        assert [p for kind, p in calls if kind == 'finalize'] == [owned.parent / '2/finalization/1']


@pytest.mark.parametrize('used', [0, 1, 2])
@pytest.mark.parametrize('stage', ['owned', 'finalize'])
def test_owned_capture_recovery_keeps_actual_driver_native_budget_and_worker_allowance(tmp_path, used, stage):
    from contextlib import nullcontext
    from copy import deepcopy
    from trellis.dispatch.driver import ProductionDriver
    identity = 'c' * 64
    directory = tmp_path / 'dispatch/1/acceptance/4'
    if stage == 'finalize':
        directory = directory / 'finalization/1'
    attempt = {'infrastructure_retries':used, 'turn':1,
               'phase':{'phase':'checking', 'candidate_sha256':identity},
               'policy':{'max_infrastructure_retries':2, 'max_submissions':3}}
    state = {'dispatch':{'tasks':{'1':{'id':1, 'status':{'status':'checking', 'candidate_sha256':identity},
                                     'worker_attempt':attempt}}}}
    before = deepcopy(state)
    calls, pauses = [], []
    native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None,
                             _call=calls.append)
    driver = SimpleNamespace(native=native, state=lambda:state, runtime=tmp_path, root=tmp_path / 'dispatch',
                             _pause=lambda *a, **kw:pauses.append((a,kw)))
    if used == 2:
        # This budget-only fixture has no authenticated completion. It must
        # remain a global evidence failure, never a task-local parked verdict.
        with pytest.raises(FileNotFoundError):
            ProductionDriver._retry_check(driver, 1, identity, directory, 'capture unavailable')
        assert calls == [] and pauses == []  # Coordinator handles the propagated global error.
        assert not (directory / 'retry-authorized.json').exists()
    else:
        ProductionDriver._retry_check(driver, 1, identity, directory, 'capture unavailable')
        assert calls == [{'action':'dispatch_control', 'root':str(tmp_path),
                          'event':{'kind':'check_retry', 'task_id':1, 'candidate_sha256':identity, 'retry':used + 1}}]
        assert json.loads((directory / 'retry-authorized.json').read_text())['kind'] == 'bounded_infrastructure_retry'
    assert state == before  # Only native dispatch_control may advance the counter.
    assert json.loads((directory / 'infrastructure-failure.json').read_text())['event']['retry'] == used + 1


@pytest.mark.parametrize('exhausted', [False, True])
def test_foreign_owner_refresh_retains_old_resource_failure_and_uses_native_budget(tmp_path, monkeypatch, exhausted):
    from contextlib import nullcontext
    from trellis.dispatch.driver import ProductionDriver
    from trellis.dispatch import parking
    root = tmp_path / 'dispatch'
    identity = 'c' * 64
    owned = root / '1/acceptance/1'
    plan = {'read_nodes': ['A'], 'write_nodes': ['A'], 'owner_nodes': ['A']}
    put(owned / 'canonical.json', {'key': {'candidate_sha256': identity,
        'implementation_identity': 'older-unreviewed-build'}, 'staged_plan': plan})
    old = owned / 'finalization/1/runtime/checker-state'
    put(old / 'memory-outcome.json', {'kind': 'resource_rejected', 'detail': 'x' * 41000})
    put(old / 'resource-failure.json', {'reason': 'internal_error: host capacity evidence exceeds its bound'})
    evidence = {p: p.read_bytes() for p in old.iterdir()}
    monkeypatch.setattr(staged.receipts, 'implementation_identity', lambda: 'current-build')
    monkeypatch.setattr(staged.stages, 'reviewed_owned_producers', lambda: {'reviewed-build'})
    task = dict(id=1, status=dict(status='checking', candidate_sha256=identity),
        worker_attempt=dict(infrastructure_retries=2 if exhausted else 0, turn=1,
            phase=dict(phase='checking', candidate_sha256=identity),
            policy=dict(max_infrastructure_retries=2, max_submissions=3)))
    calls = []
    def native(payload):
        assert payload['action'] == 'dispatch_control'
        assert payload['event'] == dict(kind='check_retry', task_id=1, candidate_sha256=identity, retry=1)
        task['worker_attempt']['infrastructure_retries'] = 1
        calls.append('native-retry')
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root, driver.runtime = root, tmp_path
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None, _call=native)
    driver.state = lambda: {'dispatch': {'tasks': {'1': task}}}
    driver.finalization_gate = asyncio.Lock()
    driver._publish = lambda *a, **k: True
    def prepare(_driver, _task, sha, directory, **kwargs):
        assert sha == identity and directory != owned
        put(directory / 'canonical.json', {'key': {'candidate_sha256': identity,
            'implementation_identity': 'current-build'}, 'staged_plan': plan})
        return dict(status='prepared')
    monkeypatch.setattr(staged, '_prepare', prepare)
    async def execute(_driver, directory, _task, sha, stage):
        assert sha == identity
        calls.append((stage, directory))
        if stage == 'owned':
            return dict(status='facts', facts=dict(read_nodes=['A']))
        assert directory == owned.parent / '2/finalization/1'
        return dict(status='checked', output=dict(response=dict(outcome='Valid')),
                    candidate_sha256=identity, checked_sources={})
    monkeypatch.setattr(staged, '_execute', execute)
    def block(*args):
        raise RuntimeError('host retry allowance exhausted')
    monkeypatch.setattr(parking, 'block', block)
    if exhausted:
        with pytest.raises(RuntimeError, match='retry allowance exhausted'):
            asyncio.run(staged.run(driver, 1, identity))
        assert calls == [('owned', owned)]
        assert not (owned.parent / '2').exists()
    else:
        assert asyncio.run(staged.run(driver, 1, identity))
        assert calls == [('owned', owned), 'native-retry',
            ('owned', owned.parent / '2'), ('finalize', owned.parent / '2/finalization/1')]
        assert json.loads((owned / 'retry-authorized.json').read_text())['kind'] == 'bounded_infrastructure_retry'
        assert task['worker_attempt']['turn'] == 1
        assert task['status'] == dict(status='checking', candidate_sha256=identity)
    assert all(p.read_bytes() == data for p, data in evidence.items())


@pytest.mark.parametrize('producer', ['current-build', 'reviewed-build'])
def test_current_or_reviewed_owner_does_not_spend_refresh_budget(tmp_path, monkeypatch, producer):
    put(tmp_path / 'canonical.json', {'key': {'implementation_identity': producer}})
    monkeypatch.setattr(staged.receipts, 'implementation_identity', lambda: 'current-build')
    monkeypatch.setattr(staged.stages, 'reviewed_owned_producers', lambda: {'reviewed-build'})
    assert not staged._requires_fresh_owner(tmp_path)

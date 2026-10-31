import asyncio
import copy
import json
from pathlib import Path
from types import SimpleNamespace
from contextlib import nullcontext

import pytest

from trellis.dispatch import candidate, acceptance_receipts as receipts
from trellis.dispatch.driver import ProductionDriver, acceptance_identity
from trellis.dispatch.publication import atomic


def put(path, value):
    atomic(path, candidate.canonical_json(value))


@pytest.fixture
def completed(tmp_path, monkeypatch):
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'test-implementation')
    # Tests isolate receipt logic; these exact input maps stand for the native
    # read-only verifier inventory (the real Lean handoff test uses real hashes).
    monkeypatch.setattr(receipts, 'input_hashes', lambda repo: {
        'lakefile.toml': candidate.digest((repo / 'lakefile.toml').read_bytes())})
    root = tmp_path / 'runtime/dispatch'
    directory = root / '1/acceptance/1'
    repo = directory / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('theorem A : True := by trivial\n')
    (repo / 'lakefile.toml').write_text('fixed build')
    (repo / 'trellis.config.json').write_text('{}')
    sources = candidate.source_manifest(repo)
    sealed = root / '1/candidate'
    identity = candidate.seal(repo, sealed, task_id=1, base_manifest=sources,
        inputs=sources, response={'outcome': 'Valid'})
    private = dict(request_seq=7, in_flight_request=dict(id=7, cycle=2, profile='sol'), gate='authoritative')
    task = dict(id=1, assignment=dict(profile='sol', compiler='managed', scope={'nodes': ['A']}),
        binding={'model': 'test-model'}, status={'status': 'checking', 'candidate_sha256': identity})
    source = dict(input_hashes=sources, manifest=sources, graph={'edges': {'A': []}})
    key = receipts.key(1, identity, task, acceptance_identity(private), source)
    config_sha = candidate.digest(b'original config')
    put(directory / 'canonical.json', dict(key=key, source=source,
        acceptance_sha256=acceptance_identity(private), config_sha256=config_sha))
    put(directory / 'runtime/protocol_state.json', private)
    put(directory / 'runtime/runtime_metadata.json', {'repo_path': str(repo)})
    put(directory / 'base-manifest.json', sources)
    put(directory / 'prepared.json', dict(identity=candidate.digest(candidate.canonical_json(
        dict(state=private, sources=sources, config_sha256=config_sha)))))
    process = dict(pid=99999999, start_ticks=1, boot_id='expired-host-process')
    for name in ('process', 'permit', 'cleanup'):
        put(directory / f'execution/{name}.json', process)
    put(directory / 'execution/recipe.json', dict(command=['python3', '-m', 'trellis.dispatch.acceptance'],
        payload=dict(config_path=str(repo / 'trellis.config.json'), runtime_root=str(directory / 'runtime'),
            candidate=str(sealed), captured_inputs=sources),
        checker=dict(runtime=str(root.parent), task_runtime=str(directory / 'runtime'), compiler='managed',
            scope='1/acceptance/1')))
    checked = dict(candidate_sha256=identity, response=dict(status='Ok', outcome='Valid'),
        checked_sources=receipts.checked_sources(repo))
    put(directory / 'execution/response.json', checked)
    put(directory / 'execution/result.json', dict(returncode=0,
        response_sha256=candidate.digest((directory / 'execution/response.json').read_bytes())))
    receipts.complete(directory, checked)
    return SimpleNamespace(directory=directory, root=root, repo=repo, task=task, source=source,
        key=key, checked=checked, identity=identity, private=private)


def test_exact_key_completed_acceptance_reused_without_new_launcher(completed):
    f = completed
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = f.root; driver.repo = f.repo
    driver.state = lambda: {'dispatch': {'tasks': {'1': f.task}}}
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None,
        observe_sources=lambda *_: f.source, _call=lambda _: f.private)
    driver._prepare = lambda *_: pytest.fail('exact current completion must not prepare another workspace')
    directory, task, captured, checked = driver._prepare_acceptance(1, 2, f.identity, {'1': 1})
    assert directory == f.directory and checked == f.checked
    assert task == f.task and captured == f.source['input_hashes']


@pytest.mark.parametrize("compact", [False, True])
def test_archived_terminal_acceptance_retains_original_authenticated_bytes(completed, monkeypatch, compact):
    from trellis.dispatch import retained_files as retained
    f = completed
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    path = f.directory / 'runtime/protocol_state.json'
    if compact:
        from trellis.history_artifacts import encode_shared_state
        put(path, encode_shared_state({'state': f.private}))
    original = path.read_bytes() + b' ' * 10000
    path.write_bytes(original)
    # Prepare a normally authenticated fixture whose whitespace compresses.
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    before = receipts.authenticate(f.directory, f.key)
    assert retained.archive(path)
    assert receipts.authenticate(f.directory, f.key) == before
    assert receipts.reusable(f.directory, f.key) == f.checked
    assert retained.restore(path)
    assert path.read_bytes() == original
    assert receipts.authenticate(f.directory, f.key) == before


@pytest.mark.parametrize('damage', ['truncated', 'null_manifest', 'list_manifest'])
def test_corrupt_archive_is_not_a_reusable_acceptance(completed, monkeypatch, damage):
    from trellis.dispatch import retained_files as retained
    f = completed
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    path = f.directory / 'runtime/protocol_state.json'
    path.write_bytes(path.read_bytes() + b' ' * 10000)
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    assert retained.archive(path)
    archive, metadata = retained._paths(path)
    if damage == 'truncated':
        archive.write_bytes(archive.read_bytes()[:-5])
    else:
        metadata.write_text('null' if damage == 'null_manifest' else '[]')
    assert receipts.reusable(f.directory, f.key) is None


def test_receipt_separately_binds_host_authority_inputs(completed):
    f = completed
    source = dict(f.source, authority_inputs={'.trellis/scripts/guard': 'bound-tool'})
    bound = receipts.key(1, f.identity, f.task, acceptance_identity(f.private), source)
    assert bound['input_hashes'] == f.source['input_hashes']
    assert bound['authority_input_hashes'] == source['authority_inputs']
    captured = json.loads((f.directory / 'canonical.json').read_text())
    captured.update(key=bound, source=source)
    put(f.directory / 'canonical.json', captured)
    receipts.authenticate(f.directory, bound)
    captured = json.loads((f.directory / 'canonical.json').read_text())
    captured['source']['authority_inputs']['.trellis/scripts/guard'] = 'changed-tool'
    put(f.directory / 'canonical.json', captured)
    with pytest.raises(candidate.CandidateError, match='authority inputs disagree'):
        receipts.authenticate(f.directory, bound)


def test_exact_reuse_still_enters_native_publication_gates(completed):
    f = completed
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = f.root; driver.repo = f.repo
    driver.require_no_halt = lambda: None; driver.now_ms = lambda: 100
    driver.state = lambda: {'dispatch': {'tasks': {'1': f.task}}}
    driver.publisher = SimpleNamespace(locked=nullcontext,
        prepare=lambda *_args, **_kwargs: pytest.fail('native gate must run first'))
    calls = []
    def prepare(payload):
        calls.append(payload)
        raise RuntimeError('ordinary native publication gate rejected')
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        observe_sources=lambda *_args, **_kwargs: f.source, _call=lambda _: f.private,
        prepare_publication=prepare)
    checked = receipts.reusable(f.directory, f.key)
    with pytest.raises(RuntimeError, match='native publication gate rejected'):
        driver._publish(1, f.identity, f.directory, checked)
    assert len(calls) == 1 and calls[0]['response']['outcome'] == 'Valid'
    assert calls[0]['inputs'] == f.key['input_hashes']


@pytest.mark.parametrize('status', ['rebased', 'cold', 'cold_bad_diagnostic', 'cold_extra_diagnostic', 'cold_overlimit_diagnostic', 'integrity_error'])
@pytest.mark.parametrize('staged', [False, True])
@pytest.mark.parametrize("compact", [False, True])
def test_semantic_rebase_authenticates_original_and_supplies_both_complete_source_views(completed, status, staged, compact):
    f = completed
    if compact:
        from trellis.history_artifacts import encode_shared_state
        put(f.directory / "runtime/protocol_state.json", encode_shared_state({"state": f.private}))
        (f.directory / "completed.json").unlink()
        receipts.complete(f.directory, f.checked)
    driver = ProductionDriver.__new__(ProductionDriver)
    if staged:
        # Staged carrier authentication has its own receipt tests. This test
        # exercises selection of the native gate after that authentication.
        driver._completed_output = lambda *_args, **_kwargs: f.checked
    driver.config_path = f.root / 'current-config'
    driver.config_path.write_bytes(b'original config')
    private = dict(f.private, worker_execution={'task_id': 1}, latest_worker_summary='current sibling report')
    source = copy.deepcopy(f.source)
    if not staged:
        source['input_hashes']['Tablet/B.lean'] = 'new sibling proof'
    current_key = receipts.key(1, f.identity, f.task, acceptance_identity(private), source)
    captured = json.loads((f.directory / 'canonical.json').read_text())
    calls = []
    response = dict(f.checked['response'], snapshot={'current': 'native merge'})
    effective_status = 'cold' if status.startswith('cold_') else status
    attempts = [dict(lane='ordinary_context', reason='context changed', truncated=False),
                dict(lane='scoped_response_checks', reason='semantic input changed', truncated=False)]
    def native(payload):
        calls.append(payload)
        assert payload['action'] == ('dispatch_rebase_staged_finalization' if staged
                                     else 'dispatch_rebase_checked_response')
        assert payload['original_private'] == f.private
        assert payload['current_private'] == private
        assert payload['original_source'] == f.source
        assert payload['current_source'] == source
        assert payload['checked_response'] == f.checked['response']
        assert payload['write_nodes'] == ['A']
        if status == 'integrity_error':
            return dict(status=status, reason='inconsistent retained certificate', operand='current',
                        integrity=dict(tier='committed', node='A', field='toolchain_root', recorded='bad', expected='bound'))
        if effective_status == 'cold':
            diagnostic_attempts = attempts
            if status == 'cold_bad_diagnostic':
                diagnostic_attempts = [{'reason': object()}]
            elif status == 'cold_extra_diagnostic':
                diagnostic_attempts = [dict(row, extra=object(), large='x' * 10000) for row in attempts]
            elif status == 'cold_overlimit_diagnostic':
                diagnostic_attempts = attempts + attempts[:1]
            return dict(status='cold', reason='semantic input changed',
                        attempts=diagnostic_attempts)
        return dict(status=status, **({'response': response} if status == 'rebased' else {'reason': 'semantic input changed'}))
    driver.native = SimpleNamespace(_call=native)
    before = {name: (f.directory / name).read_bytes() for name in (*receipts._RECEIPTS, 'completed.json')}
    if status == 'integrity_error':
        with pytest.raises(candidate.CandidateError, match='retain original completion without another finalizer'):
            driver._rebase_acceptance(f.directory, captured, current_key, private, source, f.checked, f.task,
                                     staged=staged)
    else:
        result = driver._rebase_acceptance(f.directory, captured, current_key, private, source, f.checked, f.task,
                                          staged=staged)
        assert result == (dict(f.checked, response=response) if status == 'rebased' else None)
    assert len(calls) == 1
    assert all((f.directory / name).read_bytes() == value for name, value in before.items())
    diagnostic = json.loads(next((f.directory / 'semantic-rebase').glob('*.json')).read_text())
    assert diagnostic['status'] == effective_status
    if status in ('cold', 'cold_extra_diagnostic'):
        assert diagnostic['attempts'] == attempts
    elif status in ('cold_bad_diagnostic', 'cold_overlimit_diagnostic'):
        assert 'attempts' not in diagnostic
    if status == 'integrity_error':
        assert diagnostic['operand'] == 'current'
        assert diagnostic['integrity']['field'] == 'toolchain_root'
        assert not (f.directory / 'finalization-retry.json').exists()
    assert 'new sibling proof' not in json.dumps(diagnostic)


@pytest.mark.parametrize('fault', ['config', 'implementation', 'task_binding', 'host_response'])
def test_semantic_rebase_never_uses_changed_producer_or_unauthenticated_completion(completed, fault):
    f = completed
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.config_path = f.root / 'current-config'
    driver.config_path.write_bytes(b'changed config' if fault == 'config' else b'original config')
    private = dict(f.private, worker_execution={'task_id': 1})
    current_key = receipts.key(1, f.identity, f.task, acceptance_identity(private), f.source)
    if fault == 'implementation':
        current_key['implementation_identity'] = 'different producer'
    if fault == 'task_binding':
        current_key['task_binding_sha256'] = 'different frozen task'
    captured = json.loads((f.directory / 'canonical.json').read_text())
    checked = dict(f.checked, forged=True) if fault == 'host_response' else f.checked
    driver.native = SimpleNamespace(_call=lambda _: pytest.fail('native rebase cannot bypass receipt/key guards'))
    if fault == 'host_response':
        with pytest.raises(candidate.CandidateError, match='completion changed'):
            driver._rebase_acceptance(f.directory, captured, current_key, private, f.source, checked, f.task)
    else:
        assert driver._rebase_acceptance(f.directory, captured, current_key, private, f.source, checked, f.task) is None


def test_staged_admin_rebase_keeps_checked_helper_graph_for_native_publication(completed):
    f = completed
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = f.root; driver.repo = f.repo
    driver.require_no_halt = lambda: None; driver.now_ms = lambda: 100
    private = dict(f.private, request_seq=8, in_flight_request=dict(id=8, cycle=2, profile='sol'))
    driver.state = lambda: {'dispatch': {'tasks': {'1': f.task}}}
    driver._rebase_acceptance = lambda *_args, **_kwargs: f.checked
    driver._completed_output = lambda *_args, **_kwargs: f.checked
    driver.publisher = SimpleNamespace(locked=nullcontext,
        prepare=lambda *_args, **_kwargs: pytest.fail('native publication must authorize helper changes first'))
    proposed = dict(f.source, graph={'edges': {'A': ['A_Helper'], 'A_Helper': []}})
    calls = []
    def prepare(payload):
        calls.append(payload)
        raise RuntimeError('native gate reached with checked helper graph')
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        observe_sources=lambda *_args, **kwargs: proposed if 'response' in kwargs else f.source,
        _call=lambda _: private, prepare_publication=prepare)
    with pytest.raises(RuntimeError, match='native gate reached with checked helper graph'):
        driver._publish(1, f.identity, f.directory, f.checked, staged=True)
    assert calls[0]['graph_after'] == proposed['graph']
    assert calls[0]['response']['request_id'] == 8


@pytest.fixture
def refreshable(completed):
    f = completed
    context = dict(request=f.private['in_flight_request'], baseline='host-observed')
    environment = dict(version=1, environment_sha256s={'A': 'bound-import-graph'},
                       artifact_files={str(f.repo / 'Tablet/A.lean'): 'artifact-digest'})
    context['baseline_environment'] = environment
    put(f.directory / 'runtime/acceptance-context.json', context)
    f.checked.update(acceptance_context_sha256=candidate.digest(
        (f.directory / 'runtime/acceptance-context.json').read_bytes()),
        final_environment=environment)
    put(f.directory / 'execution/response.json', f.checked)
    put(f.directory / 'execution/result.json', dict(returncode=0,
        response_sha256=candidate.digest((f.directory / 'execution/response.json').read_bytes())))
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    f.context = context
    f.environment = environment
    return f


def test_refresh_preparation_authenticates_original_before_environment_capture(refreshable, monkeypatch):
    from trellis.dispatch import publication_refresh as refresh
    f = refreshable
    calls = []
    def capture(runtime, owners, *, task_runtime=None):
        calls.append((runtime, sorted(owners), task_runtime))
        return dict(environment=f.environment)
    monkeypatch.setattr(refresh, '_capture', capture)
    original = {name: (f.directory / name).read_bytes() for name in receipts._RECEIPTS}
    evidence = refresh.prepare(f.root.parent, f.directory, f.checked)
    assert evidence['status'] == 'prepared'
    assert evidence['original_acceptance_context'] == f.context
    assert calls == [(f.root.parent, ['A'], None), (f.root.parent, ['A'], f.directory / 'runtime')]
    assert all((f.directory / name).read_bytes() == data for name, data in original.items())
    calls.clear()
    (f.directory / 'runtime/acceptance-context.json').write_text('{}')
    assert refresh.prepare(f.root.parent, f.directory, f.checked)['status'] == 'cold'
    assert calls == []


def test_refresh_keeps_completed_history_and_uses_fresh_native_request(refreshable, monkeypatch):
    from trellis.dispatch import publication_refresh as refresh
    f = refreshable
    monkeypatch.setattr(refresh, '_capture', lambda *_args, **_kwargs: dict(environment=f.environment))
    evidence = refresh.prepare(f.root.parent, f.directory, f.checked)
    old = (f.directory / 'completed.json').read_bytes()
    request = dict(f.private['in_flight_request'], id=11, cycle=3)
    current_key = dict(f.key, acceptance_sha256='fresh-context')
    fresh = dict(ok=True, response=dict(status='Ok', outcome='Valid', request_id=11, cycle=3))
    def kernel(payload):
        assert payload['action'] == 'try_refresh_trellis_worker_result'
        assert payload['request'] == request
        assert payload['original_checked'] == f.checked
        assert payload['original_acceptance_context'] == f.context
        assert payload['baseline_repo_path'] == str(f.repo)
        return dict(status='try_refresh_trellis_worker_result_ok', output=dict(
            status='refreshed', output=fresh, refresh_receipt={'version': 1}))
    result = refresh.apply(f.directory, evidence, current_key=current_key, request=request, repo=f.repo, kernel=kernel)
    assert result['output'] == fresh
    assert (f.directory / 'completed.json').read_bytes() == old
    recorded = json.loads(next((f.directory / 'refreshes').iterdir()).read_text())
    assert recorded['donor_completion_sha256'] == candidate.digest(old)
    assert recorded['current_key'] == current_key
    # A source publication or task/candidate change cannot use the context-only route.
    changed = dict(current_key, input_hashes={'Tablet/A.lean': 'changed'})
    assert refresh.apply(f.directory, evidence, current_key=changed, request=request, repo=f.repo,
        kernel=lambda _: pytest.fail('changed source must stay cold'))['status'] == 'cold'


@pytest.mark.parametrize('missing', [None, 'final_environment', 'acceptance_context_sha256'])
def test_context_refresh_releases_publication_lock_before_capture(refreshable, monkeypatch, missing):
    from contextlib import contextmanager
    from trellis.dispatch import publication_refresh as refresh
    f = refreshable
    locked = []
    @contextmanager
    def lock():
        locked.append(True)
        try: yield
        finally: locked.pop()
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = f.root; driver.repo = f.repo
    driver.require_no_halt = lambda: None; driver.now_ms = lambda: 100
    driver.state = lambda: {'dispatch': {'tasks': {'1': f.task}}}
    driver.publisher = SimpleNamespace(locked=lock)
    private = dict(f.private, latest_worker_summary='Another task completed')
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        observe_sources=lambda *_args, **_kwargs: f.source, _call=lambda _: private,
        kernel=lambda _: pytest.fail('no native refresh until outside-lock capture'))
    monkeypatch.setattr(refresh, '_capture', lambda *_args, **_kwargs:
        pytest.fail('environment capture must not run inside _publish'))
    rejected = []
    monkeypatch.setattr(receipts, 'reject', lambda *args, **kwargs: rejected.append(True))
    checked = dict(f.checked)
    if missing:
        checked.pop(missing)
    assert driver._publish(1, f.identity, f.directory, checked) is (False if missing else None)
    if missing:
        diagnostic = json.loads(next((f.directory / 'refresh-misses').glob('*.json')).read_text())
        expected = ('original final artifact environment unavailable' if missing == 'final_environment'
                    else 'original acceptance context binding unavailable')
        assert diagnostic['reason'] == expected and rejected == [True]
    else:
        assert not rejected
    assert not locked


@pytest.mark.parametrize('cleanup_fails', [False, True])
def test_refresh_capture_requires_drain_and_revokes_its_exact_channel(tmp_path, monkeypatch, cleanup_fails):
    from trellis.atomic_actions import checker_client
    from trellis.checker import workspaces
    from trellis.dispatch import publication_refresh as refresh, resources
    capability = dict(TRELLIS_CHECKER_SOCKET=str(tmp_path / 'sockets/checker.sock'),
                      TRELLIS_CHECKER_TOKEN='private-host-capture')
    events = []
    monkeypatch.setattr(workspaces, 'register', lambda *_args, **_kwargs: capability)
    monkeypatch.setattr(checker_client, '_round_trip', lambda *_args, **_kwargs: {'environment': 'observed'})
    def drain(env):
        assert env == capability
        events.append('drain')
        if cleanup_fails:
            raise RuntimeError('compiler cleanup was not acknowledged')
    monkeypatch.setattr(resources, 'drain_channels', drain)
    monkeypatch.setattr(workspaces, 'revoke', lambda runtime, token:
        events.append(('revoke', runtime, token)))
    if cleanup_fails:
        with pytest.raises(RuntimeError, match='cleanup was not acknowledged'):
            refresh._capture(tmp_path, ['A'], task_runtime=tmp_path / 'dispatch/1/runtime')
    else:
        assert refresh._capture(tmp_path, ['A'], task_runtime=tmp_path / 'dispatch/1/runtime') == {'environment': 'observed'}
    assert events == ['drain', ('revoke', tmp_path, 'private-host-capture')]


@pytest.fixture(params=[None, 1])
def stuck_completed(completed, request):
    f = completed
    # The native-source stub below reports this unchanged canonical baseline.
    # Materialize those bytes too: stale-negative guards read actual sources.
    canonical = f.root.parent / 'canonical'
    for name, digest in f.source['manifest'].items():
        data = (f.repo / name).read_bytes()
        assert candidate.digest(data) == digest
        target = canonical / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    # Production shape: the worker retained a partial edit; a full host round
    # already checked it, but no part of that candidate will be published.
    (f.repo / 'Tablet/A.lean').write_text('theorem A : True := by\n  sorry\n')
    f.raw = dict(outcome='stuck', summary='Partial route retained', comments='Missing lemma', deleted_nodes=[])
    sealed = f.root / '1/turns/1/candidate' if request.param else f.root / '1/stuck-candidate'
    f.identity = candidate.seal(f.repo, sealed, task_id=1, base_manifest=f.source['manifest'],
        inputs=f.source['input_hashes'], response=f.raw,
        submission={'turn': 1} if request.param else None)
    if not request.param:
        (f.root / '1/candidate').rename(f.root / '1/old-candidate')
        sealed.rename(f.root / '1/candidate')
        sealed = f.root / '1/candidate'
    f.sealed = sealed
    f.task['assignment'].update(task_kind='lean_closure', scope=dict(nodes=['A'],
        helper_prefixes=[], shared_writes=[], exclusive=False))
    f.task['status']['candidate_sha256'] = f.identity
    if request.param:
        f.task['worker_attempt'] = {'turn': 1}
    f.private.update(live={'present_nodes': ['A'], 'open_nodes': ['A']})
    f.private['in_flight_request'].update(kind='Worker', phase='ProofFormalization',
        active_node='A', task_execution={'task_kind': 'lean_closure'})
    put(f.directory / 'runtime/protocol_state.json', f.private)
    f.key = receipts.key(1, f.identity, f.task, acceptance_identity(f.private), f.source)
    canonical = json.loads((f.directory / 'canonical.json').read_text())
    canonical.update(key=f.key, acceptance_sha256=f.key['acceptance_sha256'])
    put(f.directory / 'canonical.json', canonical)
    put(f.directory / 'prepared.json', dict(identity=candidate.digest(candidate.canonical_json(
        dict(state=f.private, sources=f.source['manifest'], config_sha256=canonical['config_sha256'])))))
    recipe = json.loads((f.directory / 'execution/recipe.json').read_text())
    recipe['payload']['candidate'] = str(sealed)
    put(f.directory / 'execution/recipe.json', recipe)
    f.checked = dict(ok=True, candidate_sha256=f.identity, final_outcome='stuck',
        response=dict(status='Ok', outcome='Stuck', snapshot={'old': 'partial observations'}),
        checked_sources=receipts.checked_sources(f.repo))
    put(f.directory / 'execution/response.json', f.checked)
    put(f.directory / 'execution/result.json', dict(returncode=0,
        response_sha256=candidate.digest((f.directory / 'execution/response.json').read_bytes())))
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    return f


def stuck_driver(f, monkeypatch):
    from trellis.dispatch import publication_refresh as refresh
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = f.root
    # The constructor must see canonical sources, never the partial workspace.
    driver.repo = f.root.parent / 'canonical'
    driver.require_no_halt = lambda: None; driver.now_ms = lambda: 100
    driver.state = lambda: {'dispatch': {'tasks': {'1': f.task}}}
    driver.publisher = SimpleNamespace(locked=nullcontext,
        prepare=lambda *_args, **_kwargs: pytest.fail('Stuck must not publish source'),
        recover=lambda *_args, **_kwargs: pytest.fail('Stuck must not publish artifacts'))
    private = copy.deepcopy(f.private)
    private['in_flight_request'].update(id=11, cycle=3,
        review_verifier_evidence={'corr': {}}, worker_contract={'prompt_fragments': ['current']})
    private.update(cycle=3, latest_corr_review_nodes=[])
    calls, failures = [], []
    def kernel(payload):
        calls.append(payload)
        assert payload == dict(action='check_unchanged_closure_stuck', repo_path=str(driver.repo),
            request=private['in_flight_request'], baseline=private['live'], raw_payload=f.raw)
        return dict(status='dispatch_ok', output=dict(ok=True, final_outcome='stuck',
            response=dict(kind='worker', status='Ok', outcome='Stuck', request_id=11, cycle=3,
                snapshot=private['live'], acceptance_check_passed=False, local_closure_results={})))
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        observe_sources=lambda *_args, **_kwargs: f.source, _call=lambda _: private, kernel=kernel,
        prepare_publication=lambda payload: payload, commit_failure=lambda payload: failures.append(payload))
    monkeypatch.setattr(refresh, '_capture', lambda *_args, **_kwargs: pytest.fail('Stuck needs no compiler environment'))
    return driver, private, calls, failures


@pytest.mark.parametrize('outcome', ['Stuck', 'Invalid'])
@pytest.mark.parametrize('fault', [None, 'completion', 'candidate', 'halt'])
def test_obsolete_completed_negative_stops_stale_before_context_reconstruction(
        stuck_completed, monkeypatch, outcome, fault):
    f = stuck_completed
    f.checked['response']['outcome'] = outcome
    put(f.directory / 'execution/response.json', f.checked)
    put(f.directory / 'execution/result.json', dict(returncode=0,
        response_sha256=candidate.digest((f.directory / 'execution/response.json').read_bytes())))
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    driver, _, calls, failures = stuck_driver(f, monkeypatch)
    driver.runtime = f.root.parent
    changed = b'theorem A : True := True.intro\n'
    (driver.repo / 'Tablet/A.lean').write_bytes(changed)
    current = copy.deepcopy(f.source)
    current['input_hashes']['Tablet/A.lean'] = candidate.digest(changed)
    current['manifest']['Tablet/A.lean'] = candidate.digest(changed)
    driver.native.observe_sources = lambda *_args, **_kwargs: current
    events = []
    def native(payload):
        assert payload['action'] == 'dispatch_control', 'Must dispose before rebuilding private context'
        events.append(payload)
    driver.native._call = native
    driver._rebase_acceptance = lambda *_args, **_kwargs: pytest.fail('Obsolete negative needs no rebase')
    driver.native.prepare_publication = lambda *_args: pytest.fail('Stale is not a publication')
    if fault == 'completion':
        (f.directory / 'completed.json').write_text('{}')
    elif fault == 'candidate':
        (f.sealed / 'files/Tablet/A.lean').write_bytes(b'tampered')
    elif fault == 'halt':
        driver.require_no_halt = lambda: (_ for _ in ()).throw(RuntimeError('halt retained'))
    preserved = {str(p): p.read_bytes() for p in f.root.rglob('*') if p.is_file()}
    if fault:
        with pytest.raises((candidate.CandidateError, RuntimeError)):
            driver._publish(1, f.identity, f.directory, f.checked)
        assert not events
    else:
        assert driver._publish(1, f.identity, f.directory, f.checked) is True
        assert len(events) == 1
        event = events[0]['event']
        assert event['kind'] == 'attempt_stopping' and event['task_id'] == 1
        assert event['outcome']['kind'] == 'stale'
        assert 'stale task input: Tablet/A.lean' in event['outcome']['reason']
    assert not calls and not failures
    assert {str(p): p.read_bytes() for p in f.root.rglob('*') if p.is_file()} == preserved


@pytest.mark.parametrize('change', ['positive', 'authority_only', 'unrelated_input', 'keyless'])
def test_early_negative_disposition_preserves_other_currentness_paths(stuck_completed, monkeypatch, change):
    f = stuck_completed
    driver, _, _, _ = stuck_driver(f, monkeypatch)
    current = copy.deepcopy(f.source)
    checked = copy.deepcopy(f.checked)
    if change in ('positive', 'keyless'):
        current['input_hashes']['Tablet/A.lean'] = 'changed'
    if change == 'positive':
        checked['response']['outcome'] = 'Valid'
    elif change == 'authority_only':
        current['authority_inputs'] = {'policy': 'new'}
    elif change == 'unrelated_input':
        current['input_hashes']['Tablet/New.lean'] = 'new-node'
    elif change == 'keyless':
        captured = json.loads((f.directory / 'canonical.json').read_text())
        del captured['key']; put(f.directory / 'canonical.json', captured)
    driver.native.observe_sources = lambda *_args, **_kwargs: current
    def native(payload):
        assert payload['action'] == 'dispatch_acceptance_state', 'No obsolete sealed input means no Stale'
        raise RuntimeError('existing currentness path reached')
    driver.native._call = native
    with pytest.raises(RuntimeError, match='existing currentness path reached'):
        driver._publish(1, f.identity, f.directory, checked)


@pytest.mark.parametrize('upgraded', [False, True])
def test_context_only_stuck_discards_partial_edits_without_another_host_round(stuck_completed, monkeypatch, upgraded):
    f = stuck_completed
    driver, private, calls, failures = stuck_driver(f, monkeypatch)
    original = {name: (f.directory / name).read_bytes() for name in (*receipts._RECEIPTS, 'completed.json')}
    if upgraded:
        # Production114: the exact old host result survives a software upgrade
        # and native coarse-retention activation/checkpoint, with no source change.
        monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'current-implementation')
        private['coarse_task_retention_enabled'] = True
        private['coarse_goals'] = {'goals': {'Goal': {'status': 'active'}}}
        private['last_clean_commit'] = 'new-checkpoint'
        private['in_flight_request']['acceptance_logic_identity'] = 'current-implementation'
        private['in_flight_request']['worker_contract']['request_summary'] = {
            'acceptance_logic_identity': 'current-implementation'}
    partial = candidate.read(f.sealed)
    assert list(partial[0]['changes']) == ['Tablet/A.lean']
    assert driver._publish(1, f.identity, f.directory, f.checked) is True
    assert len(calls) == len(failures) == 1
    payload = failures[0]
    assert payload['response']['snapshot'] == private['live']
    assert payload['response']['outcome'] == 'Stuck'
    assert not payload['response']['acceptance_check_passed']
    assert not payload['response']['local_closure_results']
    assert payload['response']['cycle'] == 3
    assert payload['inputs'] == f.source['input_hashes']
    assert candidate.read(f.sealed) == partial
    assert all((f.directory / name).read_bytes() == data for name, data in original.items())
    refresh_receipt = json.loads(next((f.directory / 'refreshes').glob('*.json')).read_text())
    assert refresh_receipt['kind'] == 'closure_stuck_current_context'
    assert refresh_receipt['donor_completion_sha256'] == candidate.digest(original['completed.json'])
    assert refresh_receipt['current_key']['acceptance_sha256'] == acceptance_identity(private)
    assert refresh_receipt['current_key']['implementation_identity'] == receipts.implementation_identity()
    assert not (f.directory / 'retry-authorized.json').exists()
    assert [p.name for p in f.directory.parent.iterdir()] == ['1']


@pytest.mark.parametrize('fault', ['source', 'authority', 'candidate', 'binding', 'turn',
    'host_response', 'partial_source', 'completion', 'cleanup', 'sealed_edit',
    'not_ok', 'not_stuck', 'ordinary', 'native_cold'])
def test_stuck_refresh_keeps_identity_and_native_eligibility_guards(stuck_completed, monkeypatch, fault):
    from trellis.dispatch import publication_refresh as refresh
    f = stuck_completed
    driver, private, calls, failures = stuck_driver(f, monkeypatch)
    current = dict(f.key, acceptance_sha256=acceptance_identity(private),
        implementation_identity='upgraded-implementation')
    fields = dict(source='input_hashes', authority='authority_input_hashes',
        candidate='candidate_sha256', binding='task_binding_sha256', turn='submission_turn')
    if fault in fields:
        current[fields[fault]] = 'different'
    elif fault == 'host_response':
        (f.directory / 'execution/response.json').write_text('{}')
    elif fault == 'partial_source':
        (f.repo / 'Tablet/A.lean').write_text('changed after host completion')
    elif fault == 'completion':
        (f.directory / 'completed.json').write_text('{}')
    elif fault == 'cleanup':
        (f.directory / 'execution/cleanup.json').unlink()
    elif fault == 'sealed_edit':
        (f.sealed / 'files/Tablet/A.lean').write_text('tampered sealed proof')
    elif fault == 'not_ok':
        f.checked['ok'] = False
    elif fault == 'not_stuck':
        f.checked['response']['outcome'] = 'Valid'
    elif fault == 'ordinary':
        private['in_flight_request']['task_execution']['task_kind'] = 'ordinary'
    elif fault == 'native_cold':
        driver.native.kernel = lambda _: dict(status='dispatch_ok', output=None)
    result = refresh.refresh_stuck(f.directory, f.checked, current_key=current,
        private=private, repo=driver.repo, kernel=driver.native.kernel)
    assert result['status'] == 'cold'
    assert not calls and not failures
    assert not (f.directory / 'refreshes').exists()


@pytest.mark.parametrize('guard', ['halt', 'native_transition'])
def test_stuck_refresh_still_obeys_current_halt_and_failure_policy(stuck_completed, monkeypatch, guard):
    f = stuck_completed
    driver, private, calls, failures = stuck_driver(f, monkeypatch)
    def rejected(*args):
        raise RuntimeError('current native policy rejects')
    if guard == 'halt':
        driver.require_no_halt = rejected
    else:
        driver.native.prepare_publication = rejected
    with pytest.raises(RuntimeError, match='current native policy rejects'):
        driver._publish(1, f.identity, f.directory, f.checked)
    assert len(calls) == (0 if guard == 'halt' else 1)
    assert not failures


def test_freshness_conflict_retains_check_without_starting_another_round(completed, monkeypatch):
    f = completed
    f.task['assignment'].update(task_kind='proof', scope=dict(nodes=['A'],
        helper_prefixes=[], shared_writes=[], exclusive=False))
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.finalization_gate = asyncio.Lock()
    driver.acceptance_queue = SimpleNamespace(active={})
    driver.root = f.root; driver.repo = f.repo
    driver.state = lambda: {'dispatch': {'tasks': {'1': f.task}}}
    launched, published, pauses = [], [], []
    def process(directory, **_):
        launched.append(directory.name)
        return SimpleNamespace(root=directory / 'execution', stop_and_reap=lambda _: None), {}
    async def wait(*_): return f.checked
    def prepare(*_):
        pytest.fail('freshness conflict must not repeat expensive checking')
    def publish(task_id, identity, directory, checked):
        published.append(directory.name)
        return len(published) == 2
    driver._process = process; driver._wait = wait; driver._permit = lambda *_: None
    driver._pause = lambda *args, **kwargs: pauses.append((args, kwargs))
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None)
    driver._prepare_acceptance = prepare; driver._publish = publish
    monkeypatch.setattr(receipts, 'complete', lambda *_: None)
    from trellis.dispatch import staged_driver
    monkeypatch.setattr(staged_driver, 'plan', lambda *_: dict(status='unsupported'))
    async def unsupported_stage(*_):
        return False  # This fixture exercises the legacy full-acceptance route.
    monkeypatch.setattr(staged_driver, 'run', unsupported_stage)
    from trellis.dispatch import freshness_recheck
    monkeypatch.setattr(freshness_recheck, 'authorize', lambda *_: False)
    with pytest.raises(RuntimeError, match='freshness conflict'):
        asyncio.run(driver._integrate_reserved(1, f.identity))
    assert launched == published == ['1']
    assert len(pauses) == 1 and pauses[0][1]['evidence'] == str(f.directory)
    assert (f.directory / 'completed.json').exists()


def freshness_driver(f):
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root, driver.repo = f.root, f.repo
    driver.config_path = f.root / 'config.json'
    driver.config_path.write_bytes(b'original config')
    driver.require_no_halt = lambda: None
    driver.state = lambda: {'dispatch': {'tasks': {'1': f.task}}}
    current = copy.deepcopy(f.private)
    current['coarse_goals'] = {'goals': {'NewGoal': {}}}
    driver.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None,
        observe_sources=lambda *_: f.source, _call=lambda _: current)
    return driver, current


def test_context_recheck_is_durable_bounded_and_keeps_original_completion(completed):
    from trellis.dispatch import freshness_recheck as recovery
    f = completed
    driver, current = freshness_driver(f)
    originals = {str(p.relative_to(f.directory)): p.read_bytes()
                 for p in f.directory.rglob('*') if p.is_file()}
    assert recovery.authorize(driver, 1, f.identity, f.directory, f.checked)
    assert recovery.superseded(f.directory, f.identity)
    assert receipts.reusable(f.directory, f.key) is None
    assert not (f.directory / 'retry-authorized.json').exists()
    assert f.task['status']['status'] == 'checking'
    current['coarse_goals']['goals']['AnotherGoal'] = {}
    assert not recovery.authorize(driver, 1, f.identity, f.directory, f.checked)
    for name, data in originals.items():
        assert (f.directory / name).read_bytes() == data


@pytest.mark.parametrize('change', ['unchanged', 'source', 'authority', 'binding', 'config',
    'task', 'not_valid', 'response', 'receipt', 'cleanup', 'sealed'])
def test_context_recheck_refuses_noncontext_or_unauthenticated_changes(completed, change):
    from trellis.dispatch import freshness_recheck as recovery
    f = completed
    driver, current = freshness_driver(f)
    if change == 'unchanged': current.clear(); current.update(f.private)
    elif change == 'source': f.source['input_hashes']['Tablet/A.lean'] = 'different'
    elif change == 'authority': f.source['authority_inputs'] = {'tool': 'different'}
    elif change == 'binding': f.task['binding']['model'] = 'different'
    elif change == 'config': driver.config_path.write_bytes(b'different')
    elif change == 'task': f.task['status'] = {'status': 'resolved'}
    elif change == 'not_valid': f.checked['response']['outcome'] = 'Invalid'
    elif change == 'response': f.checked['response']['summary'] = 'tampered'
    elif change == 'receipt': put(f.directory / 'completed.json', {})
    elif change == 'cleanup': put(f.directory / 'execution/cleanup.json', {})
    elif change == 'sealed': put(f.root / '1/candidate/candidate.json', {})
    try:
        assert not recovery.authorize(driver, 1, f.identity, f.directory, f.checked)
    except (candidate.CandidateError, KeyError):
        assert change in ('response', 'receipt', 'cleanup', 'sealed')
    assert not (f.directory / recovery.MARKER).exists()


def test_context_recheck_can_fully_recheck_under_new_implementation(completed, monkeypatch):
    from trellis.dispatch import freshness_recheck as recovery
    f = completed
    driver, _ = freshness_driver(f)
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'new-implementation')
    assert recovery.authorize(driver, 1, f.identity, f.directory, f.checked)
    assert recovery.superseded(f.directory, f.identity)


def test_old_submission_recheck_does_not_block_new_candidate(completed):
    from trellis.dispatch import freshness_recheck as recovery
    f = completed
    driver, _ = freshness_driver(f)
    assert recovery.authorize(driver, 1, f.identity, f.directory, f.checked)
    def prepare(task_id, round_id, identity, excluded):
        assert (task_id, round_id, identity) == (1, 2, 'new-candidate')
        return None
    driver._prepare_acceptance = prepare
    asyncio.run(driver._integrate_legacy(1, 'new-candidate', f.task, f.root / 'new-candidate'))


def test_explicit_legacy_skip_preserves_success_and_native_retry_accounting(completed):
    from trellis.dispatch import freshness_recheck as recovery
    f = completed
    driver, _ = freshness_driver(f)
    assert recovery.authorize(driver, 1, f.identity, f.directory, f.checked, apply=False)
    assert not (f.directory / recovery.MARKER).exists()
    with pytest.raises(candidate.CandidateError):
        recovery.legacy_skip(f.directory, f.identity)
    assert recovery.authorize(driver, 1, f.identity, f.directory, f.checked)
    recovery.legacy_skip(f.directory, f.identity)
    recovery.legacy_skip(f.directory, f.identity)
    skip = json.loads((f.directory / 'retry-authorized.json').read_text())
    assert skip['kind'] == 'fresh_current_context_check'
    assert 'failed_result' not in skip and 'event' not in skip
    assert json.loads((f.directory / 'execution/result.json').read_text())['returncode'] == 0
    assert receipts.reusable(f.directory, f.key) is None


@pytest.mark.parametrize('restart', [False, True])
def test_legacy_context_conflict_checks_new_round_and_publishes_normally(completed, monkeypatch, restart):
    from trellis.dispatch import staged_driver, freshness_recheck
    f = completed
    driver, _ = freshness_driver(f)
    driver.finalization_gate = asyncio.Lock()
    launches, publications = [], []
    def process(directory, **_):
        launches.append(directory.name)
        return SimpleNamespace(root=directory / 'execution', stop_and_reap=lambda _: None), {}
    async def wait(*_): return f.checked
    driver._process, driver._wait = process, wait
    driver._permit = lambda *_: None
    driver._pause = lambda *_args, **_kwargs: pytest.fail('recoverable context drift must not stop the run')
    def publish(_task, _identity, directory, _checked):
        publications.append(directory.name)
        return directory.name == '2'
    driver._publish = publish
    crashed = False
    def prepare(_task, round_id, _identity, _excluded):
        nonlocal crashed
        assert round_id == 2 and freshness_recheck.superseded(f.directory, f.identity)
        if restart and not crashed:
            crashed = True
            raise RuntimeError('simulated preparation crash')
        put(f.directory.parent / '2/prepared.json', {})
        return f.directory.parent / '2', f.task, f.source['input_hashes'], None
    driver._prepare_acceptance = prepare
    monkeypatch.setattr(staged_driver, 'plan', lambda *_: {'status': 'unsupported'})
    monkeypatch.setattr(receipts, 'complete', lambda *_: None)
    if restart:
        with pytest.raises(RuntimeError, match='simulated preparation crash'):
            asyncio.run(driver._integrate_legacy(1, f.identity, f.task, f.root / '1/candidate'))
    asyncio.run(driver._integrate_legacy(1, f.identity, f.task, f.root / '1/candidate'))
    assert publications == launches == ['1', '2']
    assert (f.directory / 'completed.json').exists()


@pytest.mark.parametrize('damage', ['marker', 'completion', 'response'])
def test_context_recheck_restart_authenticates_original_evidence(completed, damage):
    from trellis.dispatch import freshness_recheck as recovery
    f = completed
    driver, _ = freshness_driver(f)
    assert recovery.authorize(driver, 1, f.identity, f.directory, f.checked)
    if damage == 'marker':
        marker = json.loads((f.directory / recovery.MARKER).read_text())
        marker['current_key']['input_hashes'] = {}
        put(f.directory / recovery.MARKER, marker)
    elif damage == 'completion': put(f.directory / 'completed.json', {'key': f.key})
    else: put(f.directory / 'execution/response.json', {})
    with pytest.raises(candidate.CandidateError):
        recovery.superseded(f.directory, f.identity)


@pytest.mark.parametrize('field', ['acceptance_sha256', 'input_hashes', 'candidate_sha256',
    'task_id', 'task_binding_sha256', 'implementation_identity'])
def test_changed_complete_key_cannot_reuse(completed, field):
    key = copy.deepcopy(completed.key)
    key[field] = {'different': 'hash'} if field == 'input_hashes' else 'changed'
    assert receipts.reusable(completed.directory, key) is None


@pytest.mark.parametrize('change', ['source', 'global', 'candidate', 'response', 'completion', 'cleanup',
    'permit', 'prepared', 'private', 'canonical', 'recipe', 'symlink', 'hardlink', 'retry', 'missing'])
def test_changed_checked_evidence_cannot_reuse(completed, change):
    f = completed
    paths = dict(source='repo/Tablet/A.lean', global_='repo/lakefile.toml', response='execution/response.json',
        completion='completed.json', cleanup='execution/cleanup.json', permit='execution/permit.json',
        prepared='prepared.json', private='runtime/protocol_state.json', canonical='canonical.json', recipe='execution/recipe.json')
    if change == 'candidate':
        (f.root / '1/candidate/candidate.json').write_text('{}')
    elif change == 'retry':
        put(f.directory / 'retry-authorized.json', {})
    elif change in ('symlink', 'hardlink', 'missing'):
        path = f.directory / 'execution/response.json'
        saved = f.directory / 'saved-response'; path.rename(saved)
        if change == 'symlink': path.symlink_to(saved)
        elif change == 'hardlink': path.hardlink_to(saved)
    else:
        (f.directory / paths['global_' if change == 'global' else change]).write_text('{}')
    assert receipts.reusable(f.directory, f.key) is None


def test_unauthorized_compiler_recipe_never_creates_reusable_completion(completed):
    f = completed
    (f.directory / 'completed.json').unlink()
    recipe = json.loads((f.directory / 'execution/recipe.json').read_text())
    recipe['checker']['compiler'] = 'none'
    put(f.directory / 'execution/recipe.json', recipe)
    with pytest.raises(candidate.CandidateError, match='authorized host compiler'):
        receipts.complete(f.directory, f.checked)


@pytest.mark.parametrize('outcome', ['Stuck', 'Invalid'])
def test_nonvalid_response_is_preserved_exactly(completed, outcome):
    f = completed
    (f.directory / 'completed.json').unlink()
    f.checked['response']['outcome'] = outcome
    put(f.directory / 'execution/response.json', f.checked)
    put(f.directory / 'execution/result.json', dict(returncode=0,
        response_sha256=candidate.digest((f.directory / 'execution/response.json').read_bytes())))
    receipts.complete(f.directory, f.checked)
    assert receipts.reusable(f.directory, f.key)['response']['outcome'] == outcome


def test_publication_records_actual_key_delta_before_rejection(completed):
    f = completed
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.repo = f.repo; driver.require_no_halt = lambda: None; driver.now_ms = lambda: 100
    state = {'dispatch': {'tasks': {'1': f.task}}}
    driver.state = lambda: state
    driver.publisher = SimpleNamespace(locked=nullcontext)
    private = dict(f.private, gate='changed secret value')
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        observe_sources=lambda *_: f.source, _call=lambda _: private,
        last_source_observation={'cache_hit': True, 'key_sha256': 'captured-observation'})
    assert driver._publish(1, f.identity, f.directory, f.checked) is False
    rows = list((f.directory / 'rejections').glob('*.json'))
    assert len(rows) == 1
    data = rows[0].read_text(); evidence = json.loads(data)
    assert 'changed secret value' not in data
    assert evidence['acceptance']['differences'][0]['path'] == '/gate'
    assert evidence['observation_cache']['cache_hit'] is True
    assert evidence['captured_acceptance_sha256'] == f.key['acceptance_sha256']
    assert evidence['current_acceptance_sha256'] == acceptance_identity(private)
    assert evidence['canonical_state_sha256'] == candidate.digest(candidate.canonical_json(state))


def test_rejection_deltas_are_bounded_and_hash_oversized_values():
    result = receipts.differences({'private': 's' * 100000, **{str(i): 1 for i in range(1000)}},
        {'private': 't' * 100000, **{str(i): 2 for i in range(1000)}})
    assert len(result['differences']) == 48 and result['truncated']
    assert len(candidate.canonical_json(result)) < 20000


def observation_caches(directory):
    from trellis.checker import sync
    state = directory / 'runtime/checker-state'
    semantic = 'a' * 64 + '-principal-' + 'b' * 64
    axioms = 'c' * 64
    closure = 'd' * 64 + '-principal-' + 'b' * 64 + '-premises-' + 'c' * 64
    sync.store_semantic_payload(state / sync.SEMANTIC_PAYLOAD_DIRNAME, semantic,
        node_name='A', ok=True, payload='checked semantic observation', error='',
        cache_version=sync.SEMANTIC_PAYLOAD_CACHE_VERSION)
    sync.store_print_axioms(state / sync.PRINT_AXIOMS_DIRNAME, axioms,
        node_name='A', returncode=0, stdout='checked axioms', stderr='', timed_out=False,
        spawn_error='', cache_version=sync.PRINT_AXIOMS_CACHE_VERSION)
    sync.store_local_closure_axioms(state / sync.LOCAL_CLOSURE_AXIOMS_DIRNAME, closure,
        node_name='A', response=dict(status='ok', returncode=0, timed_out=False,
            spawn_error='', checker_identity='checked-implementation', premise_owners=['A']),
        cache_version=sync.LOCAL_CLOSURE_AXIOMS_CACHE_VERSION)
    # These are not portable observations and must never be seeded.
    put(state / 'sync-fingerprints.json', {'untrusted_as_replay_authority': True})
    put(state / 'compile-cache.json', {'untrusted_as_replay_authority': True})
    return sorted(state.glob('*/*.json'))


def test_changed_context_gets_fresh_round_with_independent_host_facts(completed, monkeypatch):
    from contextlib import contextmanager
    f = completed
    facts = observation_caches(f.directory)
    old_completion = (f.directory / 'completed.json').read_bytes()
    private = dict(f.private, cycle=3, gate='new current authorization')
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = f.root; driver.repo = f.repo; driver.config_path = f.repo / 'trellis.config.json'
    driver.state = lambda: {'dispatch': {'tasks': {'1': f.task}}}
    held = False
    @contextmanager
    def locked():
        nonlocal held
        held = True
        try: yield
        finally: held = False
    driver.native = SimpleNamespace(locked=locked, _require_recovered_publication=lambda: None,
        observe_sources=lambda *_: f.source, _call=lambda _: private)
    prepared = []
    def prepare(directory, current, *, host_only):
        assert held and current == private and host_only
        prepared.append(current)
        directory.mkdir()
    driver._prepare = prepare
    seed = receipts.seed_probe_caches
    def outside_lock(*args):
        assert not held
        return seed(*args)
    monkeypatch.setattr(receipts, 'seed_probe_caches', outside_lock)
    directory, task, inputs, checked = driver._prepare_acceptance(1, 2, f.identity)
    assert prepared == [private] and checked is None  # A new host check, not the old verdict.
    assert inputs == f.source['input_hashes'] and task == f.task
    capture = json.loads((directory / 'canonical.json').read_text())
    assert capture['acceptance_sha256'] == acceptance_identity(private) != f.key['acceptance_sha256']
    for path in facts:
        copied = directory / path.relative_to(f.directory)
        assert copied.read_bytes() == path.read_bytes()
        assert copied.stat().st_ino != path.stat().st_ino
    assert not (directory / 'runtime/checker-state/sync-fingerprints.json').exists()
    assert not (directory / 'runtime/checker-state/compile-cache.json').exists()
    assert not (directory / 'completed.json').exists()
    assert (f.directory / 'completed.json').read_bytes() == old_completion
    assert sum(json.loads((directory / 'probe-cache-seed.json').read_text())['copied'].values()) == 3


@pytest.mark.parametrize('field', ['input_hashes', 'candidate_sha256', 'task_id',
    'task_binding_sha256', 'implementation_identity', 'submission_turn'])
def test_observation_donation_requires_exact_other_bindings(completed, field):
    f = completed
    observation_caches(f.directory)
    current = dict(f.key, acceptance_sha256='changed-context')
    current[field] = {'different': 'hash'} if field == 'input_hashes' else 'changed'
    destination = f.directory.parent / '2'
    assert receipts.seed_probe_caches(destination, current, [f.directory]) == {}
    assert not destination.exists()


@pytest.mark.parametrize('damage', ['receipt', 'source', 'cleanup', 'live', 'retry', 'foreign'])
def test_observation_donor_must_be_authentic_and_reaped(completed, monkeypatch, damage):
    f = completed
    observation_caches(f.directory)
    destination = f.directory.parent / '2'
    if damage == 'receipt': (f.directory / 'completed.json').write_text('{}')
    elif damage == 'source': (f.repo / 'Tablet/A.lean').write_text('changed')
    elif damage == 'cleanup': (f.directory / 'execution/cleanup.json').unlink()
    elif damage == 'retry': put(f.directory / 'retry-authorized.json', {})
    elif damage == 'live': monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: True)
    elif damage == 'foreign': destination = f.root / '2/acceptance/2'
    assert receipts.seed_probe_caches(destination, dict(f.key, acceptance_sha256='new'), [f.directory]) == {}
    assert not destination.exists()


@pytest.mark.parametrize('damage', ['symlink', 'hardlink', 'corrupt', 'version', 'key', 'failure', 'oversized'])
def test_unusable_observation_entries_remain_cold(completed, damage):
    f = completed
    facts = observation_caches(f.directory)
    for path in facts:
        if damage in ('symlink', 'hardlink'):
            saved = path.with_suffix('.original'); path.rename(saved)
            if damage == 'symlink': path.symlink_to(saved)
            else: path.hardlink_to(saved)
        elif damage == 'corrupt': path.write_text('{')
        elif damage == 'oversized':
            with path.open('wb') as stream: stream.truncate((16 << 20) + 1)
        else:
            data = json.loads(path.read_text())
            if damage == 'version': data['cache_version'] = 0
            elif damage == 'key': data['key_blob_sha256'] = 'wrong'
            else:
                data['ok'] = False; data['returncode'] = 1
                if 'response' in data: data['response']['status'] = 'failed'
            put(path, data)
    destination = f.directory.parent / '2'
    assert receipts.seed_probe_caches(destination, dict(f.key, acceptance_sha256='new'), [f.directory]) == {}
    assert not destination.exists()


def test_optional_observation_copy_failure_keeps_donor_and_allows_cold_check(completed, monkeypatch):
    f = completed
    facts = observation_caches(f.directory)
    original = {p: p.read_bytes() for p in facts}
    monkeypatch.setattr(receipts, 'atomic', lambda *_: (_ for _ in ()).throw(OSError('disk full')))
    destination = f.directory.parent / '2'
    assert receipts.seed_probe_caches(destination, dict(f.key, acceptance_sha256='new'), [f.directory]) == {}
    assert original == {p: p.read_bytes() for p in facts}


def test_seed_summary_failure_does_not_start_another_donor(completed, monkeypatch):
    f = completed
    facts = observation_caches(f.directory)
    calls = []
    reusable = receipts.reusable
    def recorded(*args):
        calls.append(args[0])
        return reusable(*args)
    monkeypatch.setattr(receipts, 'reusable', recorded)
    monkeypatch.setattr(receipts, 'write_once', lambda *_: (_ for _ in ()).throw(OSError('summary write failed')))
    destination = f.directory.parent / '2'
    copied = receipts.seed_probe_caches(destination, dict(f.key, acceptance_sha256='new'), [f.directory, f.directory])
    assert sum(copied.values()) == 3 and calls == [f.directory]
    assert all((destination / p.relative_to(f.directory)).read_bytes() == p.read_bytes() for p in facts)


def test_probe_seed_transfer_has_one_byte_and_entry_budget(completed, monkeypatch):
    f = completed
    facts = observation_caches(f.directory)
    budget = max(p.stat().st_size for p in facts)
    monkeypatch.setattr(receipts, '_PROBE_SEED_MAX_BYTES', budget)
    destination = f.directory.parent / '2'
    receipts.seed_probe_caches(destination, dict(f.key, acceptance_sha256='new'), [f.directory])
    actual = list((destination / 'runtime/checker-state').glob('*/*.json'))
    assert actual and sum(p.stat().st_size for p in actual) <= budget
    monkeypatch.setattr(receipts, '_PROBE_SEED_MAX_ENTRIES', 1)
    destination = f.directory.parent / '3'
    copied = receipts.seed_probe_caches(destination, dict(f.key, acceptance_sha256='new'), [f.directory])
    assert sum(copied.values()) == 1


def test_installed_copy_with_failed_fsync_still_consumes_budget(completed, monkeypatch):
    f = completed
    facts = observation_caches(f.directory)
    budget = max(p.stat().st_size for p in facts)
    monkeypatch.setattr(receipts, '_PROBE_SEED_MAX_BYTES', budget)
    atomic = receipts.atomic
    def write_then_fail(path, data):
        atomic(path, data)
        raise OSError('directory fsync failed after installation')
    monkeypatch.setattr(receipts, 'atomic', write_then_fail)
    destination = f.directory.parent / '2'
    receipts.seed_probe_caches(destination, dict(f.key, acceptance_sha256='new'), [f.directory, f.directory])
    actual = list((destination / 'runtime/checker-state').glob('*/*.json'))
    assert actual and sum(p.stat().st_size for p in actual) <= budget


def test_replay_and_probe_donors_share_one_completed_host_authentication(completed, monkeypatch):
    from trellis.dispatch.publication_artifacts import lexical_authority
    from trellis.checker.portable_replay import DIRECTORY
    f = completed
    observation_caches(f.directory)
    authority = lexical_authority(f.directory / 'repo')
    (authority / DIRECTORY).mkdir(parents=True)
    (authority / 'lakefile.toml').write_bytes((f.repo / 'lakefile.toml').read_bytes())
    # This fixture originally completed without an authority. Re-complete the
    # test host round with the newly materialized authority before donation.
    f.checked['checked_sources'] = receipts.checked_sources(f.repo)
    put(f.directory / 'execution/response.json', f.checked)
    put(f.directory / 'execution/result.json', dict(returncode=0,
        response_sha256=candidate.digest((f.directory / 'execution/response.json').read_bytes())))
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    calls = []
    original = receipts.reusable
    def authenticated(*args):
        calls.append(args[0])
        return original(*args)
    monkeypatch.setattr(receipts, 'reusable', authenticated)
    destination = f.directory.parent / '2'
    current = dict(f.key, acceptance_sha256='new-context')
    copied = receipts.seed_probe_caches(destination, current, [f.directory])
    marker = json.loads((destination / 'replay-cache-donor.json').read_text())
    assert sum(copied.values()) == 3 and calls == [f.directory]
    assert marker['donor_round'] == f.directory.name
    assert marker['current_key_sha256'] == candidate.digest(candidate.canonical_json(current))
    assert not (destination / 'repo/.trellis/supervisor/repo/.lake/build').exists()


@pytest.mark.parametrize('damage', [None, 'bytes', 'symlink', 'hardlink'])
def test_completed_context_binding_authenticates_exact_host_baseline(completed, damage):
    f = completed
    path = f.directory / 'runtime/acceptance-context.json'
    atomic(path, b'{"baseline":"fresh before overlay"}')
    f.checked['acceptance_context_sha256'] = receipts.regular_digest(path)
    put(f.directory / 'execution/response.json', f.checked)
    put(f.directory / 'execution/result.json', dict(returncode=0,
        response_sha256=candidate.digest((f.directory / 'execution/response.json').read_bytes())))
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    if damage == 'bytes': path.write_text('{}')
    elif damage in {'symlink', 'hardlink'}:
        saved = path.with_suffix('.original'); path.rename(saved)
        if damage == 'symlink': path.symlink_to(saved)
        else: path.hardlink_to(saved)
    assert (receipts.reusable(f.directory, f.key) is not None) == (damage is None)


def test_context_digest_streams_large_snapshot_without_read_bytes(tmp_path, monkeypatch):
    path = tmp_path / 'context.json'
    data = b'x' * ((2 << 20) + 7)
    path.write_bytes(data)
    monkeypatch.setattr(Path, 'read_bytes', lambda _: pytest.fail('snapshot digest must stream'))
    assert receipts.regular_digest(path) == candidate.digest(data)


def shadow_trace(*, operand='a', result='b', purpose='observation', status='recorded'):
    digest = lambda value: dict(sha256=value * 64 if status == 'recorded' else None,
        serialized_bytes=1, status=status)
    return dict(version=1, producer='host_acceptance_native', baseline=None,
        check=dict(version=1, coverage='PARTIAL', authority='diagnostic_only', uncovered=['recursive reads'],
            groups=[dict(name='validation.plan', source_phase='post_overlay_pre_autofix',
                purpose=purpose, status='completed', input=digest(operand), result=digest(result))]))


def shadow_completed_pair(f, *, before=None, after=None):
    """Build each host receipt using the ordinary completion authentication."""
    import shutil
    first = f.directory
    second = first.parent / '2'
    shutil.copytree(first, second)
    for directory, trace in ((first, before), (second, after)):
        (directory / 'completed.json').unlink()
        private = copy.deepcopy(f.private)
        if directory == second:
            private['review_comment'] = 'new context, same source and task'
        key = dict(f.key, acceptance_sha256=acceptance_identity(private))
        capture = json.loads((directory / 'canonical.json').read_text())
        capture.update(key=key, acceptance_sha256=key['acceptance_sha256'])
        put(directory / 'canonical.json', capture)
        put(directory / 'runtime/protocol_state.json', private)
        put(directory / 'prepared.json', dict(identity=candidate.digest(candidate.canonical_json(
            dict(state=private, sources=json.loads((directory / 'base-manifest.json').read_text()),
                 config_sha256=capture['config_sha256'])))))
        recipe = json.loads((directory / 'execution/recipe.json').read_text())
        recipe['payload'].update(config_path=str(directory / 'repo/trellis.config.json'),
            runtime_root=str(directory / 'runtime'))
        recipe['checker'].update(task_runtime=str(directory / 'runtime'), scope='1/acceptance/' + directory.name)
        put(directory / 'execution/recipe.json', recipe)
        checked = copy.deepcopy(f.checked)
        if trace is not None:
            checked['acceptance_read_trace'] = trace
        put(directory / 'execution/response.json', checked)
        put(directory / 'execution/result.json', dict(returncode=0,
            response_sha256=candidate.digest((directory / 'execution/response.json').read_bytes())))
        receipts.complete(directory, checked)
    return first, second


@pytest.mark.parametrize('after, expected', [
    (shadow_trace(), 'observations_equal'),
    (shadow_trace(operand='c'), 'operands_changed'),
    (shadow_trace(result='c'), 'equal_operands_different_results'),
    (shadow_trace(status='unavailable_or_budget_exhausted'), 'inconclusive'),
])
def test_shadow_pair_classifies_only_authenticated_diagnostics(completed, monkeypatch, after, expected):
    from trellis.dispatch.acceptance_shadow import compare_completed_rounds
    first, second = shadow_completed_pair(completed, before=shadow_trace(), after=after)
    # Pairing must not start the compiler, a native RPC, or a source inventory.
    monkeypatch.setattr(receipts, 'checked_sources', lambda *_: pytest.fail('offline comparison scanned sources'))
    snapshots = {p: p.read_bytes() for d in (first, second) for p in d.rglob('*.json')}
    report = compare_completed_rounds(first, second)
    assert report['authority'] == 'diagnostic_only' and report['coverage'] == 'PARTIAL'
    assert report['acceptance_changed'] and report['groups'][0]['status'] == expected
    assert all(p.read_bytes() == data for p, data in snapshots.items())


def test_shadow_does_not_relax_exact_reuse_or_treat_refresh_as_evidence(completed):
    from trellis.dispatch.acceptance_shadow import compare_completed_rounds
    first, second = shadow_completed_pair(completed,
        before=shadow_trace(purpose='refresh_only'), after=shadow_trace(purpose='refresh_only'))
    new_key = json.loads((second / 'canonical.json').read_text())['key']
    assert receipts.reusable(first, new_key) is None
    assert compare_completed_rounds(first, second)['groups'][0]['status'] == 'refresh_only_operands_equal'


@pytest.mark.parametrize('change, expected', [
    (None, 'successful_facts_equal'),
    ('fact', 'successful_facts_differ'),
    ('input', 'inconclusive'),
    ('projection', 'inconclusive'),
    ('failure', 'inconclusive'),
    ('budget', 'inconclusive'),
    ('refresh', 'inconclusive'),
    ('missing', 'inconclusive'),
])
def test_shadow_success_facts_do_not_hide_raw_differences_or_authorize_reuse(completed, change, expected):
    from trellis.dispatch.acceptance_shadow import compare_completed_rounds
    before = shadow_trace()
    before['check']['groups'][0]['success_fact'] = dict(
        projection='worker_validation_success_v1', status='recorded_successful_steps',
        digest=dict(status='recorded', sha256='d' * 64, serialized_bytes=17))
    after = copy.deepcopy(before)
    group = after['check']['groups'][0]
    group['result']['sha256'] = 'c' * 64  # Timing/prose still differs in the raw observation.
    fact = group['success_fact']
    if change == 'fact': fact['digest']['sha256'] = 'e' * 64
    elif change == 'input': group['input']['sha256'] = 'f' * 64
    elif change == 'projection': fact['projection'] = 'unknown_future_projection'
    elif change == 'failure': fact['status'] = 'no_success_fact'
    elif change == 'budget': fact['digest'].update(status='unavailable_or_budget_exhausted', sha256=None)
    elif change == 'refresh': group['purpose'] = 'refresh_only'
    elif change == 'missing': del group['success_fact']
    first, second = shadow_completed_pair(completed, before=before, after=after)
    report = compare_completed_rounds(first, second)
    row = report['groups'][0]
    assert row['success_fact_status'] == expected
    if change not in ('input', 'refresh'):
        assert row['status'] == 'equal_operands_different_results'
    assert report['authority'] == 'diagnostic_only' and report['coverage'] == 'PARTIAL'
    assert receipts.reusable(first, json.loads((second / 'canonical.json').read_text())['key']) is None


@pytest.mark.parametrize('damage', ['response', 'key', 'budget', 'candidate_trace'])
def test_shadow_rejects_changed_receipts_and_bounds_reads(completed, monkeypatch, damage):
    from trellis.dispatch import acceptance_shadow as shadow
    first, second = shadow_completed_pair(completed, before=shadow_trace(), after=shadow_trace())
    if damage == 'response':
        put(second / 'execution/response.json', dict(completed.checked, acceptance_read_trace=shadow_trace(result='f')))
    elif damage == 'key':
        saved = json.loads((second / 'completed.json').read_text())
        saved['key']['implementation_identity'] = 'another implementation'
        put(second / 'completed.json', saved)
    elif damage == 'budget':
        monkeypatch.setattr(shadow, '_MAX_BYTES', 16)
    else:
        # A trace supplied by a worker cannot be authenticated as a host completion.
        raw = dict(completed.checked, acceptance_read_trace=shadow_trace())
        put(completed.directory.parent.parent / 'candidate/response.json', raw)
        (second / 'completed.json').unlink()
    with pytest.raises((ValueError, OSError)):
        shadow.compare_completed_rounds(first, second)


def test_shadow_legacy_completion_has_no_fabricated_observation(completed):
    from trellis.dispatch.acceptance_shadow import compare_completed_rounds
    first, second = shadow_completed_pair(completed)
    report = compare_completed_rounds(first, second)
    assert not report['traces_present'] and report['groups'] == []


def test_shadow_streams_large_pinned_snapshots_with_one_pair_budget(completed, monkeypatch):
    from trellis.dispatch import acceptance_shadow as shadow
    # Real host completion authentication includes this large private state.
    # It exceeds the later parsed-JSON ceiling but must not be parsed offline.
    completed.private['large_history'] = 'x' * (32 << 10)
    first, second = shadow_completed_pair(completed, before=shadow_trace(), after=shadow_trace())
    monkeypatch.setattr(shadow, '_MAX_JSON_BYTES', 16 << 10)
    monkeypatch.setattr(shadow, '_HASH_CHUNK_BYTES', 256)
    size = sum((directory / name).stat().st_size for directory in (first, second)
        for name in ('completed.json', *receipts._RECEIPTS))
    reads = []
    original_open = Path.open
    class CountedReader:
        def __init__(self, stream): self.stream = stream
        def __enter__(self): return self
        def __exit__(self, *args): return self.stream.__exit__(*args)
        def fileno(self): return self.stream.fileno()
        def read(self, amount):
            assert 0 < amount <= 256  # Never request the whole snapshot.
            reads.append(amount)
            return self.stream.read(amount)
    def opened(path, *args, **kwargs):
        stream = original_open(path, *args, **kwargs)
        return CountedReader(stream) if path.name == 'protocol_state.json' else stream
    monkeypatch.setattr(Path, 'open', opened)
    report = shadow.compare_completed_rounds(first, second, max_bytes=size)
    assert report['authenticated_bytes'] == size
    assert report['groups'][0]['status'] == 'observations_equal'
    assert len(reads) > 200
    # Each file fits this limit individually; their cumulative pair does not.
    with pytest.raises(ValueError, match='budget'):
        shadow.compare_completed_rounds(first, second, max_bytes=size - 1)
    monkeypatch.setattr(shadow, '_MAX_BYTES', size - 1)
    with pytest.raises(ValueError, match='budget'):
        shadow.compare_completed_rounds(first, second)
    assert shadow.compare_completed_rounds(first, second, max_bytes=size)['authenticated_bytes'] == size


def test_shadow_streamed_hash_rejects_same_size_same_mtime_changed_bytes(completed, monkeypatch):
    import os
    from trellis.dispatch import acceptance_shadow as shadow
    completed.private['large_history'] = 'x' * 4096
    first, second = shadow_completed_pair(completed, before=shadow_trace(), after=shadow_trace())
    path = second / 'runtime/protocol_state.json'
    info = path.stat()
    data = path.read_bytes().replace(b'xxx', b'yxx', 1)
    path.write_bytes(data)
    os.utime(path, ns=(info.st_atime_ns, info.st_mtime_ns))
    assert path.stat().st_size == info.st_size and path.stat().st_mtime_ns == info.st_mtime_ns
    monkeypatch.setattr(shadow, '_HASH_CHUNK_BYTES', 128)
    with pytest.raises(ValueError, match='host completion evidence changed: runtime/protocol_state.json'):
        shadow.compare_completed_rounds(first, second)


def test_shadow_total_io_override_does_not_relax_parsed_json_limit(completed, monkeypatch):
    from trellis.dispatch import acceptance_shadow as shadow
    first, second = shadow_completed_pair(completed, before=shadow_trace(), after=shadow_trace())
    monkeypatch.setattr(shadow, '_MAX_JSON_BYTES', 16)
    with pytest.raises(ValueError, match='parsed JSON'):
        shadow.compare_completed_rounds(first, second, max_bytes=1 << 40)


def test_compact_receipt_binds_stored_bytes_even_unused_table_entries(completed):
    from trellis.history_artifacts import encode_shared_state
    from trellis.private_state import read_private_state
    f = completed
    path = f.directory / 'runtime/protocol_state.json'
    carrier = encode_shared_state({'state': f.private})
    put(path, carrier)
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    assert receipts.reusable(f.directory, f.key) == f.checked
    carrier['$strings']['unused'] = 'not referenced by any logical field'
    put(path, carrier)
    assert read_private_state(path) == f.private
    assert receipts.reusable(f.directory, f.key) is None


def test_archived_host_response_retains_authentication_and_offline_shadow(completed, monkeypatch):
    from trellis.dispatch import retained_files as retained, acceptance_shadow as shadow
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    first, second = shadow_completed_pair(completed, before=shadow_trace(), after=shadow_trace())
    before = shadow.compare_completed_rounds(first, second)
    for directory in (first, second):
        path = directory / 'execution/response.json'
        raw = path.read_bytes()
        # Existing fixture facts contain enough repeated structure to compress.
        assert retained.archive(path)
        assert receipts.regular(path) == raw
    assert shadow.compare_completed_rounds(first, second) == before
    assert receipts.reusable(first, completed.key) is not None
    monkeypatch.setattr(shadow, '_MAX_JSON_BYTES', 10)
    with pytest.raises(ValueError, match='byte limit|byte budget'):
        shadow.compare_completed_rounds(first, second)

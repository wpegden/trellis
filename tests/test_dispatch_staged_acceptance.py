import copy
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from trellis.dispatch import acceptance_receipts as receipts, candidate, staged_acceptance as staged
from trellis.dispatch.driver import acceptance_identity
from trellis.dispatch.publication import atomic


def put(path, value):
    atomic(path, candidate.canonical_json(value))


@pytest.fixture
def fixture(tmp_path, monkeypatch):
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'test-implementation')
    monkeypatch.setattr(receipts, 'input_hashes', lambda repo: candidate.source_manifest(repo))
    root = tmp_path / 'runtime/dispatch'
    task_root = root / '1'
    worker = tmp_path / 'worker'
    (worker / 'Tablet').mkdir(parents=True)
    (worker / 'Tablet/A.lean').write_text('theorem A : True := by sorry\n')
    (worker / 'Tablet/B.lean').write_text('old sibling proof')
    before = candidate.source_manifest(worker)
    (worker / 'Tablet/A.lean').write_text('theorem A : True := by trivial\n')
    identity = candidate.seal(worker, task_root / 'candidate', task_id=1,
        base_manifest=before, inputs=before, response={'outcome': 'valid'})
    task = dict(id=1, assignment={'scope': {'nodes': ['A']}, 'compiler': 'managed'})

    def prepare(directory, *, stage='owned', cycle=1, implementation=None):
        repo = directory / 'repo'
        (repo / 'Tablet').mkdir(parents=True)
        (repo / 'Tablet/A.lean').write_text('theorem A : True := by trivial\n')
        (repo / 'Tablet/B.lean').write_text('old sibling proof')
        (repo / 'trellis.config.json').write_text('{}')
        private = dict(phase='ProofFormalization', in_flight_request=dict(id=cycle, cycle=cycle, kind='Worker',
            phase='ProofFormalization', acceptance_logic_identity=implementation or receipts.implementation_identity()), request_seq=cycle)
        source = dict(input_hashes=before, graph={'edges': {'A': []}}, authority_inputs={'.trellis/scripts/check.py': 'exact-script'})
        key = receipts.key(1, identity, task, acceptance_identity(private), source)
        if implementation is not None:
            key['implementation_identity'] = implementation
        captured = dict(key=key, acceptance_sha256=key['acceptance_sha256'], source=source,
                        config_sha256=candidate.digest(b'canonical configuration'))
        put(directory / 'canonical.json', captured)
        put(directory / 'prepared.json', dict(identity=candidate.digest(candidate.canonical_json(
            dict(state=private, sources=before, config_sha256=captured['config_sha256'])))))
        put(directory / 'base-manifest.json', before)
        put(directory / 'runtime/protocol_state.json', private)
        put(directory / 'runtime/runtime_metadata.json', {'repo_path': str(repo)})
        process = dict(pid=99999999, start_ticks=1, boot_id='expired-test-process')
        for name in ('process', 'permit', 'cleanup'):
            put(directory / f'execution/{name}.json', process)
        payload = dict(stage=stage, config_path=str(repo / 'trellis.config.json'),
            runtime_root=str(directory / 'runtime'), candidate=str(task_root / 'candidate'),
            captured_inputs=before, source_graph=source['graph'])
        put(directory / 'execution/recipe.json', dict(command=['python', *staged._COMMAND], payload=payload,
            checker=dict(runtime=str(root.parent), task_runtime=str(directory / 'runtime'),
                         compiler='managed', scope=str(directory.relative_to(root)))))
        context = {'request': private['in_flight_request'], 'host_baseline': True}
        put(directory / 'runtime/staged-context.json', context)
        facts = dict(schema_version=1, owner_nodes=['A'], owner_ok=True,
                     acceptance_logic_identity=key['implementation_identity'],
                     observation_environment={'environment_sha256s': {'A': 'complete-import-bundle'}})
        output = dict(stage=stage, status='facts', facts=facts, candidate_sha256=identity,
            context_sha256=candidate.digest(candidate.canonical_json(context)),
            checked_sources=receipts.checked_sources(repo))

        def save(value):
            put(directory / 'execution/response.json', value)
            put(directory / 'execution/result.json', dict(returncode=0,
                response_sha256=candidate.digest((directory / 'execution/response.json').read_bytes())))
        save(output)
        return SimpleNamespace(directory=directory, repo=repo, key=key, private=private,
            payload=payload, output=output, save=save)

    owned = prepare(task_root / 'acceptance/1')
    staged.complete(owned.directory, owned.output)
    return SimpleNamespace(root=root, task_root=task_root, owned=owned, prepare=prepare,
                           identity=identity, before=before)


def test_owned_completion_is_restartable_facts_without_worker_decision(fixture):
    owned = fixture.owned
    assert staged.reusable(owned.directory, owned.key) == owned.output
    receipt = json.loads((owned.directory / 'staged-completed.json').read_text())
    assert receipt['stage'] == 'owned'
    assert 'runtime/staged-context.json' in receipt['files']
    assert not (owned.directory / 'completed.json').exists()
    assert 'response' not in owned.output


@pytest.mark.parametrize('with_finalizer', [False, True])
def test_terminal_staged_context_archive_preserves_authentication_and_donation(fixture, monkeypatch, with_finalizer):
    from trellis.dispatch import retained_files as retained
    monkeypatch.setattr(retained, 'MIN_BYTES', 4096)
    owned = fixture.owned
    contexts = [owned]
    path = owned.directory / 'runtime/staged-context.json'
    path.write_bytes(path.read_bytes() + b' ' * 10000)
    owned.output['context_sha256'] = candidate.digest(path.read_bytes())
    owned.save(owned.output)
    (owned.directory / 'staged-completed.json').unlink()
    staged.complete(owned.directory, owned.output)
    if with_finalizer:
        final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
        marker = staged.prepare_donor(final.directory, owned.output)
        path = final.directory / 'runtime/staged-context.json'
        path.write_bytes(path.read_bytes() + b' ' * 10000)
        final.output = dict(stage='finalize', status='checked', candidate_sha256=fixture.identity,
            context_sha256=candidate.digest(path.read_bytes()),
            donor_completion_sha256=marker['donor_completion_sha256'],
            output=dict(response={'outcome': 'Valid', 'request_id': 2}),
            checked_sources=receipts.checked_sources(final.repo))
        final.save(final.output)
        staged.complete(final.directory, final.output)
        contexts.append(final)
    for context in contexts:
        (context.directory / 'runtime/staged-result.json').write_bytes(
            candidate.canonical_json(context.output) + b' ' * 10000)
    put(fixture.root.parent / 'protocol_state.json', dict(dispatch=dict(tasks={
        '1': dict(status=dict(status='resolved'))})))
    before = {context.directory: staged.authenticate(context.directory, context.key) for context in contexts}
    originals = {path: path.read_bytes() for path in retained.eligible_paths(fixture.task_root)}
    assert retained.archive_terminal(fixture.root, fixture.task_root, max_files=100) == len(contexts) * 2
    for context in contexts:
        assert not (context.directory / 'runtime/staged-context.json').exists()
        assert staged.authenticate(context.directory, context.key) == before[context.directory]
        assert staged.reusable(context.directory, context.key) == context.output
    if with_finalizer:
        assert staged.donor(contexts[-1].directory / 'runtime')[0] == owned.directory
    for path, data in originals.items():
        assert retained.read_bytes(path) == data
        if retained._paths(path)[1].exists():
            retained.restore(path)
        assert path.read_bytes() == data
    for context in contexts:
        assert staged.authenticate(context.directory, context.key) == before[context.directory]


@pytest.mark.parametrize('fault', ['candidate', 'context', 'recipe', 'cleanup', 'response', 'sources', 'key'])
def test_mutated_or_incomplete_fact_evidence_never_reuses(fixture, fault):
    f = fixture.owned
    if fault == 'candidate':
        (fixture.task_root / 'candidate/files/Tablet/A.lean').write_text('forged')
    elif fault == 'context':
        put(f.directory / 'runtime/staged-context.json', {'forged': True})
    elif fault == 'recipe':
        recipe = json.loads((f.directory / 'execution/recipe.json').read_text())
        recipe['command'] = ['python', '-m', 'trellis.dispatch.acceptance']
        put(f.directory / 'execution/recipe.json', recipe)
    elif fault == 'cleanup':
        (f.directory / 'execution/cleanup.json').unlink()
    elif fault == 'response':
        put(f.directory / 'execution/response.json', dict(f.output, facts={'forged': True}))
    elif fault == 'sources':
        (f.repo / 'Tablet/A.lean').write_text('changed source')
    else:
        f.key = dict(f.key, task_binding_sha256='different task')
    assert staged.reusable(f.directory, f.key) is None


@pytest.mark.parametrize('field', ['response', 'outcome', 'acceptance_check_passed', 'output'])
def test_owned_stage_cannot_promote_a_full_acceptance_result(fixture, field):
    f = fixture.owned
    output = dict(f.output, **{field: {'outcome': 'Valid'}})
    f.save(output)
    with pytest.raises(candidate.CandidateError, match='cannot be a worker decision'):
        staged.authenticate(f.directory, f.key)


def test_live_proof_process_cannot_complete(fixture, monkeypatch):
    from trellis.dispatch import process
    monkeypatch.setattr(process, 'alive', lambda _: True)
    with pytest.raises(candidate.CandidateError, match='cleanup is not complete'):
        staged.authenticate(fixture.owned.directory, fixture.owned.key)


def test_fact_receipt_and_donor_keep_old_context_immutable(fixture):
    owned = fixture.owned
    before = {p: p.read_bytes() for p in owned.directory.rglob('*.json')}
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    marker = staged.prepare_donor(final.directory, owned.output)
    previous, actual, completed = staged.donor(final.directory / 'runtime')
    assert previous == owned.directory and actual == marker
    assert completed['key'] == owned.key
    assert final.key['acceptance_sha256'] != owned.key['acceptance_sha256']
    assert all(path.read_bytes() == data for path, data in before.items())


@pytest.mark.parametrize('fault', ['candidate_sha256', 'task_binding_sha256', 'implementation_identity', 'config'])
def test_donor_rejects_changed_task_candidate_config_or_implementation(fixture, fault):
    owned = fixture.owned
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    canonical = json.loads((final.directory / 'canonical.json').read_text())
    if fault == 'config':
        canonical['config_sha256'] = 'changed config'
    else:
        canonical['key'][fault] = 'changed binding'
    put(final.directory / 'canonical.json', canonical)
    with pytest.raises(candidate.CandidateError, match='changed|not compatible'):
        staged.prepare_donor(final.directory, owned.output)


def test_donor_has_no_caller_controlled_directory(fixture):
    owned = fixture.owned
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize')
    marker = staged.prepare_donor(final.directory, owned.output)
    marker['donor_round'] = '../../candidate'
    put(final.directory / 'staged-donor.json', marker)
    with pytest.raises(candidate.CandidateError, match='binding changed'):
        staged.donor(final.directory / 'runtime')


@pytest.mark.parametrize('fault', [None, 'pv_false', 'unknown', 'negative', 'pv', 'pv_null', 'pv_zero', 'pv_string', 'phase', 'facts_identity', 'authority', 'request_identity'])
@pytest.mark.parametrize('producer', sorted(staged.reviewed_owned_producers()))
def test_reviewed_b7_owned_facts_keep_original_receipt_and_require_current_contract(fixture, fault, producer):
    if fault == 'unknown':
        producer = 'trellis-worker-acceptance-v2:sha256:' + '0' * 64
    owned = fixture.prepare(fixture.task_root / 'acceptance/2', implementation=producer)
    if fault == 'negative':
        owned.output['status'] = 'rejected'
        owned.output['facts']['owner_ok'] = False
    elif fault == 'facts_identity':
        owned.output['facts']['acceptance_logic_identity'] = 'other'
    owned.save(owned.output)
    staged.complete(owned.directory, owned.output)
    original = {p: p.read_bytes() for p in owned.directory.rglob('*.json')}
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    if fault in ('pv', 'pv_false', 'pv_null', 'pv_zero', 'pv_string', 'phase', 'request_identity'):
        private = final.private
        field, value = {'pv': ('is_pv', True), 'pv_false': ('is_pv', False),
                        'pv_null': ('is_pv', None), 'pv_zero': ('is_pv', 0),
                        'pv_string': ('is_pv', 'false'), 'phase': ('phase', 'Modeling'),
                        'request_identity': ('acceptance_logic_identity', 'other')}[fault]
        private['in_flight_request'][field] = value
        put(final.directory / 'runtime/protocol_state.json', private)
    elif fault == 'authority':
        captured = json.loads((final.directory / 'canonical.json').read_text())
        captured['key']['authority_input_hashes'] = {'.trellis/scripts/check.py': 'changed'}
        put(final.directory / 'canonical.json', captured)
    if fault not in (None, 'pv_false'):
        with pytest.raises(candidate.CandidateError, match='not compatible'):
            staged.prepare_donor(final.directory, owned.output)
    else:
        marker = staged.prepare_donor(final.directory, owned.output)
        assert marker['producer_compatibility'] == dict(version=1, kind='reviewed_staged_owner_producer',
            producer_identity=producer, consumer_identity='test-implementation')
        assert staged.donor(final.directory / 'runtime')[1] == marker
        assert staged.reusable(owned.directory, owned.key) == owned.output
        assert all(path.read_bytes() == data for path, data in original.items())
        marker['producer_compatibility']['producer_identity'] = 'other'
        put(final.directory / 'staged-donor.json', marker)
        with pytest.raises(candidate.CandidateError, match='compatibility binding changed'):
            staged.donor(final.directory / 'runtime')


def test_final_completion_binds_current_worker_decision_and_original_donor(fixture):
    owned = fixture.owned
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    marker = staged.prepare_donor(final.directory, owned.output)
    output = dict(stage='finalize', status='checked', candidate_sha256=fixture.identity,
        donor_completion_sha256=marker['donor_completion_sha256'],
        output=dict(response={'outcome': 'Valid', 'request_id': 2}),
        checked_sources=receipts.checked_sources(final.repo))
    final.save(output)
    staged.complete(final.directory, output)
    assert staged.reusable(final.directory, final.key) == output
    changed = copy.deepcopy(owned.output)
    changed['facts']['owner_ok'] = False
    owned.save(changed)
    assert staged.reusable(final.directory, final.key) is None


@pytest.mark.parametrize('outcome', ['Invalid', 'Stuck'])
@pytest.mark.parametrize('corrupt', [False, True])
def test_completed_staged_negative_with_stale_dependency_retains_owner_and_finalizer(
        fixture, tmp_path, outcome, corrupt):
    from contextlib import nullcontext
    from trellis.dispatch.driver import ProductionDriver
    from trellis.dispatch.staged_driver import _flatten
    f = fixture
    final = f.prepare(f.owned.directory / 'finalization/1', stage='finalize', cycle=2)
    marker = staged.prepare_donor(final.directory, f.owned.output)
    output = dict(stage='finalize', status='checked', candidate_sha256=f.identity,
        donor_completion_sha256=marker['donor_completion_sha256'],
        output=dict(response=dict(status='Ok', outcome=outcome, request_id=2)),
        checked_sources=receipts.checked_sources(final.repo))
    final.save(output)
    staged.complete(final.directory, output)
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = f.root; driver.runtime = f.root.parent; driver.repo = tmp_path / 'canonical'
    (driver.repo / 'Tablet').mkdir(parents=True)
    (driver.repo / 'Tablet/A.lean').write_text('theorem A : True := by sorry\n')
    (driver.repo / 'Tablet/B.lean').write_text('new sibling proof')
    task = dict(id=1, assignment=dict(scope={'nodes': ['A']}, compiler='managed'),
        status=dict(status='checking', candidate_sha256=f.identity))
    source = json.loads((final.directory / 'canonical.json').read_text())['source']
    source['input_hashes']['Tablet/B.lean'] = candidate.digest((driver.repo / 'Tablet/B.lean').read_bytes())
    driver.state = lambda: {'dispatch': {'tasks': {'1': task}}}
    driver.require_no_halt = lambda: None
    driver.publisher = SimpleNamespace(locked=nullcontext)
    events = []
    def native(payload):
        assert payload['action'] == 'dispatch_control', 'Stale negative must precede private-state reconstruction'
        events.append(payload['event'])
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        observe_sources=lambda *_: source, _call=native)
    if corrupt:
        (final.directory / 'staged-completed.json').write_text('{}')
    pinned = {str(p): p.read_bytes() for p in f.root.rglob('*') if p.is_file()}
    if corrupt:
        with pytest.raises(candidate.CandidateError):
            driver._publish(1, f.identity, final.directory, _flatten(output), staged=True)
        assert events == []
    else:
        assert driver._publish(1, f.identity, final.directory, _flatten(output), staged=True) is True
        assert len(events) == 1 and events[0]['kind'] == 'attempt_stopping'
        assert events[0]['outcome']['kind'] == 'stale'
        assert 'Tablet/B.lean' in events[0]['outcome']['reason']
    assert {str(p): p.read_bytes() for p in f.root.rglob('*') if p.is_file()} == pinned


def test_merge_keeps_unrelated_current_sources_and_rejects_own_conflict_before_any_write(fixture, tmp_path):
    repo = tmp_path / 'current'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('theorem A : True := by sorry\n')
    (repo / 'Tablet/B.lean').write_text('new sibling proof')
    (repo / 'Tablet/B_Aux.lean').write_text('new sibling helper')
    captured = candidate.source_manifest(repo)
    staged.merge_delta(fixture.task_root / 'candidate', repo, captured)
    assert (repo / 'Tablet/A.lean').read_text() == 'theorem A : True := by trivial\n'
    assert (repo / 'Tablet/B.lean').read_text() == 'new sibling proof'
    assert (repo / 'Tablet/B_Aux.lean').read_text() == 'new sibling helper'
    final = candidate.source_manifest(repo)
    with pytest.raises(candidate.CandidateError, match='conflicts with current'):
        staged.merge_delta(fixture.task_root / 'candidate', repo, captured)
    assert candidate.source_manifest(repo) == final


def test_staged_donation_uses_existing_exact_import_transfer(fixture, monkeypatch):
    from trellis.dispatch import replay_donation
    owned = fixture.owned
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize')
    staged.prepare_donor(final.directory, owned.output)
    assert replay_donation._marker(final.directory / 'runtime')[0] == owned.directory
    put(owned.directory / 'execution/response.json', {'facts': 'forged'})
    with pytest.raises(candidate.CandidateError, match='observations changed'):
        replay_donation._marker(final.directory / 'runtime')


@pytest.mark.parametrize('middle, expected', [
    ('acceptance/1', True), ('acceptance/1/finalization/2', True),
    ('acceptance/1/finalization/nope', False), ('acceptance/nope/finalization/2', False),
    ('candidate/1/finalization/2', False), ('acceptance/1/worker/2', False),
])
def test_nested_finalizer_is_an_explicit_portable_capture_layout(tmp_path, middle, expected):
    from trellis.checker.portable_replay import acceptance_workspace
    repo = tmp_path / 'dispatch/1' / middle / 'repo/.trellis/supervisor/repo'
    assert acceptance_workspace(repo) is expected


def test_owned_producer_prepares_baseline_then_overlays_and_returns_only_native_facts(fixture, monkeypatch):
    import trellis.config
    import trellis.supervisor_workspace as supervisor
    from trellis.dispatch import acceptance
    f = fixture.owned
    (f.repo / 'Tablet/A.lean').write_text('theorem A : True := by sorry\n')
    (f.repo / 'Tablet/B.lean').write_text('current sibling proof')
    (f.repo / 'Tablet/B_Aux.lean').write_text('new sibling helper')
    (f.repo / 'Tablet/INDEX.md').write_text('current generated index')
    payload = dict(f.payload, captured_inputs=candidate.source_manifest(f.repo))
    sealed = candidate.read(fixture.task_root / 'candidate')[0]
    assert sealed['inputs']['Tablet/B.lean'] != payload['captured_inputs']['Tablet/B.lean']
    (f.directory / 'runtime/staged-context.json').unlink()
    config = SimpleNamespace(repo_path=f.repo, workflow=SimpleNamespace(paper_tex_path=None), goal_file=None)
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: config)
    monkeypatch.setattr(supervisor, 'sync_supervisor_workspace', lambda _: {'authoritative_repo_path': str(f.repo)})
    monkeypatch.setattr(supervisor, 'propagate_tablet_back_to_worker', lambda *args, mirror_build: None if mirror_build is False else pytest.fail('host must not mirror builds'))
    calls = []
    def native(payload):
        calls.append(payload)
        if payload['action'] == 'prepare_staged_worker_context':
            assert 'sorry' in (f.repo / 'Tablet/A.lean').read_text()
            return dict(status='prepared', context={'baseline': 'native staged baseline'})
        assert payload['action'] == 'check_staged_owned_proof'
        assert 'trivial' in (f.repo / 'Tablet/A.lean').read_text()
        assert (f.repo / 'Tablet/B.lean').read_text() == 'current sibling proof'
        assert (f.repo / 'Tablet/B_Aux.lean').read_text() == 'new sibling helper'
        assert (f.repo / 'Tablet/INDEX.md').read_text() == 'current generated index'
        return dict(status='facts', facts={'owner_nodes': ['A'], 'owner_ok': True})
    monkeypatch.setattr(staged, '_native', native)
    result = staged.check(payload)
    assert result['status'] == 'facts' and 'response' not in result
    assert [p['action'] for p in calls] == ['prepare_staged_worker_context', 'check_staged_owned_proof']


def test_unsupported_stage_does_not_enter_legacy_full_acceptance(fixture, monkeypatch):
    import trellis.config
    import trellis.supervisor_workspace as supervisor
    from trellis.dispatch import acceptance
    f = fixture.owned
    config = SimpleNamespace(repo_path=f.repo, workflow=SimpleNamespace(paper_tex_path=None), goal_file=None)
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: config)
    monkeypatch.setattr(supervisor, 'sync_supervisor_workspace', lambda _: {'authoritative_repo_path': str(f.repo)})
    monkeypatch.setattr(staged, '_native', lambda _: {'status': 'unsupported', 'reason': 'outside staged contract'})
    monkeypatch.setattr(acceptance, 'overlay', lambda *_: pytest.fail('unsupported input must not overlay'))
    monkeypatch.setattr(acceptance, 'check', lambda *_: pytest.fail('no implicit legacy full fallback'))
    assert staged.check(f.payload)['status'] == 'unsupported'


@pytest.mark.parametrize('environment_available', [True, False])
@pytest.mark.parametrize('donation_status', ['installed', 'installed_skipped', 'mismatch', 'write_failure', 'saved_binding', 'capture_binding'])
def test_finalizer_uses_current_baseline_then_exact_donor_and_environment(fixture, monkeypatch, environment_available, donation_status):
    import trellis.config
    import trellis.supervisor_workspace as supervisor
    from trellis.dispatch import replay_donation
    from trellis.dispatch import enrichment
    monkeypatch.setattr(enrichment, 'enrich', lambda _runtime, response: dict(response, enriched=True))
    owned = fixture.owned
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    staged.prepare_donor(final.directory, owned.output)
    (final.directory / 'runtime/staged-context.json').unlink()
    (final.repo / 'Tablet/A.lean').write_text('theorem A : True := by sorry\n')
    (final.repo / 'Tablet/B.lean').write_text('current sibling proof')
    config = SimpleNamespace(repo_path=final.repo, workflow=SimpleNamespace(paper_tex_path=None), goal_file=None)
    monkeypatch.setattr(trellis.config, 'load_config', lambda _: config)
    monkeypatch.setattr(supervisor, 'sync_supervisor_workspace', lambda _: {'authoritative_repo_path': str(final.repo)})
    monkeypatch.setattr(supervisor, 'propagate_tablet_back_to_worker', lambda *args, mirror_build: None if mirror_build is False else pytest.fail('host must not mirror builds'))
    calls = []
    def native(payload):
        calls.append(payload['action'])
        if payload['action'] == 'prepare_staged_worker_context':
            assert payload['current_private'] == final.private
            assert payload['request'] == final.private['in_flight_request']
            assert payload['owner_facts'] == owned.output['facts']
            assert 'sorry' in (final.repo / 'Tablet/A.lean').read_text()
            return dict(status='prepared', context={'current': True})
        assert environment_available
        assert payload['action'] == 'finalize_staged_worker'
        assert payload['current_environment'] == {'freshly_resolved': True}
        assert payload['owner_facts'] == owned.output['facts']
        assert payload['current_private'] == final.private
        assert (final.repo / 'Tablet/B.lean').read_text() == 'current sibling proof'
        return dict(status='checked', output={'response': {'outcome': 'Valid', 'request_id': 2}})
    def install(runtime):
        calls.append('install')
        assert runtime == final.directory / 'runtime'
        assert 'trivial' in (final.repo / 'Tablet/A.lean').read_text()
        if donation_status in ('installed', 'capture_binding'):
            return {'status': 'installed'}
        if donation_status == 'saved_binding':
            return {'status': 'cold', 'failure_class': 'saved_owner_binding',
                    'reason': 'saved owner imported environment changed: A'}
        reason = 'provenance write failed' if donation_status == 'write_failure' else 'import environment changed'
        return {'status': 'installed' if donation_status == 'installed_skipped' else 'cold',
                'skipped': {reason: 1}, 'reason': 'no replay artifacts retained: ' + reason}
    def capture(owners):
        calls.append('capture')
        assert owners == ['A']
        return {'environment': {'freshly_resolved': True}} if environment_available else dict(cold_reason='missing A.olean', measurement={'wall_secs': 12}, failure_detail={'returncode': 1}, **({'failure_class':'environment_binding'} if donation_status == 'capture_binding' else {}))
    monkeypatch.setattr(staged, '_native', native)
    monkeypatch.setattr(replay_donation, 'install_via_checker', install)
    monkeypatch.setattr(staged, '_capture_environment', capture)
    result = staged.check(final.payload)
    assert calls == ['prepare_staged_worker_context', 'install', 'capture'] + (
        ['finalize_staged_worker'] if environment_available else [])
    assert result['status'] == ('checked' if environment_available else 'cold')
    if not environment_available:
        assert 'missing A.olean' in result['reason']
        assert result['environment_capture']['measurement'] == {'wall_secs': 12}
        assert result['environment_capture']['failure_detail'] == {'returncode': 1}
        if donation_status in ('mismatch', 'saved_binding', 'capture_binding'):
            assert 'retry_class' not in result
            if donation_status != 'capture_binding': assert 'environment changed' in result['reason']
        else:
            assert result['retry_class'] == 'finalization_environment_capture'
    assert staged.reusable(owned.directory, owned.key) == owned.output


def test_publisher_authenticates_only_this_task_finalization(fixture, tmp_path, monkeypatch):
    from trellis.dispatch import publication_artifacts
    from trellis.dispatch.publication import Publisher
    owned = fixture.owned
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    marker = staged.prepare_donor(final.directory, owned.output)
    output = dict(stage='finalize', status='checked', candidate_sha256=fixture.identity,
        donor_completion_sha256=marker['donor_completion_sha256'],
        output={'response': {'outcome': 'Valid'}}, checked_sources=receipts.checked_sources(final.repo))
    final.save(output)
    staged.complete(final.directory, output)
    repo = tmp_path / 'canonical'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('theorem A : True := by sorry\n')
    publisher = Publisher(fixture.root.parent, repo)
    calls = []
    monkeypatch.setattr(publication_artifacts, 'stage', lambda *args: calls.append(args) or {'version': 1, 'nodes': {}})
    identity = publisher.prepare(1, fixture.identity, final.repo,
        expected_manifest=candidate.source_manifest(repo), kernel_event={'response': {'outcome': 'Valid'}},
        artifact_source=final.directory)
    assert calls[0][0] == final.repo
    assert json.loads((publisher.root / identity / 'record.json').read_text())['version'] == 2
    (publisher.root / 'pending.json').unlink()
    with pytest.raises(candidate.CandidateError, match='not this task host acceptance'):
        publisher.prepare(2, fixture.identity, final.repo,
            expected_manifest=candidate.source_manifest(repo), kernel_event={'response': {}},
            artifact_source=final.directory)
    (final.directory / 'staged-completed.json').unlink()
    with pytest.raises(candidate.CandidateError, match='no intact staged'):
        publisher.prepare(1, fixture.identity, final.repo,
            expected_manifest=candidate.source_manifest(repo), kernel_event={'response': {}},
            artifact_source=final.directory)


_F3 = 'trellis-worker-acceptance-v2:sha256:37fa50a178c6eec123e7c6512a01f2d778b68bb4eb314c922199dad0bc544a34'
_B7 = 'trellis-worker-acceptance-v2:sha256:62a0e7fa5977c48a8b8622cef1d7327787c00482c10852a4e4836a7414806378'


def _historical_final(fixture, monkeypatch, status='cold', producer=_F3, positive=False):
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: producer)
    owned = fixture.prepare(fixture.task_root / 'acceptance/2', implementation=_B7)
    staged.complete(owned.directory, owned.output)
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    marker = staged.prepare_donor(final.directory, owned.output)
    output = dict(stage='finalize', status=status, candidate_sha256=fixture.identity,
        donor_completion_sha256=marker['donor_completion_sha256'],
        reason='staged normalization changed source; owned facts remain retained',
        replay_donation={'status': 'installed'},
        checked_sources=receipts.checked_sources(final.repo))
    if status == 'checked':
        output['output'] = {'response': {'outcome': 'Valid', 'request_id': 2,
            **({'status': 'Ok', 'acceptance_check_passed': True} if positive else {})}}
    final.save(output)
    staged.complete(final.directory, output)
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'new-current')
    return owned, final, output


@pytest.mark.parametrize('status', ['cold', 'checked'])
def test_historical_cold_authentication_is_not_current_donor_or_old_acceptance(fixture, monkeypatch, status):
    owned, final, output = _historical_final(fixture, monkeypatch, status)
    original = {p: p.read_bytes() for p in owned.directory.rglob('*.json')}
    assert (staged.reusable(final.directory, final.key) == output) is (status == 'cold')
    with pytest.raises(candidate.CandidateError, match='not compatible'):
        staged.donor(final.directory / 'runtime')
    with pytest.raises(candidate.CandidateError, match='not compatible'):
        staged.prepare_donor(final.directory, owned.output)
    assert all(p.read_bytes() == data for p, data in original.items())


@pytest.mark.parametrize('fault', ['marker', 'cleanup', 'result', 'response', 'source', 'context'])
def test_historical_cold_keeps_all_original_completion_bindings(fixture, monkeypatch, fault):
    _owned, final, _output = _historical_final(fixture, monkeypatch)
    if fault == 'marker':
        marker = json.loads((final.directory / 'staged-donor.json').read_text())
        marker['producer_compatibility']['consumer_identity'] = 'new-current'
        put(final.directory / 'staged-donor.json', marker)
    elif fault == 'source':
        (final.repo / 'Tablet/A.lean').write_text('different source')
    elif fault == 'context':
        # Existing response need not have a context hash, but the process
        # request is always part of the exact completion receipt.
        put(final.directory / 'runtime/protocol_state.json', {'in_flight_request': {}})
    else:
        (final.directory / f'execution/{fault}.json').unlink()
    assert staged.reusable(final.directory, final.key) is None


@pytest.mark.parametrize('fault', [None, 'authored', 'header', 'dependency', 'toolchain', 'authority',
                                  'config', 'cleanup', 'reason', 'producer', 'donation'])
@pytest.mark.parametrize('producer', [_F3, 'trellis-worker-acceptance-v2:sha256:c1f83ea36e761ae6ad4c9139bced3bd75f63a63011b55b9010fc2a7e3be897a7'])
def test_reviewed_source_order_retry_requires_exact_sealed_authored_and_host_inputs(fixture, monkeypatch, fault, producer):
    from trellis.dispatch import staged_driver
    owned, final, output = _historical_final(fixture, monkeypatch, producer=producer)
    # Recreate a genuine host completion whose optional generated docs were
    # overwritten by cold sync. All mathematical and host input hashes remain
    # bound to the captured base plus the sealed delta.
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: producer)
    (final.directory / 'staged-completed.json').unlink()
    captured = json.loads((final.directory / 'canonical.json').read_text())
    prepared = json.loads((final.directory / 'prepared.json').read_text())
    prepared['prepared_config_sha256'] = candidate.digest((final.repo / 'trellis.config.json').read_bytes())
    put(final.directory / 'prepared.json', prepared)
    (final.repo / 'lean-toolchain').write_text('leanprover/lean4:exact')
    script = final.repo / '.trellis/scripts/check.py'
    script.parent.mkdir(parents=True)
    script.write_text('host checker input')
    captured['source']['input_hashes']['lean-toolchain'] = candidate.digest((final.repo / 'lean-toolchain').read_bytes())
    captured['source']['input_hashes']['trellis.config.json'] = candidate.digest(b'canonical configuration')
    captured['key']['input_hashes'] = captured['source']['input_hashes']
    captured['source']['authority_inputs'] = {'.trellis/scripts/check.py': candidate.digest(script.read_bytes())}
    captured['key']['authority_input_hashes'] = captured['source']['authority_inputs']
    # The owned authority map must be the same exact captured host authority.
    old_capture = json.loads((owned.directory / 'canonical.json').read_text())
    old_capture['key']['authority_input_hashes'] = captured['source']['authority_inputs']
    old_capture['source']['authority_inputs'] = captured['source']['authority_inputs']
    put(owned.directory / 'canonical.json', old_capture)
    owned.key = old_capture['key']
    (owned.directory / 'staged-completed.json').unlink()
    staged.complete(owned.directory, owned.output)
    put(final.directory / 'canonical.json', captured)
    final.key = captured['key']
    recipe = json.loads((final.directory / 'execution/recipe.json').read_text())
    recipe['payload']['captured_inputs'] = captured['source']['input_hashes']
    put(final.directory / 'execution/recipe.json', recipe)
    (final.directory / 'staged-donor.json').unlink()
    marker = staged.prepare_donor(final.directory, owned.output)
    output['donor_completion_sha256'] = marker['donor_completion_sha256']
    for name in ('INDEX.md', 'README.md'):
        (final.repo / 'Tablet' / name).write_text('stale generated support')
    faults = {'authored': 'Tablet/A.lean', 'header': 'Tablet/header.tex', 'dependency': 'Tablet/B.lean',
              'toolchain': 'lean-toolchain', 'authority': '.trellis/scripts/check.py', 'config': 'trellis.config.json'}
    if fault in faults:
        (final.repo / faults[fault]).write_text('{"changed":true}' if fault == 'config' else 'changed during old check')
    authority = final.repo / '.trellis/supervisor/repo'
    import shutil
    for relative in ['Tablet', 'lean-toolchain', 'trellis.config.json', '.trellis/scripts']:
        source = final.repo / relative
        dest = authority / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir(): shutil.copytree(source, dest)
        else: shutil.copyfile(source, dest)
    (authority / '.trellis-supervisor.json').write_text('{"host_projection":true}')
    def full_inputs(repo):
        result = candidate.source_manifest(repo)
        for relative in ['lean-toolchain', 'trellis.config.json', '.trellis/scripts/check.py', '.trellis-supervisor.json']:
            path = Path(repo) / relative
            if path.exists(): result[relative] = candidate.digest(path.read_bytes())
        return result
    monkeypatch.setattr(receipts, 'input_hashes', full_inputs)
    output['checked_sources'] = receipts.checked_sources(final.repo)
    if fault == 'reason': output['reason'] += ': changed authored source'
    if fault == 'donation': output['replay_donation']['status'] = 'cold'
    final.save(output)
    staged.complete(final.directory, output)
    original = {p: p.read_bytes() for p in owned.directory.rglob('*.json')}
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'new-current')
    if fault == 'cleanup': (final.directory / 'execution/cleanup.json').unlink()
    if fault == 'producer': captured['key']['implementation_identity'] = 'unreviewed'
    assert staged_driver._normalization_projection_retry(final.directory, captured, output) is (fault is None)
    if fault is None:
        assert staged_driver._retryable_finalization(final.directory, output)
    if fault != 'cleanup':
        assert all(p.read_bytes() == data for p, data in original.items())
    assert not (owned.directory / 'retry-authorized.json').exists()


@pytest.fixture
def owned_cold(fixture, monkeypatch):
    f = fixture.owned
    (f.directory / 'staged-completed.json').unlink()
    captured = json.loads((f.directory / 'canonical.json').read_text())
    prepared = json.loads((f.directory / 'prepared.json').read_text())
    prepared['prepared_config_sha256'] = candidate.digest((f.repo / 'trellis.config.json').read_bytes())
    put(f.directory / 'prepared.json', prepared)
    script = f.repo / '.trellis/scripts/check.py'
    script.parent.mkdir(parents=True)
    script.write_text('host checker input')
    captured['source']['authority_inputs'] = {'.trellis/scripts/check.py': candidate.digest(script.read_bytes())}
    captured['key']['authority_input_hashes'] = captured['source']['authority_inputs']
    put(f.directory / 'canonical.json', captured)
    f.key = captured['key']
    import shutil
    authority = f.repo / '.trellis/supervisor/repo'
    for relative in ('Tablet', 'trellis.config.json', '.trellis/scripts'):
        source, target = f.repo / relative, authority / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir(): shutil.copytree(source, target)
        else: shutil.copyfile(source, target)
    (authority / '.trellis-supervisor.json').write_text('{"host_projection":true}')
    def inputs(repo):
        result = candidate.source_manifest(repo)
        for name in ('trellis.config.json', '.trellis/scripts/check.py', '.trellis-supervisor.json'):
            path = Path(repo) / name
            if path.exists(): result[name] = candidate.digest(path.read_bytes())
        return result
    monkeypatch.setattr(receipts, 'input_hashes', inputs)
    f.output = {k: v for k, v in f.output.items() if k != 'facts'}
    f.output.update(status='cold', retry_class='owned_environment_capture',
                    reason='staged owner environment unavailable (before_observations): missing inventory',
                    checked_sources=receipts.checked_sources(f.repo))
    f.save(f.output)
    staged.complete(f.directory, f.output)
    return fixture


@pytest.mark.parametrize('fault', [None, 'old_producer', 'same_old_producer', 'reason', 'source', 'header',
                                  'authority', 'config', 'facts', 'rejected', 'cleanup', 'response', 'completion'])
def test_owned_capture_retry_authenticates_drain_and_exact_authored_inputs(owned_cold, monkeypatch, fault):
    from trellis.dispatch import staged_driver
    f = owned_cold.owned
    if fault in ('old_producer', 'same_old_producer', 'reason'):
        (f.directory / 'staged-completed.json').unlink()
        f.output.pop('retry_class')
        f.output['reason'] = 'staged owner environment unavailable before successful observations'
        if fault == 'reason': f.output['reason'] += ': changed source'
        f.save(f.output)
        staged.complete(f.directory, f.output)
        if fault != 'same_old_producer': monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'current-update')
    if fault in ('source', 'header', 'authority', 'config', 'facts', 'rejected'):
        (f.directory / 'staged-completed.json').unlink()
        if fault == 'facts': f.output['facts'] = {'owner_ok': True}
        elif fault == 'rejected': f.output.update(status='rejected', facts={'owner_ok': False})
        else:
            name = {'source': 'Tablet/A.lean', 'header': 'Tablet/header.tex',
                    'authority': '.trellis/scripts/check.py', 'config': 'trellis.config.json'}[fault]
            (f.repo / name).write_text('{"changed":true}' if fault == 'config' else 'changed during check')
            f.output['checked_sources'] = receipts.checked_sources(f.repo)
        f.save(f.output)
        staged.complete(f.directory, f.output)
    elif fault == 'cleanup': (f.directory / 'execution/cleanup.json').unlink()
    elif fault == 'response': put(f.directory / 'execution/response.json', dict(f.output, reason='forged'))
    elif fault == 'completion': put(f.directory / 'staged-completed.json', {'forged': True})
    assert staged_driver._retryable_owned(f.directory, f.output) is (fault in (None, 'old_producer'))
    assert not (f.directory / 'retry-authorized.json').exists()


@pytest.mark.parametrize('fault', [None, 'completion', 'cleanup', 'source', 'forged_marker', 'config', 'worker_recipe', 'retry'])
def test_owned_cold_artifact_donor_cannot_supply_facts_or_forged_replay(owned_cold, monkeypatch, fault):
    from trellis.dispatch import staged_driver, replay_donation
    f = owned_cold.owned
    current = owned_cold.prepare(f.directory.parent / '2')
    captured = json.loads((current.directory / 'canonical.json').read_text())
    captured['staged_plan'] = {'owner_nodes': ['A']}
    captured['key']['authority_input_hashes'] = f.key['authority_input_hashes']
    captured['source']['authority_inputs'] = f.key['authority_input_hashes']
    put(current.directory / 'canonical.json', captured)
    put(f.directory / 'retry-authorized.json', {'kind':'bounded_infrastructure_retry',
        'event': {'kind':'check_retry', 'task_id':1, 'candidate_sha256':owned_cold.identity, 'retry':1}})
    original = (f.directory / 'execution/response.json').read_bytes()
    staged_driver._seed_owned_retry_artifacts(current.directory)
    marker_path = current.directory / 'replay-cache-donor.json'
    marker = json.loads(marker_path.read_text())
    assert marker['donor_kind'] == 'owned_capture_retry'
    if fault == 'completion': put(f.directory / 'staged-completed.json', {})
    elif fault == 'cleanup': (f.directory / 'execution/cleanup.json').unlink()
    elif fault == 'source': (f.repo / 'Tablet/A.lean').write_text('forged replay source')
    elif fault == 'forged_marker': put(marker_path, dict(marker, owner_nodes=['Other']))
    elif fault == 'config':
        captured['config_sha256'] = 'changed'
        put(current.directory / 'canonical.json', captured)
    elif fault == 'worker_recipe':
        recipe = json.loads((f.directory / 'execution/recipe.json').read_text())
        recipe['command'] = ['python', '-m', 'trellis.dispatch.worker']
        put(f.directory / 'execution/recipe.json', recipe)
    elif fault == 'retry': (f.directory / 'retry-authorized.json').unlink()
    if fault is None:
        previous, actual, completed = replay_donation._marker(current.directory / 'runtime')
        assert previous == f.directory and actual == marker
        witness = dict(status='installed', artifact_files={'exact':'binding'}, **marker)
        assert replay_donation.validation_fact_donor(current.directory / 'runtime', witness) is None
        assert (f.directory / 'execution/response.json').read_bytes() == original
    else:
        with pytest.raises((candidate.CandidateError, KeyError, OSError)):
            replay_donation._marker(current.directory / 'runtime')


@pytest.mark.parametrize('complete', [False, True])
def test_owned_retry_donor_preserves_legacy_unless_all_owner_replays_portable(owned_cold, monkeypatch, complete):
    from trellis.dispatch import staged_driver
    from trellis.atomic_actions import observations as obs
    f = owned_cold.owned
    current = owned_cold.prepare(f.directory.parent / '2')
    captured = json.loads((current.directory / 'canonical.json').read_text())
    captured['staged_plan'] = {'owner_nodes':['A', 'B']}
    captured['key']['authority_input_hashes'] = f.key['authority_input_hashes']
    captured['source']['authority_inputs'] = f.key['authority_input_hashes']
    put(current.directory / 'canonical.json', captured)
    put(current.directory / 'legacy-owner-donor.json', {'unchanged':'existing authenticated donor'})
    put(f.directory / 'retry-authorized.json', {'kind':'bounded_infrastructure_retry',
        'event':{'kind':'check_retry', 'task_id':1, 'candidate_sha256':owned_cold.identity, 'retry':1}})
    monkeypatch.setattr(obs, '_read_olean_provenance', lambda repo, node:
        {'kernel_replay': {'portable_environment_sha256':'a'*64}} if complete or node == 'A' else {})
    monkeypatch.setattr(obs, 'olean_is_content_current', lambda *a, **k: True)
    monkeypatch.setattr(obs, 'olean_has_current_kernel_replay', lambda *a, **k: True)
    staged_driver._seed_owned_retry_artifacts(current.directory)
    assert (current.directory / 'replay-cache-donor.json').exists() is complete
    assert json.loads((current.directory / 'legacy-owner-donor.json').read_text()) == {'unchanged':'existing authenticated donor'}


_FINALIZER_28A = 'trellis-worker-acceptance-v2:sha256:8871145c937e3236d72ba2d7f584f94b1ee24de2de3e15262a528a76d03f3cea'


def test_reviewed_positive_finalizer_reauthenticates_without_rewriting_history(fixture, monkeypatch):
    owned, final, output = _historical_final(fixture, monkeypatch, 'checked', _FINALIZER_28A, positive=True)
    saved = {p: p.read_bytes() for p in owned.directory.rglob('*.json')}
    assert staged.reusable(final.directory, final.key) is None
    assert staged.reusable_finalizer(final.directory, final.key) == output
    assert staged.reusable_finalizer(owned.directory, owned.key) == owned.output
    with pytest.raises(candidate.CandidateError, match='not compatible'):
        staged.donor(final.directory / 'runtime')
    assert all(p.read_bytes() == data for p, data in saved.items())
    from trellis.dispatch import staged_publication
    from trellis.dispatch.staged_driver import _flatten
    capture = json.loads((final.directory / 'canonical.json').read_text())
    private = copy.deepcopy(final.private)
    private['in_flight_request']['acceptance_logic_identity'] = 'new-current'
    current_key = dict(final.key, implementation_identity='new-current', acceptance_sha256='fresh-state')
    checked = _flatten(output)
    adoption = staged_publication.compatible(final.directory, capture, current_key, private,
                                             checked, capture['source'])
    assert adoption['decision']['producer_identity'] == _FINALIZER_28A
    task = dict(assignment=dict(assignment_id='task60', input_hashes={'Tablet/B.lean': 'old-imports'}))
    basis = staged_publication.fresh_basis(final.directory, capture, current_key, private,
                                          capture['source'], checked, task)
    assert basis['completion_sha256'] == receipts.regular_digest(final.directory / 'staged-completed.json')
    assert basis['inputs_sha256'] != basis['assignment_inputs_sha256']
    # The source evidence contains the compiler-observed body/import bytes,
    # not a principal or unchanged-prefix digest.
    for path in ['Tablet/B.lean', 'Tablet/B.tex', 'trellis.config.json', '.trellis/scripts/check.py']:
        drift = copy.deepcopy(capture['source']); drift['input_hashes'][path] = 'appeared-or-changed'
        assert staged_publication.fresh_basis(final.directory, capture, current_key, private, drift, checked, task) is None
    authority = copy.deepcopy(capture['source']); authority['authority_inputs']['.trellis/scripts/check.py'] = 'changed'
    assert staged_publication.fresh_basis(final.directory, capture, current_key, private, authority, checked, task) is None
    changed = copy.deepcopy(capture['source']); changed['input_hashes']['Tablet/B.lean'] = 'new-again'
    assert staged_publication.fresh_basis(final.directory, capture, current_key, private, changed, checked, task) is None
    assert staged_publication.compatible(final.directory, capture, current_key, private, checked, changed) is None
    with pytest.raises(candidate.CandidateError, match='task binding'):
        staged_publication.fresh_basis(final.directory, capture, dict(current_key, task_binding_sha256='changed'),
                                       private, capture['source'], checked, task)


@pytest.mark.parametrize('fault', ['completion', 'cleanup', 'result', 'source', 'donor', 'candidate'])
def test_reviewed_positive_finalizer_never_accepts_mutated_evidence(fixture, monkeypatch, fault):
    _owned, final, _output = _historical_final(fixture, monkeypatch, 'checked', _FINALIZER_28A, positive=True)
    if fault == 'source':
        (final.repo / 'Tablet/B.lean').write_text('changed secondary definition')
    elif fault == 'candidate':
        (fixture.task_root / 'candidate/files/Tablet/A.lean').write_text('changed own proof')
    else:
        path = {'completion': 'staged-completed.json', 'cleanup': 'execution/cleanup.json',
                'result': 'execution/result.json', 'donor': 'staged-donor.json'}[fault]
        put(final.directory / path, {'changed': True})
    assert staged.reusable_finalizer(final.directory, final.key) is None


def test_unreviewed_or_negative_finalizer_never_gets_positive_compatibility(fixture, monkeypatch):
    _owned, final, _output = _historical_final(fixture, monkeypatch, 'checked', _F3, positive=True)
    assert staged.reusable_finalizer(final.directory, final.key) is None
    private = dict(phase='ProofFormalization', in_flight_request={'acceptance_logic_identity': _FINALIZER_28A})
    for response in [dict(status='Ok', outcome='Invalid', acceptance_check_passed=False),
                     dict(status='Ok', outcome='Valid', acceptance_check_passed=False),
                     dict(status='Ok', outcome='Valid', acceptance_check_passed=True, transport_failure=True)]:
        assert not staged.reviewed_positive_finalizer(_FINALIZER_28A, {'status': 'checked', 'output': {'response': response}}, private)


def test_reviewed_finalizer_adoption_never_launches_another_check(fixture, monkeypatch):
    from trellis.dispatch import staged_driver
    _owned, final, output = _historical_final(fixture, monkeypatch, 'checked', _FINALIZER_28A, positive=True)
    def forbidden(*args, **kwargs):
        pytest.fail('completed F4 must not launch an owner or finalizer again')
    driver = SimpleNamespace(_process=forbidden, _permit=forbidden, _wait=forbidden)
    import asyncio
    assert asyncio.run(staged_driver._execute(driver, final.directory, 1, fixture.identity, 'finalize')) == output


def test_reviewed_finalizer_retains_authenticated_artifact_handoff(fixture, tmp_path, monkeypatch):
    from trellis.dispatch import publication_artifacts
    from trellis.dispatch.publication import Publisher
    _owned, final, output = _historical_final(fixture, monkeypatch, 'checked', _FINALIZER_28A, positive=True)
    repo = tmp_path / 'canonical'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('theorem A : True := by sorry\n')
    publisher = Publisher(fixture.root.parent, repo)
    calls = []
    monkeypatch.setattr(publication_artifacts, 'stage', lambda *args: calls.append(args) or {'version': 1, 'nodes': {}})
    publisher.prepare(1, fixture.identity, final.repo,
        expected_manifest=candidate.source_manifest(repo), kernel_event=output['output'],
        artifact_source=final.directory)
    assert len(calls) == 1
    assert staged.reusable_finalizer(final.directory, final.key) == output


def modeling_decline(fixture):
    """A genuine authenticated pre-overlay stage result, with no owned facts."""
    f = fixture.prepare(fixture.task_root / 'acceptance/2')
    private = dict(f.private, phase='Modeling')
    private['in_flight_request'] = dict(private['in_flight_request'], phase='Modeling')
    captured = json.loads((f.directory / 'canonical.json').read_text())
    key = dict(f.key, acceptance_sha256=acceptance_identity(private))
    captured.update(key=key, acceptance_sha256=key['acceptance_sha256'],
                    staged_plan={'read_nodes': ['A', 'B'], 'write_nodes': ['A']})
    put(f.directory / 'canonical.json', captured)
    put(f.directory / 'runtime/protocol_state.json', private)
    put(f.directory / 'prepared.json', dict(identity=candidate.digest(candidate.canonical_json(
        dict(state=private, sources=fixture.before, config_sha256=captured['config_sha256'])))))
    output = dict(stage='owned', status='unsupported', fallback_class='ordinary_modeling_baseline',
                  candidate_sha256=fixture.identity, checked_sources=receipts.checked_sources(f.repo))
    f.save(output)
    staged.complete(f.directory, output)
    return SimpleNamespace(**dict(vars(f), private=private, key=key, output=output))


def test_modeling_baseline_decline_reenters_real_exclusive_ordinary_round(fixture, monkeypatch):
    import asyncio
    from trellis.dispatch import driver as driver_module, staged_driver
    f = modeling_decline(fixture)
    original = {p: p.read_bytes() for p in f.directory.rglob('*.json')}
    driver = driver_module.ProductionDriver.__new__(driver_module.ProductionDriver)
    driver.root = fixture.root
    driver.finalization_gate = asyncio.Lock()
    driver.acceptance_queue = SimpleNamespace(active={1: SimpleNamespace(exclusive=False)})
    task = {'id': 1, 'assignment': {'scope': {'exclusive': False}}}
    driver._check_candidate_scope = lambda *_: (task, fixture.task_root / 'candidate', None)
    async def inline(function, *args, **kwargs):
        return function(*args, **kwargs)
    monkeypatch.setattr(driver_module, '_run_blocking', inline)
    assert asyncio.run(driver._integrate_reserved(1, fixture.identity)) == 'readmit'
    driver.acceptance_queue.active[1].exclusive = True
    checked = {'ok': True, 'response': {'outcome': 'Valid'}}
    events = []
    def prepare(task_id, ordinal, identity, rejected):
        assert ordinal == 3 and driver.finalization_gate.locked()
        events.append(('fresh-full-gate', ordinal))
        return fixture.task_root / 'acceptance/3', task, fixture.before, checked
    driver._prepare_acceptance = prepare
    driver._process = lambda *_args, **_kwargs: pytest.fail('must not reuse the staged recipe')
    monkeypatch.setattr(receipts, 'complete', lambda directory, result: events.append(('complete', directory.name)))
    monkeypatch.setattr(staged_driver, 'plan', lambda *_: {'status': 'unsupported'})
    driver._publish = lambda _id, _identity, directory, _result: events.append(('publish', directory.name)) or True
    asyncio.run(driver._integrate_reserved(1, fixture.identity))
    assert events == [('fresh-full-gate', 3), ('complete', '3'), ('publish', '3')]
    assert original == {p: p.read_bytes() for p in f.directory.rglob('*.json')}


def test_modeling_fallback_is_conservative_after_sibling_and_context_changes(fixture):
    from trellis.dispatch import staged_driver
    f = modeling_decline(fixture)
    driver = SimpleNamespace(root=fixture.root)
    task = dict(id=1, assignment={'scope': {'nodes': ['A']}, 'compiler': 'managed'})
    current = dict(f.private, reviewer_comments='new scheduling report')
    changed_sources = {'input_hashes': dict(fixture.before, **{'Tablet/B.lean': 'new sibling'})}
    assert staged_driver._ordinary_modeling_fallback(driver, task, changed_sources, current, fixture.identity)
    # The hint grants no observation reuse; corrupted old completion stays fatal.
    (f.directory / 'execution/cleanup.json').unlink()
    with pytest.raises(candidate.CandidateError, match='fallback evidence changed'):
        staged_driver._ordinary_modeling_fallback(driver, task, changed_sources, current, fixture.identity)


@pytest.mark.parametrize('fault', [None, 'same_producer', 'phase', 'reason', 'facts', 'source', 'cleanup'])
def test_historical_initial_binding_cold_retries_only_complete_exact_retained_owner(owned_cold, monkeypatch, fault):
    from trellis.dispatch import staged_driver
    f = owned_cold.owned
    (f.directory / 'staged-completed.json').unlink()
    f.output.pop('retry_class')
    f.output.update(capture_phase='after_observations',
        reason='staged owner environment unavailable (after_observations): import resolution changed before initial content binding')
    if fault == 'phase': f.output['capture_phase'] = 'before_observations'
    if fault == 'reason': f.output['reason'] += '; other change'
    if fault == 'facts': f.output['facts'] = {'owner_ok': True}
    if fault == 'source':
        (f.repo / 'Tablet/A.lean').write_text('changed source')
        f.output['checked_sources'] = receipts.checked_sources(f.repo)
    f.save(f.output)
    staged.complete(f.directory, f.output)
    if fault != 'same_producer': monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'updated-host')
    if fault == 'cleanup': (f.directory / 'execution/cleanup.json').unlink()
    assert staged_driver._retryable_owned(f.directory, f.output) is (fault is None)
    assert not (f.directory / 'retry-authorized.json').exists()


@pytest.mark.parametrize("compact_old,compact_new", [(False, True), (True, False), (True, True)])
def test_compact_owned_donor_finalization_restart_keeps_physical_evidence(fixture, compact_old, compact_new):
    from trellis.history_artifacts import encode_shared_state
    owned = fixture.owned
    if compact_old:
        put(owned.directory / 'runtime/protocol_state.json', encode_shared_state({'state': owned.private}))
        (owned.directory / 'staged-completed.json').unlink()
        staged.complete(owned.directory, owned.output)
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    if compact_new:
        put(final.directory / 'runtime/protocol_state.json', encode_shared_state({'state': final.private}))
    originals = {p: p.read_bytes() for context in [owned, final] for p in
        [context.directory / 'runtime/protocol_state.json', context.directory / 'prepared.json']}
    marker = staged.prepare_donor(final.directory, owned.output)
    final.output = dict(stage='finalize', status='checked', candidate_sha256=fixture.identity,
        context_sha256=candidate.digest((final.directory / 'runtime/staged-context.json').read_bytes()),
        donor_completion_sha256=marker['donor_completion_sha256'],
        output=dict(response={'outcome': 'Valid', 'request_id': 2}),
        checked_sources=receipts.checked_sources(final.repo))
    final.save(final.output)
    staged.complete(final.directory, final.output)
    for _ in range(2):
        assert staged.reusable(final.directory, final.key) == final.output
        assert staged.donor(final.directory / 'runtime')[0] == owned.directory
    assert all(p.read_bytes() == raw for p, raw in originals.items())


def test_archived_owned_response_remains_exact_donor_and_fallback(fixture, monkeypatch):
    from trellis.dispatch import retained_files as retained, staged_driver
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    owned = fixture.owned
    path = owned.directory / 'execution/response.json'
    before = staged.authenticate(owned.directory, owned.key)
    assert retained.archive(path)
    assert staged.authenticate(owned.directory, owned.key) == before
    assert staged.reusable(owned.directory, owned.key) == owned.output
    final = fixture.prepare(owned.directory / 'finalization/1', stage='finalize', cycle=2)
    marker = staged.prepare_donor(final.directory, owned.output)
    assert marker['donor_completion_sha256']
    assert staged.donor(final.directory / 'runtime')[0] == owned.directory


def test_archived_modeling_decline_presence_and_corruption_are_not_silently_ignored(fixture, monkeypatch):
    from trellis.dispatch import retained_files as retained, staged_driver
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    f = modeling_decline(fixture)
    path = f.directory / 'execution/response.json'
    assert retained.archive(path)
    assert staged_driver.completed_modeling_fallback(f.directory)
    driver = SimpleNamespace(root=fixture.root)
    task = dict(id=1, assignment={'scope': {'nodes': ['A']}, 'compiler': 'managed'})
    assert staged_driver._ordinary_modeling_fallback(driver, task, {'input_hashes':fixture.before},
                                                   f.private, fixture.identity)
    retained._paths(path)[0].unlink()
    with pytest.raises((OSError, ValueError, candidate.CandidateError)):
        staged_driver.completed_modeling_fallback(f.directory)

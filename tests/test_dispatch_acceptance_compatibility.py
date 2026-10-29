import copy
import json
from types import SimpleNamespace

import pytest

from trellis.dispatch import acceptance_compatibility as compatibility
from trellis.dispatch import acceptance_receipts as receipts, candidate
from test_dispatch_acceptance_receipts import completed, put


@pytest.fixture
def adoption(tmp_path, monkeypatch):
    producer = sorted(compatibility.reviewed_producers())[0]
    consumer = 'trellis-worker-acceptance-v2:sha256:' + 'f' * 64
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: consumer)
    carriers = {'.trellis/scripts/check.py': '1' * 64, '.trellis/artifacts/policy.json': '2' * 64}
    key = dict(version=1, task_id=7, candidate_sha256='3' * 64, submission_turn=2,
        task_binding_sha256='4' * 64, implementation_identity=producer,
        acceptance_sha256='5' * 64, input_hashes={'Tablet/A.lean': '6' * 64})
    current_key = dict(key, implementation_identity=consumer, acceptance_sha256='7' * 64,
                       authority_input_hashes=carriers)
    captured = dict(key=key, source=dict(graph={'edges': {'A': []}}, input_hashes=key['input_hashes']))
    original = dict(in_flight_request=dict(acceptance_logic_identity=producer))
    current = dict(in_flight_request=dict(acceptance_logic_identity=consumer), worker_execution={})
    checked = dict(response=dict(status='Ok', outcome='Valid', acceptance_check_passed=True),
        checked_sources=dict(inputs=dict(carriers), authority_inputs=dict(carriers)))
    monkeypatch.setattr(receipts, 'reusable', lambda directory, expected: checked if expected == key else None)
    (tmp_path / 'completed.json').write_text('{"original":"completion"}\n')
    return SimpleNamespace(directory=tmp_path, captured=captured, current_key=current_key,
        original=original, current=current, checked=checked, source=dict(authority_inputs=carriers))


def prepare(f):
    return compatibility.prepare(f.directory, f.captured, f.current_key, f.original,
                                 f.current, f.checked, f.source)


def test_compatibility_recovers_original_carriers_without_rewriting_evidence(adoption):
    f = adoption
    before = copy.deepcopy(vars(f))
    raw = (f.directory / 'completed.json').read_bytes()
    result = prepare(f)
    assert result['original_source']['authority_inputs'] == f.source['authority_inputs']
    assert result['decision']['producer_identity'] == f.captured['key']['implementation_identity']
    assert result['decision']['original_completion_sha256'] == candidate.digest(raw)
    assert vars(f) == before
    assert (f.directory / 'completed.json').read_bytes() == raw
    assert 'authority_inputs' not in f.captured['source']


@pytest.mark.parametrize('change', [
    'unknown_producer', 'wrong_request_producer', 'wrong_current_consumer',
    'task', 'candidate', 'turn', 'binding', 'policy', 'failed_check', 'invalid',
    'missing_inventory', 'authority_mismatch', 'carrier_changed', 'carrier_added', 'carrier_deleted',
])
def test_compatibility_does_not_relax_other_bindings(adoption, change):
    f = adoption
    if change == 'unknown_producer':
        f.captured['key']['implementation_identity'] = 'unreviewed'
    elif change == 'wrong_request_producer':
        f.original['in_flight_request']['acceptance_logic_identity'] = 'other'
    elif change == 'wrong_current_consumer':
        f.current_key['implementation_identity'] = 'other'
    elif change in {'task', 'candidate', 'turn', 'binding'}:
        field = {'task': 'task_id', 'candidate': 'candidate_sha256', 'turn': 'submission_turn',
                 'binding': 'task_binding_sha256'}[change]
        f.current_key[field] = 'changed'
    elif change == 'policy':
        f.current_key['future_policy_binding'] = 'changed'
    elif change == 'failed_check':
        f.checked['response']['acceptance_check_passed'] = False
    elif change == 'invalid':
        f.checked['response']['outcome'] = 'Invalid'
    elif change == 'missing_inventory':
        del f.checked['checked_sources']['inputs']
    elif change == 'authority_mismatch':
        f.checked['checked_sources']['authority_inputs']['.trellis/scripts/check.py'] = 'changed'
    elif change == 'carrier_changed':
        f.source['authority_inputs']['.trellis/artifacts/policy.json'] = 'changed'
    elif change == 'carrier_added':
        f.source['authority_inputs']['.trellis/artifacts/new.json'] = 'changed'
    elif change == 'carrier_deleted':
        del f.source['authority_inputs']['.trellis/artifacts/policy.json']
    assert prepare(f) is None


def test_compatibility_requires_authenticated_completion(adoption, monkeypatch):
    monkeypatch.setattr(receipts, 'reusable', lambda *_: None)
    with pytest.raises(candidate.CandidateError, match='original completion changed'):
        prepare(adoption)


def test_compatibility_allowlist_is_the_reviewed_complete_identities():
    assert compatibility.reviewed_producers() == {
        'trellis-worker-acceptance-v2:sha256:604f9c1ade349a11d5cf9b37d98192d5cf50dd0b00ebe1579808fbf5ee5af121',
        'trellis-worker-acceptance-v2:sha256:018ff429de9b6e8a2ef4f73a855dbe175bacc2e80335b0dae4ca43f252058611',
        'trellis-worker-acceptance-v2:sha256:ef982f2a68367da2f20d55b779f1184b0c7b7de2031870d0685cc3ec19e71f9c',
    }


@pytest.mark.parametrize('producer', sorted(compatibility.reviewed_producers()))
def test_driver_authenticates_original_completion_and_audits_native_decision(completed, monkeypatch, producer):
    from trellis.dispatch.driver import ProductionDriver, acceptance_identity
    f = completed
    consumer = 'trellis-worker-acceptance-v2:sha256:' + 'f' * 64
    carrier = '.trellis/scripts/check.py'
    authority = f.repo / '.trellis/supervisor/repo'
    authority.mkdir(parents=True)
    for repo in (f.repo, authority):
        (repo / carrier).parent.mkdir(parents=True, exist_ok=True)
        (repo / carrier).write_text('original checker launcher')
        (repo / 'lakefile.toml').write_text('fixed build')
    monkeypatch.setattr(receipts, 'input_hashes', lambda repo: {
        path: candidate.digest((repo / path).read_bytes()) for path in (carrier, 'lakefile.toml')})
    f.private['in_flight_request']['acceptance_logic_identity'] = producer
    f.key.update(implementation_identity=producer, acceptance_sha256=acceptance_identity(f.private))
    captured = json.loads((f.directory / 'canonical.json').read_text())
    captured.update(key=f.key, acceptance_sha256=f.key['acceptance_sha256'])
    put(f.directory / 'canonical.json', captured)
    put(f.directory / 'runtime/protocol_state.json', f.private)
    put(f.directory / 'prepared.json', dict(identity=candidate.digest(candidate.canonical_json(
        dict(state=f.private, sources=f.source['manifest'], config_sha256=captured['config_sha256'])))))
    f.checked['response']['acceptance_check_passed'] = True
    f.checked['checked_sources'] = receipts.checked_sources(f.repo)
    put(f.directory / 'execution/response.json', f.checked)
    put(f.directory / 'execution/result.json', dict(returncode=0,
        response_sha256=candidate.digest((f.directory / 'execution/response.json').read_bytes())))
    (f.directory / 'completed.json').unlink()
    receipts.complete(f.directory, f.checked)
    originals = {path: path.read_bytes() for path in (f.directory / 'completed.json',
        f.directory / 'canonical.json', f.directory / 'runtime/protocol_state.json',
        f.directory / 'execution/response.json')}
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: consumer)
    private = copy.deepcopy(f.private)
    private['in_flight_request']['acceptance_logic_identity'] = consumer
    private['worker_execution'] = {'task_id': 1}
    carriers = {carrier: candidate.digest((f.repo / carrier).read_bytes())}
    source = dict(f.source, authority_inputs=carriers)
    key = dict(f.key, implementation_identity=consumer,
        acceptance_sha256=acceptance_identity(private), authority_input_hashes=carriers)
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.config_path = f.directory / 'canonical-config.json'
    driver.config_path.write_bytes(b'original config')
    calls = []
    def native(payload):
        calls.append(payload)
        assert payload['original_private']['in_flight_request']['acceptance_logic_identity'] == producer
        assert payload['original_source']['authority_inputs'] == carriers
        return dict(status='rebased', response=f.checked['response'])
    driver.native = SimpleNamespace(_call=native)
    assert driver._rebase_acceptance(f.directory, captured, key, private, source, f.checked, f.task) == f.checked
    assert len(calls) == 1
    assert all(path.read_bytes() == content for path, content in originals.items())
    diagnostic = json.loads(next((f.directory / 'semantic-rebase').glob('*.json')).read_text())
    assert diagnostic['status'] == 'rebased'
    assert diagnostic['producer_compatibility']['producer_identity'] == producer
    assert diagnostic['producer_compatibility']['original_completion_sha256'] == candidate.digest(
        originals[f.directory / 'completed.json'])
    (f.directory / 'execution/response.json').write_text('{}')
    with pytest.raises(candidate.CandidateError, match='original completion changed'):
        driver._rebase_acceptance(f.directory, captured, key, private, source, f.checked, f.task)
    assert len(calls) == 1


def test_modeling_reviewed_producer_cannot_enter_general_proof_lane(adoption):
    f = adoption
    producer = next(iter(compatibility.reviewed_producers(modeling=True)))
    assert producer not in compatibility.reviewed_producers()
    f.captured['key']['implementation_identity'] = producer
    f.original['in_flight_request']['acceptance_logic_identity'] = producer
    assert prepare(f) is None
    f.original['phase'] = f.current['phase'] = 'Modeling'
    assert prepare(f)['decision']['producer_identity'] == producer
    f.current['phase'] = 'ProofFormalization'
    assert prepare(f) is None


def test_theorem_stating_producer_has_separate_positive_lane(adoption):
    f = adoption
    producer, = compatibility.reviewed_producers(theorem_stating=True)
    assert producer not in compatibility.reviewed_producers()
    assert producer not in compatibility.reviewed_producers(modeling=True)
    f.captured['key']['implementation_identity'] = producer
    f.original['in_flight_request']['acceptance_logic_identity'] = producer
    f.original['phase'] = f.current['phase'] = 'TheoremStating'
    for state in (f.original, f.current):
        state['worker_execution'] = dict(task_kind='ordinary', compiler='managed')
        state['in_flight_request']['worker_acceptance'] = dict(validation_kind='TheoremTargeted')
    originals = copy.deepcopy(vars(f))
    assert prepare(f)['decision']['producer_identity'] == producer
    assert vars(f) == originals
    for phase in ('ProofFormalization', 'Modeling', 'RevisionStating'):
        f.original['phase'] = f.current['phase'] = phase
        assert prepare(f) is None
    f.original['phase'] = 'TheoremStating'
    assert prepare(f) is None
    f.current['phase'] = 'TheoremStating'
    f.checked['response']['acceptance_check_passed'] = False
    assert prepare(f) is None


@pytest.mark.parametrize('task_kind,compiler,validation', [
    ('lean_closure', 'managed', 'ProofLocal'),
    ('ordinary', 'none', 'TheoremTargeted'),
    ('ordinary', 'managed', 'TheoremGlobal'),
])
def test_stating_category_does_not_replace_existing_producer_lanes(adoption, task_kind, compiler, validation):
    f = adoption
    for state in (f.original, f.current):
        state['phase'] = 'TheoremStating'
        state['worker_execution'] = dict(task_kind=task_kind, compiler=compiler)
        state['in_flight_request']['worker_acceptance'] = dict(validation_kind=validation)
    assert prepare(f) is not None
    producer, = compatibility.reviewed_producers(theorem_stating=True)
    f.captured['key']['implementation_identity'] = producer
    f.original['in_flight_request']['acceptance_logic_identity'] = producer
    assert prepare(f) is None

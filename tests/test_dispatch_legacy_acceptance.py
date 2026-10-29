import copy
import json
from types import SimpleNamespace

import pytest

from trellis.dispatch import acceptance_receipts as receipts, candidate, legacy_acceptance as legacy
from trellis.dispatch.driver import acceptance_identity
from test_dispatch_acceptance_receipts import completed, put


@pytest.fixture
def donor(completed, monkeypatch):
    f = completed
    producer = sorted(legacy.reviewed_producers())[0]
    consumer = 'current-test-implementation'
    carrier = '.trellis/scripts/check.py'
    authority = f.repo / '.trellis/supervisor/repo'
    authority.mkdir(parents=True)
    for repo in (f.repo, authority):
        (repo / carrier).parent.mkdir(parents=True, exist_ok=True)
        (repo / carrier).write_text('original checker')
        (repo / 'lakefile.toml').write_text('fixed build')
    monkeypatch.setattr(receipts, 'input_hashes', lambda repo: {
        path: candidate.digest((repo / path).read_bytes()) for path in (carrier, 'lakefile.toml')})
    f.private['in_flight_request'].update(acceptance_logic_identity=producer, active_node='A')
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
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: consumer)
    selected = legacy.select(f.directory.parent.parent, f.identity)
    current = f.directory.parent / '2'
    carriers = {carrier: candidate.digest((f.repo / carrier).read_bytes())}
    key = dict(f.key, implementation_identity=consumer, acceptance_sha256='current-private',
        authority_input_hashes=carriers)
    source = dict(f.source, authority_inputs=carriers)
    put(current / 'canonical.json', dict(key=key, source=source,
        staged_plan={'supported': True}, config_sha256=captured['config_sha256']))
    private = copy.deepcopy(f.private)
    private['in_flight_request']['acceptance_logic_identity'] = consumer
    put(current / 'runtime/protocol_state.json', private)
    return SimpleNamespace(original=f, selected=selected, current=current)


@pytest.mark.parametrize("compact_old,compact_new", [(False, False), (False, True), (True, False), (True, True)])
def test_completed_old_receipt_becomes_artifact_donor_without_changing_evidence(donor, compact_old, compact_new):
    from trellis.history_artifacts import encode_shared_state
    f = donor
    for directory, compact in [(f.original.directory, compact_old), (f.current, compact_new)]:
        if compact:
            path = directory / "runtime/protocol_state.json"
            put(path, encode_shared_state({"state": json.loads(path.read_text())}))
    if compact_old:
        (f.original.directory / "completed.json").unlink()
        receipts.complete(f.original.directory, f.original.checked)
        f.selected = legacy.select(f.original.directory.parent.parent, f.original.identity)
    originals = {p: p.read_bytes() for p in f.original.directory.rglob('*') if p.is_file()}
    marker = legacy.prepare(f.current, f.selected)
    assert marker['owner_nodes'] == ['A']
    assert 'response' not in marker and 'outcome' not in marker
    previous, actual, completed = legacy.resolve(f.current / 'runtime')
    assert previous == f.original.directory and actual == marker
    assert completed['key'] == f.original.key
    assert all(p.read_bytes() == raw for p, raw in originals.items())
    assert legacy.select(f.original.directory.parent.parent, f.original.identity) == f.selected


def test_surviving_original_round_cannot_be_bypassed(donor):
    f = donor
    latest = f.original.directory.parent / '3'
    put(latest / 'canonical.json', json.loads((f.original.directory / 'canonical.json').read_text()))
    assert legacy.select(f.original.directory.parent.parent, f.original.identity) is None


def test_historical_b7_marker_authentication_does_not_allow_live_install(donor, monkeypatch):
    from trellis.dispatch.staged_acceptance import reviewed_owned_producers
    producer = next(iter(reviewed_owned_producers()))
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: producer)
    captured = json.loads((donor.current / 'canonical.json').read_text())
    captured['key']['implementation_identity'] = producer
    put(donor.current / 'canonical.json', captured)
    private = json.loads((donor.current / 'runtime/protocol_state.json').read_text())
    private['in_flight_request']['acceptance_logic_identity'] = producer
    put(donor.current / 'runtime/protocol_state.json', private)
    marker = legacy.prepare(donor.current, donor.selected)
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'updated-consumer')
    with pytest.raises(candidate.CandidateError, match='binding changed'):
        legacy.resolve(donor.current / 'runtime')
    assert legacy.resolve(donor.current / 'runtime', historical_consumer=producer)[1] == marker
    with pytest.raises(candidate.CandidateError, match='binding changed'):
        legacy.resolve(donor.current / 'runtime', historical_consumer='unknown')
    put(donor.original.directory / 'execution/response.json', {'forged': True})
    with pytest.raises(candidate.CandidateError, match='response changed'):
        legacy.resolve(donor.current / 'runtime', historical_consumer=producer)


@pytest.mark.parametrize('field', ['task_id', 'candidate_sha256', 'task_binding_sha256',
                                   'future_policy', 'authority_input_hashes'])
def test_current_binding_changes_rejected(donor, field):
    current = json.loads((donor.current / 'canonical.json').read_text())
    current['key'][field] = 'changed'
    put(donor.current / 'canonical.json', current)
    with pytest.raises(candidate.CandidateError, match='binding changed'):
        legacy.prepare(donor.current, donor.selected)


@pytest.mark.parametrize('change', ['canonical', 'response', 'descriptor_source', 'descriptor_carriers'])
def test_changed_original_or_descriptor_cannot_create_marker(donor, change):
    if change in {'canonical', 'response'}:
        name = {'canonical': 'canonical.json', 'response': 'execution/response.json'}[change]
        put(donor.original.directory / name, {})
    elif change == 'descriptor_source':
        donor.selected['source']['input_hashes']['lakefile.toml'] = 'changed'
    else:
        donor.selected['authority_inputs']['.trellis/scripts/check.py'] = 'changed'
    with pytest.raises(candidate.CandidateError, match='evidence|descriptor'):
        legacy.prepare(donor.current, donor.selected)
    assert not (donor.current / legacy.MARKER).exists()


@pytest.mark.parametrize('path', ['Tablet/Preamble.lean', 'Tablet/header.tex', 'lakefile.toml',
                                  'reference/paper.tex'])
def test_global_semantic_input_changes_are_not_admin_drift(donor, path):
    current = json.loads((donor.current / 'canonical.json').read_text())
    current['source']['input_hashes'][path] = 'changed'
    put(donor.current / 'canonical.json', current)
    with pytest.raises(candidate.CandidateError, match='global semantic inputs changed'):
        legacy.prepare(donor.current, donor.selected)


def test_unrelated_node_drift_and_derived_indexes_defer_to_exact_portable_guards(donor):
    current = json.loads((donor.current / 'canonical.json').read_text())
    current['source']['graph']['edges']['B'] = []
    current['source']['input_hashes'].update({'Tablet/B.lean': 'new', 'Tablet/INDEX.md': 'generated'})
    put(donor.current / 'canonical.json', current)
    assert legacy.prepare(donor.current, donor.selected)['owner_nodes'] == ['A']


@pytest.mark.parametrize('change', ['response', 'key', 'consumer', 'ordinal'])
def test_recovery_rechecks_immutable_donor_and_current_binding(donor, monkeypatch, change):
    legacy.prepare(donor.current, donor.selected)
    if change == 'response':
        put(donor.original.directory / 'execution/response.json', {})
    elif change == 'key':
        captured = json.loads((donor.current / 'canonical.json').read_text())
        captured['key']['acceptance_sha256'] = 'other'
        put(donor.current / 'canonical.json', captured)
    elif change == 'consumer':
        monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'next-version')
    else:
        marker = json.loads((donor.current / legacy.MARKER).read_text())
        marker['donor_round'] = '../1'
        put(donor.current / legacy.MARKER, marker)
    with pytest.raises(candidate.CandidateError):
        legacy.resolve(donor.current / 'runtime')

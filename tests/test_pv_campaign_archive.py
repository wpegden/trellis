"""Whole-campaign exports preserve versioned evidence without new verdicts."""
import hashlib
import json
from pathlib import Path
import shutil
import zipfile

import pytest

from trellis.json_io import save_json
from trellis.modeling.driver import source_fingerprint
from trellis.modeling.history import record_model_baseline, record_observation
from trellis.pv_toolchain import setup_basis
from trellis.pv_revision.archive import CampaignArchiveError, campaign_status, export_campaign
from trellis.pv_revision.driver import Driver, initialize
from trellis.pv_revision.store import source_manifest


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def generation(tmp_path, number=0, *, complete=True, goal='G1', source=b'pub fn value() -> u8 { 0 }\n'):
    folder = tmp_path / f'g{number}'
    repo, prep, runtime = folder / 'repo', folder / 'preparation', folder / 'runtime'
    for path in (repo / 'upstream', repo / 'crate', repo / 'Tablet', prep / 'upstream', prep / 'work/crate', runtime):
        path.mkdir(parents=True)
    for root in (repo / 'upstream', repo / 'crate', prep / 'upstream', prep / 'work/crate'):
        (root / 'lib.rs').write_bytes(source)
        (root / 'binary.dat').write_bytes(b'\x00\xffexact source')
    goals = {'context': 'All inputs satisfy the stated domain.', 'goals': [{'id': goal, 'text': 'The value satisfies the required bound.'}]}
    save_json(prep / 'goals.json', goals)
    save_json(repo / 'modeling-input/goals.json', goals)
    for root in (repo, prep):
        (root / 'GOAL.md').write_text('Shared context\n\n' + goal + ': required bound\n')
    save_json(prep / 'config.json', {'tools': {}})
    save_json(prep / 'work/extraction.json', {'crate': 'crate'})
    save_json(repo / 'modeling-input/extraction.json', {'crate': 'crate'})
    (repo / 'generated.lean').write_text('def mechanicalValue : Nat := 0\n')
    (repo / 'Tablet/Model.lean').write_text('def processValue : Nat := 0\n')
    (repo / 'Tablet/Proof.lean').write_text('theorem proof : True := True.intro\n')
    models = [{'node': 'Model', 'provenance': {'source_sha256': source_manifest(repo / 'crate')['source_tree_sha256']}}]
    save_json(repo / 'trellis.config.json', {'pv_tablet': {'extraction_models': models}, 'provider_secret': 'NEVER_EXPORT'})
    save_json(repo / 'pv_tablet.json', {'extraction_models': models})
    (repo / 'lean-toolchain').write_text('leanprover/lean4:v-test\n')
    (repo / 'lakefile.lean').write_text('require Local from "/host/specific/library"\n')
    save_json(repo / 'modeling-input/tool-identities.json', {'lean': 'pinned-library-and-tool-identity'})
    resource = 'frozen-resource-identity'
    manifest = {'provenance_schema': 'trellis-native-preparation/v1', 'preparation_root': str(prep),
                'source_basis': source_fingerprint(prep, setup=setup_basis(prep, resource_identity=resource)),
                'resource_identity': resource, 'native_model': 'generated.lean',
                'bootstrap_reproduction': {'native_sha256': sha(repo / 'generated.lean')}}
    save_json(repo / 'modeling-input/manifest.json', manifest)
    record_model_baseline(prep, repo)
    (repo / 'Tablet/Model.lean').write_text('def processValue : Nat := 0\ndef addedModelSupport : Nat := 1\n')
    observation = prep / 'observations/0001'
    (observation / 'Generated').mkdir(parents=True)
    (observation / 'Generated/Raw.lean').write_text('def failedExtractionPartial : Nat := 3\n')
    save_json(observation / 'observation.json', {'aeneas': {'passed': False}})
    record_observation(prep, observation, {'aeneas': {'passed': False}}, inputs={'source/lib.rs': source})
    protocol = {'phase': 'Complete' if complete else 'ProofFormalization', 'cycle': number + 1,
                'pv_tablet_configured': True, 'pv_goal_prose_sha256': sha(repo / 'GOAL.md'),
                'authored_decide_goals': {'primary': goal}, 'live': {'present_nodes': ['Proof']},
                'local_closure_records': {'Proof': {'active_decl_hash': sha(repo / 'Tablet/Proof.lean')}}}
    if complete:
        (runtime / 'trust_package.archive').write_bytes(b'existing kernel archive bytes\x00\xff')
        protocol['trust_base'] = {'mode': 'required_v1', 'package_finalization': {
            'archive_sha256': sha(runtime / 'trust_package.archive')}}
    else:
        protocol.update(pv_live_polarity={'primary': 'Disprove'},
                        pv_refutation_findings={'primary': {'refutation_node': 'Proof'}},
                        pv_revision_handoff={'refuted_goals': [{'goal_id': 'primary', 'refutation_nodes': ['Proof']}]})
    save_json(runtime / 'protocol_state.json', protocol)
    save_json(runtime / 'runtime_metadata.json', {'repo_path': str(repo), 'secret_provider_option': 'NEVER_EXPORT'})
    (runtime / 'full-chat.log').write_text('NEVER_EXPORT full chat')
    return {'repo': repo, 'prep': prep, 'runtime': runtime, 'goals': goals}


def revision(parent, *, retire=False):
    root = parent['runtime'] / 'pv-revision'
    handoff = {'runtime': str(parent['runtime']), 'repo': str(parent['repo']), 'preparation': str(parent['prep']),
               'protocol_state_sha256': sha(parent['runtime'] / 'protocol_state.json'),
               'tablet_manifest': source_manifest(parent['repo'] / 'Tablet')}
    initialize(root, parent['repo'] / 'upstream', parent['goals'], ['G1'], handoff=handoff)
    audit = {'summary': 'Replace or retire the original refuted requirement', 'alternatives': [], 'tasks': [],
             'dispositions': [{'goal_id': 'G1', 'remedy': 'retire' if retire else 'replace',
                               'reason': 'Original behavior does not satisfy the requirement',
                               'replacement_goals': [] if retire else [{'id': 'G2', 'text': 'The value satisfies the required bound.'}],
                               'surviving_obligations': [], 'task_ids': []}]}
    responses = [audit, {'action': 'complete', 'reason': 'No code tasks are needed'}]
    def agents(_root, state, task):
        return responses.pop(0) if responses else {'decision': 'pass', 'binding': state['binding'],
                   'reason': 'Audited requirement change', 'alternatives_assessment': 'Alternatives reviewed'}
    driver = Driver(root, agents=agents)
    for _ in range(3):
        driver.step()
    assert driver.state['status'] == 'awaiting_human'
    driver.decide('accept', digest=driver.state['human_package']['digest'], reason='Human accepted requirements')
    driver.state['status'] = 'all_requirements_retired' if retire else 'successor_started'
    driver.save()
    return {'schema': 'trellis-pv-revision-successor/v1', 'revision_root': str(root),
            'package_digest': driver.state['human_package']['digest'], 'predecessor': handoff}


def contents(path):
    with zipfile.ZipFile(path) as archive:
        return json.loads(archive.read('campaign.json')), {name: archive.read(name) for name in archive.namelist()}


def test_final_campaign_preserves_required_archive_and_mechanical_process_history(tmp_path):
    item = generation(tmp_path)
    path = export_campaign(item['runtime'])
    manifest, files = contents(path)
    assert manifest['final'] and manifest['terminal_outcome'] == 'complete'
    assert 'original_goals_all_proved' not in manifest
    assert manifest['history_completeness'] == 'partial'
    assert files['generations/0000/trust_package.archive'] == (item['runtime'] / 'trust_package.archive').read_bytes()
    assert files['generations/0000/source/original/binary.dat'] == b'\x00\xffexact source'
    assert b'addedModelSupport' in files['generations/0000/model-lineage/process-model.diff']
    assert any(b'failedExtractionPartial' in raw for name, raw in files.items() if '/extraction-history/blobs/' in name)
    assert b'host-specific absolute library paths' in files['README.md']
    assert not any('trellis.config' in name or 'full-chat' in name or 'runtime_metadata' in name for name in files)
    assert not any(b'NEVER_EXPORT' in raw for raw in files.values())
    for row in manifest['files']:
        assert hashlib.sha256(files[row['path']]).hexdigest() == row['sha256']
    assert path == export_campaign(item['runtime'])
    status = campaign_status(item['runtime'])
    assert status['can_export_final'] and status['final_archive']['path'] == str(path)
    assert status['history_completeness'] == 'partial'


def test_revision_chain_preserves_original_disproof_replacement_and_new_proof(tmp_path):
    first = generation(tmp_path, complete=False)
    receipt = revision(first)
    last = generation(tmp_path, 1, goal='G2')
    save_json(last['repo'] / 'modeling-input/revision-from.json', receipt)
    path = export_campaign(last['runtime'])
    manifest, files = contents(path)
    assert manifest['generation_count'] == 2
    assert manifest['generations'][0]['recorded_refutations']['primary']['refutation_node'] == 'Proof'
    assert manifest['generations'][1]['recorded_closures']['Proof']
    assert [(row['generation'], row['goal_id']) for row in manifest['goal_lineage']] == [(0, 'G1'), (1, 'G2')]
    assert manifest['goal_lineage'][0]['recorded_dispositions'][0]['remedy'] == 'replace'
    assert 'generations/0000/revision/review.zip' in files
    decisions = json.loads(files['generations/0000/revision/decisions.json'])
    assert decisions['human_decisions'][-1]['action'] == 'accept'
    assert files['generations/0000/Tablet/Proof.lean'] == (first['repo'] / 'Tablet/Proof.lean').read_bytes()


def test_native_reopening_is_a_separate_generation_with_same_goal_identity(tmp_path):
    first = generation(tmp_path, complete=False)
    last = generation(tmp_path, 1)
    save_json(last['repo'] / 'modeling-input/reopened-from.json', {
        'schema': 'trellis-native-reopening/v1', 'repo': str(first['repo']), 'runtime': str(first['runtime']),
        'preparation': str(first['prep']), 'protocol_state_sha256': sha(first['runtime'] / 'protocol_state.json')})
    manifest, _ = contents(export_campaign(last['runtime']))
    assert manifest['generation_count'] == 2
    assert [row['goal_id'] for row in manifest['goal_lineage']] == ['G1', 'G1']
    assert manifest['generations'][1]['predecessor_kind'] == 'reopening'


def test_retired_terminal_never_claims_original_requirements_proved(tmp_path):
    item = generation(tmp_path, complete=False)
    revision(item, retire=True)
    manifest, files = contents(export_campaign(item['runtime']))
    assert manifest['final'] and manifest['terminal_outcome'] == 'all_requirements_retired'
    assert b'No final successful verification' in files['README.md']
    assert manifest['goal_lineage'][0]['recorded_dispositions'][0]['remedy'] == 'retire'
    assert 'generations/0000/trust_package.archive' not in files


def test_nonterminal_requires_explicit_incomplete_preview(tmp_path):
    item = generation(tmp_path, complete=False)
    with pytest.raises(CampaignArchiveError, match='terminal outcome'):
        export_campaign(item['runtime'])
    manifest, _ = contents(export_campaign(item['runtime'], allow_incomplete=True))
    assert not manifest['final'] and manifest['terminal_outcome'] == 'incomplete'


@pytest.mark.parametrize('what', ['protocol', 'proof', 'trust', 'source', 'goals', 'model'])
def test_final_archive_rejects_stale_bound_evidence(tmp_path, what):
    first = generation(tmp_path, complete=False)
    last = generation(tmp_path, 1)
    save_json(last['repo'] / 'modeling-input/reopened-from.json', {
        'repo': str(first['repo']), 'runtime': str(first['runtime']), 'preparation': str(first['prep']),
        'protocol_state_sha256': sha(first['runtime'] / 'protocol_state.json')})
    paths = {'protocol': first['runtime'] / 'protocol_state.json', 'proof': first['repo'] / 'Tablet/Proof.lean',
             'trust': last['runtime'] / 'trust_package.archive', 'source': last['repo'] / 'crate/lib.rs',
             'goals': last['repo'] / 'modeling-input/goals.json', 'model': last['repo'] / 'generated.lean'}
    path = paths[what]
    path.write_bytes(path.read_bytes() + b'\n')
    with pytest.raises(CampaignArchiveError):
        export_campaign(last['runtime'])
    assert not list((last['runtime'] / 'campaign-archives').glob('*.zip'))


def test_missing_predecessor_fails_final_and_is_explicit_preview_gap(tmp_path):
    last = generation(tmp_path)
    save_json(last['repo'] / 'modeling-input/reopened-from.json', {
        'repo': str(tmp_path / 'lost/repo'), 'runtime': str(tmp_path / 'lost/runtime'), 'preparation': str(tmp_path / 'lost/prep')})
    assert not campaign_status(last['runtime'])['can_export_final']
    assert campaign_status(last['runtime'])['terminal_outcome'] == 'complete'
    with pytest.raises(CampaignArchiveError, match='predecessor'):
        export_campaign(last['runtime'])
    manifest, _ = contents(export_campaign(last['runtime'], allow_incomplete=True))
    assert not manifest['final']
    assert any(gap['kind'] == 'missing_or_invalid_generation' for gap in manifest['gaps'])


def test_status_is_lightweight_and_reports_terminal_export_error(tmp_path, monkeypatch):
    import trellis.pv_revision.archive as archive
    item = generation(tmp_path)
    monkeypatch.setattr(archive, 'source_manifest', lambda *_: pytest.fail('status must not hash source trees'))
    save_json(item['runtime'] / 'campaign-archives/last-error.json', {'error': 'missing retained proof evidence'})
    status = campaign_status(item['runtime'])
    assert status['terminal_outcome'] == 'complete'
    assert status['error'] == 'missing retained proof evidence'
    other = tmp_path / 'non-native'
    save_json(other / 'runtime_metadata.json', {'repo_path': str(tmp_path / 'math')})
    save_json(other / 'protocol_state.json', {'phase': 'Complete'})
    assert campaign_status(other) is None


def test_matched_drift_of_native_and_preparation_source_still_fails_frozen_basis(tmp_path):
    item = generation(tmp_path)
    for path in (item['repo'] / 'crate/lib.rs', item['prep'] / 'work/crate/lib.rs'):
        path.write_text('both source trees drifted together')
    with pytest.raises(CampaignArchiveError, match='frozen basis'):
        export_campaign(item['runtime'])


def test_runtime_nested_under_native_repo_can_export_history(tmp_path):
    item = generation(tmp_path)
    nested = item['repo'] / 'runtime'
    shutil.move(item['runtime'], nested)
    manifest, files = contents(export_campaign(nested))
    assert manifest['final']
    assert 'generations/0000/model-lineage/manifest.json' in files


def test_unfinalized_trust_archive_is_labeled_as_provisional(tmp_path):
    item = generation(tmp_path, complete=False)
    (item['runtime'] / 'trust_package.archive').write_bytes(b'interrupted package construction')
    manifest, files = contents(export_campaign(item['runtime'], allow_incomplete=True))
    assert manifest['generations'][0]['trust_archive_status'] == 'unfinalized'
    assert files['generations/0000/trust_package.archive'] == b'interrupted package construction'


def test_later_export_failure_suppresses_stale_success_link(tmp_path):
    item = generation(tmp_path)
    export_campaign(item['runtime'])
    assert campaign_status(item['runtime'])['final_archive']
    save_json(item['runtime'] / 'campaign-archives/last-error.json', {'error': 'proof source changed after export'})
    status = campaign_status(item['runtime'])
    assert status['terminal_outcome'] == 'complete' and not status['can_export_final']
    assert 'final_archive' not in status
    assert status['error'] == 'proof source changed after export'


def test_campaign_shares_exact_contract_across_distinct_zip_encodings(tmp_path):
    from trellis.dispatch.history import DIRECTORY, FORMAT, resolve
    first, last = generation(tmp_path, complete=False), generation(tmp_path, 1)
    binding = dict(provider='fixture', model='fixture', reasoning_effort='high',
                   timeout_seconds=10, max_timeout_seconds=10, allowed_tasks=['ordinary'])
    context = dict(task_id=1, profile='fixture', binding=binding, compiler='managed', task_kind='ordinary')
    raw = json.dumps(dict(id=1, task_execution=context, reviewer_comments='exact frozen context λ')).encode()
    reference = {'$dispatch_request': FORMAT, 'sha256': hashlib.sha256(raw).hexdigest(),
                 'bytes': len(raw), 'task_execution': context}
    archives = []
    for item, method in [(first, zipfile.ZIP_STORED), (last, zipfile.ZIP_DEFLATED)]:
        folder = item['repo'] / DIRECTORY
        folder.mkdir(parents=True)
        path = folder / f"{reference['sha256']}.zip"
        with zipfile.ZipFile(path, 'w', compression=method) as archive:
            archive.writestr('request.json', raw)
        archives.append(path.read_bytes())
        state = json.loads((item['runtime'] / 'protocol_state.json').read_text())
        state['dispatch'] = {'tasks': {'1': dict(id=1, binding=binding, status={'status': 'resolved'},
            assignment=dict(profile='fixture', compiler='managed', task_kind='ordinary', request=reference))}}
        save_json(item['runtime'] / 'protocol_state.json', state)
    assert archives[0] != archives[1]
    save_json(last['repo'] / 'modeling-input/reopened-from.json', {
        'schema': 'trellis-native-reopening/v1', 'repo': str(first['repo']), 'runtime': str(first['runtime']),
        'preparation': str(first['prep']), 'protocol_state_sha256': sha(first['runtime'] / 'protocol_state.json')})
    manifest, files = contents(export_campaign(last['runtime']))
    objects = {name: data for name, data in files.items() if name.startswith(DIRECTORY + '/')}
    assert len(objects) == 1
    assert next(iter(objects.values())) == archives[0]
    assert len(manifest['generations']) == 2
    portable = tmp_path / 'portable'
    for name, data in objects.items():
        path = portable / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    assert resolve(portable, reference) == json.loads(raw)

"""A ratified PV revision starts fresh modeling without reviving old approvals."""
import json
import hashlib
from pathlib import Path

import pytest

from trellis.json_io import save_json
from trellis.modeling.observations import read_json
from trellis.pv_revision import lifecycle
from trellis.pv_revision.driver import Driver


@pytest.fixture
def predecessor(tmp_path, monkeypatch):
    from trellis.modeling import native, driver as modeling
    import trellis.checking
    def check_provenance(_repo, **kwargs):
        assert kwargs['bwrap_role'] == 'lake_compiler'
        assert 'pv-revision-preflight' in str(kwargs['output'])
        return {'passed': True}
    monkeypatch.setattr(native, 'check_provenance', check_provenance)
    monkeypatch.setattr(modeling, 'prepare_environment', lambda *_: None)
    monkeypatch.setattr(modeling, 'write_scripts', lambda *_: None)
    monkeypatch.setattr(trellis.checking, 'write_scripts', lambda *_: None)
    job_dir = tmp_path / 'job'
    repo = job_dir / 'native/demo'
    preparation = job_dir / 'native/demo-preparation'
    runtime = job_dir / 'native/demo-runtime'
    for folder in (repo, preparation, runtime):
        folder.mkdir(parents=True)
    (repo / 'Tablet').mkdir()
    (repo / 'Tablet/Negative.lean').write_text('theorem negative_evidence : True := True.intro\n')
    (preparation / 'upstream').mkdir()
    (preparation / 'upstream/lib.rs').write_text('fn original() {}\n')
    config = {'worker': {'provider': 'codex', 'model': 'test', 'effort': 'high'}, 'workflow': {}}
    save_json(job_dir / 'config-template.json', config)
    save_json(preparation / 'trellis.config.json', config)
    collection = {'context': 'Shared original assumption', 'goals': [{'id': 'G1', 'text': 'Original requirement'}]}
    save_json(preparation / 'goals.json', collection)
    save_json(preparation / 'config.json', {'tools': {}})
    save_json(repo / 'modeling-input/manifest.json', {'provenance_schema': 'trellis-native-preparation/v1',
                                                    'preparation_root': str(preparation)})
    handoff = {'schema_version': 1, 'cycle': 7, 'refuted_goals': [{'goal_id': 'primary:G1', 'proof': 'negative-evidence', 'refutation_nodes': ['Negative']}]}
    protocol = {'phase': 'ProofFormalization', 'cycle': 7, 'pv_revision_handoff': handoff,
                'authored_decide_goals': {'primary:G1': 'G1'}, 'live': {'present_nodes': ['P1', 'G1']},
                'deps': {'P1': ['G1']}, 'node_role': {'P1': 'source_preservation'}}
    protocol['local_closure_records'] = {'Negative': {'active_decl_hash': hashlib.sha256((repo / 'Tablet/Negative.lean').read_bytes()).hexdigest()}}
    save_json(runtime / 'protocol_state.json', protocol)
    save_json(runtime / 'pv_revision_ready.json', {'pv_revision_handoff': handoff})
    save_json(runtime / 'runtime_metadata.json', {'repo_path': str(repo)})
    alias = tmp_path / 'demo'
    alias.symlink_to(repo, target_is_directory=True)
    job = {'goal_format': 'native', 'repo': str(alias), 'runtime_root': str(runtime), 'slug': 'demo', 'native_generation': 0}
    save_json(job_dir / 'job.json', job)
    return {'job_dir': job_dir, 'repo': repo, 'preparation': preparation, 'runtime': runtime,
            'job': job, 'alias': alias, 'config': job_dir / 'config-template.json'}


def ratify(predecessor, *, repair=False, retire=False, evidence=False):
    root = lifecycle.initialize_runtime(predecessor['runtime'])
    remedy = 'repair' if repair else 'retire' if retire else 'replace'
    audit = {'summary': 'Compare explicit remedies', 'alternatives': [], 'tasks': [],
             'dispositions': [{'goal_id': 'G1', 'reason': 'Original proved false', 'remedy': remedy,
                               'task_ids': [], 'surviving_obligations': [],
                               'replacement_goals': [] if repair or retire else [{'id': 'G2', 'text': 'Ratified replacement requirement'}]}]}
    if repair:
        audit['tasks'] = [{'id': 'fix', 'title': 'Repair', 'authorization': 'Repair original function',
                           'paths': ['lib.rs'], 'goal_ids': ['G1'],
                           'checks': [{'id': 'regression', 'argv': ['true'], 'expected_exit': 0}]}]
        audit['dispositions'][0]['task_ids'] = ['fix']
        if evidence:
            audit['tasks'][0]['paths'].append('regression.py')
            audit['tasks'][0]['checks'][0]['harness_paths'] = ['regression.py']
    def agent(_root, state, task):
        if task['role'] == 'audit':
            return audit
        if task['role'] == 'reviewer':
            if repair and state['tasks']['fix']['status'] != 'complete':
                return {'action': 'work', 'task_id': 'fix', 'reason': 'Need repair'}
            return {'action': 'complete', 'reason': 'Ready for aggregate review'}
        if task['role'] == 'worker':
            (Path(task['writable_source']) / 'lib.rs').write_text('fn repaired() {}\n')
            if evidence:
                (Path(task['writable_source']) / 'regression.py').write_text('print("check")\n')
            return {'outcome': 'complete', 'summary': 'Repaired behavior', 'alternatives': []}
        return {'decision': 'pass', 'reason': 'Authorized exact candidate', 'binding': state['binding'],
                'alternatives_assessment': 'Considered alternatives'}
    driver = Driver(root, agents=agent, checker=lambda *_: {'ok': True})
    for _ in range(10):
        if driver.state['status'] != 'active':
            break
        driver.step()
    assert driver.state['status'] == 'awaiting_human', driver.state.get('feedback')
    driver.decide('accept', digest=driver.state['human_package']['digest'], reason='Human ratification')
    return Driver(root)


def test_successor_uses_production_only_and_keeps_evidence_in_revision(predecessor, tmp_path):
    driver = ratify(predecessor, repair=True, evidence=True)
    destination = tmp_path / 'successor'
    lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])
    assert (destination / 'upstream/lib.rs').read_text() == 'fn repaired() {}\n'
    assert not (destination / 'upstream/regression.py').exists()
    assert (driver.root / driver.state['source'] / 'regression.py').is_file()
    # Repeated preparation validates the same production projection.
    lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])


def test_capture_uses_authoritative_handoff_and_preserves_full_state(predecessor):
    root = lifecycle.initialize_runtime(predecessor['runtime'])
    record = read_json(root / 'predecessor/record.json')
    assert record['negated_goals'][0]['goal_id'] == 'G1'
    assert record['negated_goals'][0]['challenge_id'] == 'primary:G1'
    assert (root / 'predecessor/protocol_state.json').read_bytes() == (predecessor['runtime'] / 'protocol_state.json').read_bytes()
    assert lifecycle.initialize_runtime(predecessor['runtime']) == root
    protocol = read_json(predecessor['runtime'] / 'protocol_state.json')
    protocol['cycle'] += 1
    save_json(predecessor['runtime'] / 'protocol_state.json', protocol)
    with pytest.raises(ValueError, match='predecessor changed'):
        lifecycle.initialize_runtime(predecessor['runtime'])


@pytest.mark.parametrize('repair', [False, True])
def test_successor_has_ratified_source_requirements_and_fresh_modeling(predecessor, tmp_path, repair):
    driver = ratify(predecessor, repair=repair)
    destination = tmp_path / 'successor'
    receipt = lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])
    assert read_json(destination / 'goals.json') == driver.state['proposed_collection']
    expected = 'fn repaired() {}\n' if repair else 'fn original() {}\n'
    assert (destination / 'upstream/lib.rs').read_text() == expected
    assert (driver.root / 'sources/original/lib.rs').read_text() == 'fn original() {}\n'
    state = read_json(destination / '.trellis/modeling/state.json')
    assert state['stage'] == 'setup'
    assert state['next']['role'] == 'reviewer'
    assert state['audits'] == {} and state['history'] == []
    assert state['status'] == 'active'
    assert read_json(destination / 'config.json')['launch_config']['pv_tablet']['revision']['enabled']
    assert 'pv_tablet' not in read_json(predecessor['config'])
    assert 'ordinary modeling human review' in state['feedback']
    assert receipt == lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])


def test_stale_ratification_cannot_prepare_successor(predecessor, tmp_path):
    driver = ratify(predecessor)
    driver.state['proposed_goals'][0]['text'] = 'Unapproved change'
    driver.state['proposed_collection']['goals'] = driver.state['proposed_goals']
    driver.save()
    with pytest.raises(ValueError, match='changed'):
        lifecycle.prepare_successor(driver.root, tmp_path / 'successor', config_template=predecessor['config'])
    assert not (tmp_path / 'successor').exists()


def test_retirement_retains_original_proof_but_creates_no_empty_successor(predecessor, tmp_path):
    driver = ratify(predecessor, retire=True)
    assert driver.state['human_package']['retirement_is_proof'] is False
    with pytest.raises(ValueError, match='All requirements were retired'):
        lifecycle.prepare_successor(driver.root, tmp_path / 'successor', config_template=predecessor['config'])
    assert read_json(driver.root / 'predecessor/protocol_state.json')['pv_revision_handoff']['refuted_goals'][0]['proof'] == 'negative-evidence'
    assert (driver.root / 'sources/original/lib.rs').exists()


def test_paused_revision_does_not_publish_new_generation(predecessor):
    ratify(predecessor)
    (predecessor['job_dir'] / 'native-controller.pause').touch()
    assert lifecycle.continue_runtime(predecessor['runtime'])
    job = read_json(predecessor['job_dir'] / 'job.json')
    assert job['native_generation'] == 0
    assert predecessor['alias'].resolve() == predecessor['repo']


def test_retired_runtime_duplicate_callback_is_inert(predecessor):
    job = predecessor['job']
    job.update(native_generation=1, runtime_root=str(predecessor['runtime'].with_name('demo-r0001-runtime')))
    save_json(predecessor['job_dir'] / 'job.json', job)
    assert lifecycle.continue_runtime(predecessor['runtime'])
    assert read_json(predecessor['job_dir'] / 'job.json') == job
    assert not lifecycle.revision_root(predecessor['runtime']).exists()


def test_interrupted_successor_launch_resumes_same_generation(predecessor, monkeypatch):
    from scripts import pv_native_create as native
    ratify(predecessor)
    monkeypatch.setattr(native, 'stop_retired_services', lambda *_: None)
    calls = []
    def interrupt(*args):
        calls.append(args)
        raise RuntimeError('Simulated launch interruption')
    monkeypatch.setattr(native, 'launch_preparation', interrupt)
    with pytest.raises(RuntimeError, match='interruption'):
        lifecycle.continue_runtime(predecessor['runtime'])
    pending = read_json(predecessor['job_dir'] / 'job.json')
    assert pending['native_generation'] == 1 and pending['revision']['pending']
    assert read_json(native.active_config_file(predecessor['job_dir'], pending))['pv_tablet']['revision']['enabled']
    preparation, _, _ = native.active_paths(predecessor['job_dir'], pending)
    assert predecessor['alias'].resolve() == preparation
    (predecessor['job_dir'] / 'native-controller.pause').touch()
    monkeypatch.setattr(native, 'launch_preparation', lambda *args: calls.append(args))
    assert lifecycle.continue_runtime(predecessor['runtime'], resume=True)
    resumed = read_json(predecessor['job_dir'] / 'job.json')
    assert resumed['native_generation'] == 1
    assert not resumed['revision']['pending']
    assert not (predecessor['job_dir'] / 'native-controller.pause').exists()
    assert len(calls) == 2
    assert lifecycle.continue_runtime(predecessor['runtime'])
    assert len(calls) == 2


def test_approved_shared_context_is_preserved(predecessor, tmp_path):
    driver = ratify(predecessor)
    # Reopen the explicit human gate for a context edit; a prior acceptance
    # cannot cover it. The new package is separately ratified.
    driver.state['proposed_collection']['context'] = 'Ratified new shared assumption'
    driver.state['status'] = 'awaiting_human'
    driver.state['human_package'] = driver._package(driver.state)
    driver.save()
    driver.decide('accept', digest=driver.state['human_package']['digest'], reason='Approve revised context')
    destination = tmp_path / 'successor'
    lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])
    assert read_json(destination / 'goals.json')['context'] == 'Ratified new shared assumption'
    assert 'Ratified new shared assumption' in (destination / 'GOAL.md').read_text()


def test_interrupted_preparation_has_no_commit_marker_and_retries(predecessor, tmp_path, monkeypatch):
    driver = ratify(predecessor)
    destination = tmp_path / 'successor'
    original_save = lifecycle.save_json
    def interrupted_save(path, value):
        if path == destination / '.trellis/modeling/state.json':
            raise RuntimeError('Interrupted before controller setup')
        original_save(path, value)
    monkeypatch.setattr(lifecycle, 'save_json', interrupted_save)
    with pytest.raises(RuntimeError, match='Interrupted'):
        lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])
    assert not (destination / '.trellis/modeling/revision-from.json').exists()
    monkeypatch.setattr(lifecycle, 'save_json', original_save)
    lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])
    assert read_json(destination / '.trellis/modeling/state.json')['next']['role'] == 'reviewer'
    assert list(tmp_path.glob('successor-interrupted-*'))
    assert read_json(predecessor['job_dir'] / 'job.json')['native_generation'] == 0


def test_capture_refuses_proof_sources_changed_after_closure(predecessor):
    (predecessor['repo'] / 'Tablet/Negative.lean').write_text('theorem negative_evidence : False := by sorry\n')
    with pytest.raises(ValueError, match='differs from its closure'):
        lifecycle.capture(predecessor['runtime'])


def test_preservation_refuses_source_changes_during_capture(predecessor):
    record = lifecycle.capture(predecessor['runtime'])
    root = predecessor['runtime'] / 'capture-race'
    root.mkdir()
    (predecessor['repo'] / 'Tablet/Negative.lean').write_text('changed after capture')
    with pytest.raises(ValueError, match='differ from the captured'):
        lifecycle.preserve_predecessor(root, record)


def test_successor_refuses_changed_upstream_after_publication(predecessor, tmp_path):
    driver = ratify(predecessor)
    destination = tmp_path / 'successor'
    lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])
    (destination / 'upstream/lib.rs').write_text('different program')
    with pytest.raises(ValueError, match='source differs from the ratified'):
        lifecycle.prepare_successor(driver.root, destination, config_template=predecessor['config'])


@pytest.mark.parametrize('change', ['cycle', 'in_flight_request', 'identity'])
def test_capture_rejects_inconsistent_or_unfinished_handoff(predecessor, change):
    path = predecessor['runtime'] / 'protocol_state.json'
    protocol = read_json(path)
    if change == 'cycle':
        protocol['cycle'] += 1
    elif change == 'in_flight_request':
        protocol['in_flight_request'] = {'id': 'unfinished'}
    else:
        protocol['authored_decide_goals']['primary:G1'] = 'nonexistent-goal'
    save_json(path, protocol)
    with pytest.raises(ValueError):
        lifecycle.capture(predecessor['runtime'])
    assert not lifecycle.revision_root(predecessor['runtime']).exists()


def test_revision_and_successor_keep_current_launch_models(predecessor):
    runtime = predecessor['runtime']
    preparation_config = predecessor['preparation'] / 'trellis.config.json'
    preparation_bytes = preparation_config.read_bytes()
    protocol_bytes = (runtime / 'protocol_state.json').read_bytes()
    current = read_json(preparation_config)
    current['worker']['model'] = 'gpt-6.1-sol'
    current['reviewer'] = dict(provider='codex', model='gpt-6.1-sol', effort='xhigh')
    current['parallel_dispatch'] = dict(enabled=True, worker_slots=4, sol_worker_slots=2)
    config_path = predecessor['repo'] / 'trellis.config.json'
    save_json(config_path, current)
    metadata = read_json(runtime / 'runtime_metadata.json')
    metadata['config_path'] = str(config_path)
    save_json(runtime / 'runtime_metadata.json', metadata)
    root = lifecycle.initialize_runtime(runtime)
    revision = read_json(root / 'trellis.config.json')
    assert revision['worker'] == current['worker']
    assert revision['reviewer'] == current['reviewer']
    assert revision['parallel_dispatch'] == current['parallel_dispatch']
    successor = read_json(lifecycle.successor_config(root, predecessor['config']))
    assert successor['worker'] == current['worker']
    assert successor['reviewer'] == current['reviewer']
    assert successor['parallel_dispatch'] == current['parallel_dispatch']
    assert (runtime / 'protocol_state.json').read_bytes() == protocol_bytes
    assert preparation_config.read_bytes() == preparation_bytes
    assert lifecycle.initialize_runtime(runtime) == root

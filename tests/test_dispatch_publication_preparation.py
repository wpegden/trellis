"""Uncommitted packet work releases the lock; durable roll-forward never does."""
import copy
import json
from pathlib import Path
import shutil
import threading
from concurrent.futures import ThreadPoolExecutor
from types import SimpleNamespace

import pytest

from trellis.dispatch import candidate, publication
from trellis.dispatch.driver import ProductionDriver


def sources(tmp_path):
    repo = tmp_path / 'canonical'
    (repo / 'Tablet').mkdir(parents=True)
    for node in ['A', 'Sibling']:
        (repo / f'Tablet/{node}.lean').write_text(node + ' old')
    staged = tmp_path / 'acceptance/repo'
    shutil.copytree(repo, staged)
    (staged / 'Tablet/A.lean').write_text('A checked')
    publisher = publication.Publisher(tmp_path / 'runtime', repo)
    event = dict(task_id=1, candidate_sha256='a'*64, base_state_sha256='old-state',
        decided_at_ms=1, response=dict(status='Ok', outcome='Valid', request_id=1, cycle=1,
            local_closure_revalidation=dict(refreshed=[['A', {'node_certificate': {'artifact_bundle': ['exact']}}]])))
    return publisher, staged, event


def test_stage_does_not_arm_or_write_sources_and_startup_discards_only_scratch(tmp_path):
    publisher, staged, event = sources(tmp_path)
    before = candidate.source_manifest(publisher.repo)
    packet = publisher.stage(1, 'a'*64, staged, expected_manifest=before, kernel_event=event)
    assert packet.directory.exists()
    assert not (publisher.root / 'pending.json').exists()
    assert candidate.source_manifest(publisher.repo) == before
    assert not (packet.directory / 'record.json').exists()
    # Simulate death before activation; no check/canonical data is disposable.
    evidence = publisher.root / ('e'*64)
    evidence.mkdir(); (evidence / 'record.json').write_text('retained journal')
    with publisher.locked():
        assert publisher.recover(lambda *_: pytest.fail('no pending decision')) is None
        publisher.discard_unused_preparations()
    assert not packet.directory.exists()
    assert (evidence / 'record.json').read_text() == 'retained journal'
    assert (staged / 'Tablet/A.lean').read_text() == 'A checked'


@pytest.mark.parametrize('crash', ['after_stage', 'pending', 'source:Tablet/A.lean', 'kernel_committed'])
def test_split_crash_boundaries_recover_exactly_once(tmp_path, crash):
    publisher, staged, event = sources(tmp_path)
    packet = publisher.stage(1, 'a'*64, staged,
        expected_manifest=candidate.source_manifest(publisher.repo), kernel_event=event)
    if crash == 'after_stage':
        with publisher.locked(): publisher.discard_unused_preparations()
        packet = publisher.stage(1, 'a'*64, staged,
            expected_manifest=candidate.source_manifest(publisher.repo), kernel_event=event)
    committed = {}
    calls = []
    def commit(task, identity, payload):
        if task not in committed:
            calls.append(payload)
            committed[task] = dict(candidate_sha256=identity, event_id=7)
        return committed[task]
    def checkpoint(stage):
        if stage == crash: raise RuntimeError('crash')
    with publisher.locked():
        publisher.activate(packet, kernel_event=event)
        if crash not in ('after_stage', 'pending'):
            with pytest.raises(RuntimeError, match='crash'):
                publisher.recover(commit, checkpoint=checkpoint)
    shutil.rmtree(staged)
    with publisher.locked():
        assert publisher.recover(commit)['event_id'] == 7
        assert publisher.recover(commit) is None
        publisher.discard_unused_preparations()
    assert len(calls) == 1
    assert (publisher.repo / 'Tablet/A.lean').read_text() == 'A checked'


@pytest.mark.parametrize('change', ['source', 'new_source', 'config', 'new_config', 'certificate', 'response'])
def test_activation_rejects_changed_material_before_pending_or_source_write(tmp_path, change):
    publisher, staged, event = sources(tmp_path)
    if change == 'config': (publisher.repo / 'trellis.config.json').write_text('old')
    packet = publisher.stage(1, 'a'*64, staged,
        expected_manifest=candidate.source_manifest(publisher.repo), kernel_event=event)
    current_event = copy.deepcopy(event)
    if change == 'source': (publisher.repo / 'Tablet/A.lean').write_text('new current proof')
    elif change == 'new_source': (publisher.repo / 'Tablet/Absent.lean').write_text('new resolver candidate')
    elif change in ('config', 'new_config'): (publisher.repo / 'trellis.config.json').write_text('changed')
    elif change == 'certificate':
        current_event['response']['local_closure_revalidation']['refreshed'][0][1]['node_certificate']['artifact_bundle'] = ['changed']
    else: current_event['response']['outcome'] = 'Invalid'
    before = candidate.source_manifest(publisher.repo)
    with publisher.locked(), pytest.raises(publication.PublicationChanged):
        publisher.activate(packet, kernel_event=current_event)
    assert candidate.source_manifest(publisher.repo) == before
    assert not (publisher.root / 'pending.json').exists()
    publisher.discard(packet)


def driver_fixture(tmp_path, monkeypatch):
    publisher, staged, _ = sources(tmp_path)
    directory = staged.parent
    before = candidate.source_manifest(publisher.repo)
    source = dict(manifest=before, input_hashes=before,
                  graph={'edges': {'A': [], 'Sibling': []}})
    checked = {'response': dict(status='Ok', outcome='Valid', request_id=1, cycle=1)}
    private = dict(phase='ProofFormalization', in_flight_request=dict(id=1, cycle=1))
    (directory / 'runtime').mkdir()
    for name, value in [('canonical.json', dict(source=source, acceptance_sha256='1')),
                        ('base-manifest.json', before), ('runtime/protocol_state.json', private)]:
        (directory / name).write_bytes(candidate.canonical_json(value))
    task = dict(status={'status': 'checking'}, assignment={'scope': {'nodes': ['A']}})
    state = dict(dispatch={'tasks': {'1': task}}, request_id=1)
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.publisher = publisher; driver.repo = publisher.repo
    driver.require_no_halt = lambda: None; driver.now_ms = lambda: 10
    driver.state = lambda: copy.deepcopy(state)
    driver._submission_directory = lambda _: tmp_path / 'submission'
    monkeypatch.setattr(candidate, 'read', lambda _: ({'changes': {'Tablet/A.lean': {}}}, 'a'*64))
    monkeypatch.setattr('trellis.dispatch.driver.acceptance_identity', lambda p: str(p['in_flight_request']['id']))
    # Native semantic rebase itself is covered separately. This fixture checks
    # packet handling across its success/refusal and real canonical lock use.
    driver._rebase_acceptance = lambda *a, **kw: checked
    def observe(_state, repo, **_kwargs):
        manifest = candidate.source_manifest(repo)
        return dict(source, manifest=manifest, input_hashes=manifest)
    prepared, committed = [], []
    def prepare(payload):
        result = dict(payload, base_state_sha256=f"state-{state['request_id']}")
        prepared.append(copy.deepcopy(result))
        return result
    def commit(_task, _identity, event):
        assert event['base_state_sha256'] == f"state-{state['request_id']}"
        committed.append(event)
        return dict(event_id=100)
    driver.native = SimpleNamespace(_require_recovered_publication=lambda: None,
        observe_sources=observe, _call=lambda _: dict(private, in_flight_request=dict(id=state['request_id'], cycle=1)),
        prepare_publication=prepare, commit_publication=commit, publication_receipt=lambda *_: None)
    def render(request):
        assert request['action'] == 'dispatch_render_tablet_support'
        return {'status': 'dispatch_ok', 'output': {'index_md_content': 'index', 'readme_md_content': 'readme'}}
    monkeypatch.setattr('trellis.runtime.kernel_cli.run_kernel_cli', render)
    return driver, directory, checked, state, prepared, committed


def test_slow_staging_allows_control_and_rebinds_admin_event_once(tmp_path, monkeypatch):
    driver, directory, checked, state, prepared, committed = driver_fixture(tmp_path, monkeypatch)
    entered, release = threading.Event(), threading.Event()
    original = driver.publisher.stage
    stages = []
    def slow(*a, **kw):
        packet = original(*a, **kw)
        stages.append(packet)
        entered.set()
        assert release.wait(5), 'canonical control could not acquire the lock'
        return packet
    monkeypatch.setattr(driver.publisher, 'stage', slow)
    with ThreadPoolExecutor(2) as pool:
        future = pool.submit(driver._publish, 1, 'a'*64, directory, checked)
        assert entered.wait(5)
        def control():
            with driver.publisher.locked():
                assert not (driver.publisher.root / 'pending.json').exists()
                assert (driver.repo / 'Tablet/A.lean').read_text() == 'A old'
                state['request_id'] = 2
            release.set()
        pool.submit(control).result(timeout=5)
        assert future.result(timeout=5) is True
    assert len(stages) == 1
    assert len(prepared) == 2 and len(committed) == 1
    assert committed[0]['response']['request_id'] == 2
    assert committed[0]['base_state_sha256'] == 'state-2'
    assert not list(driver.publisher.root.glob('.prepare-*'))


def test_current_source_change_restages_only_packet_and_preserves_sibling(tmp_path, monkeypatch):
    driver, directory, checked, state, prepared, committed = driver_fixture(tmp_path, monkeypatch)
    original = driver.publisher.stage
    stages = []
    def changed(*a, **kw):
        packet = original(*a, **kw)
        stages.append(packet)
        if len(stages) == 1:
            with driver.publisher.locked():
                (driver.repo / 'Tablet/Sibling.lean').write_text('Sibling separately accepted')
                state['request_id'] = 2
        return packet
    monkeypatch.setattr(driver.publisher, 'stage', changed)
    assert driver._publish(1, 'a'*64, directory, checked)
    assert len(stages) == 2 and len(committed) == 1
    assert (driver.repo / 'Tablet/Sibling.lean').read_text() == 'Sibling separately accepted'
    assert (directory / 'repo/Tablet/Sibling.lean').read_text() == 'Sibling old'
    assert (driver.repo / 'Tablet/A.lean').read_text() == 'A checked'
    assert not list(driver.publisher.root.glob('.prepare-*'))


def test_material_drift_is_bounded_and_never_starts_proof_round(tmp_path, monkeypatch):
    driver, directory, checked, state, _, committed = driver_fixture(tmp_path, monkeypatch)
    calls = []
    def changed(*a, **kw):
        calls.append(kw)
        raise publication.PublicationChanged('captured source moved')
    monkeypatch.setattr(driver.publisher, 'stage', changed)
    with pytest.raises(RuntimeError, match='did not stabilize; checked work retained'):
        driver._publish(1, 'a'*64, directory, checked)
    assert len(calls) == 3 and committed == []
    assert (directory / 'canonical.json').exists()
    assert not (driver.publisher.root / 'pending.json').exists()


def test_native_currentness_refusal_after_staging_is_not_a_proof_retry(tmp_path, monkeypatch):
    driver, directory, checked, state, _, committed = driver_fixture(tmp_path, monkeypatch)
    original = driver.publisher.stage
    packets = []
    def changed(*a, **kw):
        packet = original(*a, **kw)
        packets.append(packet)
        state['request_id'] = 2
        return packet
    monkeypatch.setattr(driver.publisher, 'stage', changed)
    driver._rebase_acceptance = lambda *a, **kw: None
    rejected = []
    monkeypatch.setattr('trellis.dispatch.acceptance_receipts.reject', lambda *a, **kw: rejected.append(kw))
    with pytest.raises(RuntimeError, match='inputs changed during packet preparation; checked work retained'):
        driver._publish(1, 'a'*64, directory, checked)
    assert len(packets) == 1 and len(rejected) == 1 and committed == []
    assert (directory / 'canonical.json').exists()
    assert not packets[0].directory.exists()
    assert not (driver.publisher.root / 'pending.json').exists()


def test_stage_error_removes_only_unactivated_scratch(tmp_path, monkeypatch):
    publisher, staged, event = sources(tmp_path)
    before = candidate.source_manifest(publisher.repo)
    original = publication._write
    def interrupted(path, data):
        original(path, data)
        raise RuntimeError('stage interrupted')
    monkeypatch.setattr(publication, '_write', interrupted)
    with pytest.raises(RuntimeError, match='stage interrupted'):
        publisher.stage(1, 'a'*64, staged, expected_manifest=before, kernel_event=event)
    assert candidate.source_manifest(publisher.repo) == before
    assert not list(publisher.root.glob('.prepare-*'))
    assert not (publisher.root / 'pending.json').exists()
    assert (staged / 'Tablet/A.lean').read_text() == 'A checked'


def test_activated_packet_still_authenticates_artifact_bytes_before_any_source_write(tmp_path, monkeypatch):
    from trellis.dispatch import publication_artifacts as artifacts
    publisher, staged, event = sources(tmp_path)
    attempt = publisher.root.parent / '1/acceptance/1'
    attempt.mkdir(parents=True)
    shutil.move(staged, attempt / 'repo')
    staged = attempt / 'repo'
    (attempt / 'canonical.json').write_text('{"key":{}}')
    monkeypatch.setattr('trellis.dispatch.acceptance_receipts.reusable', lambda *a: {'authenticated': True})
    def stage_artifacts(_staged, _canonical, scratch, _response):
        files = dict.fromkeys(artifacts.paths('A'))
        relative = 'lib/lean/Tablet/A.olean'
        files[relative] = candidate.digest(b'exact artifact')
        candidate._write(scratch / 'artifacts' / relative, b'exact artifact')
        return dict(version=1, nodes={'A': dict(files=files)})
    monkeypatch.setattr(artifacts, 'stage', stage_artifacts)
    before = candidate.source_manifest(publisher.repo)
    packet = publisher.stage(1, 'a'*64, staged, expected_manifest=before,
        kernel_event=event, artifact_source=attempt)
    (packet.directory / 'artifacts/lib/lean/Tablet/A.olean').write_bytes(b'changed artifact')
    with publisher.locked():
        publisher.activate(packet, kernel_event=event)
        with pytest.raises(candidate.CandidateError, match='artifact evidence changed'):
            publisher.recover(lambda *_: pytest.fail('must not commit'), publication_receipt=lambda *_: None)
    assert candidate.source_manifest(publisher.repo) == before
    assert (publisher.root / 'pending.json').exists(), 'retain armed failure evidence for diagnosis'


def enable_exact_fast_path(driver, state, tmp_path):
    driver.runtime = driver.publisher.root.parent.parent
    driver.config_path = tmp_path / 'actual-config.json'
    driver.config_path.write_text('{}')
    protocol = driver.runtime / 'protocol_state.json'
    protocol.write_bytes(candidate.canonical_json(state))
    original = driver.native._call
    inventories = []
    def call(payload):
        if payload['action'] == 'dispatch_verifier_inputs':
            # Full filesystem membership, including new global/authority files.
            value = {p.relative_to(driver.repo).as_posix(): candidate.digest(p.read_bytes())
                     for p in driver.repo.rglob('*') if p.is_file()}
            inventories.append(value)
            return value
        return original(payload)
    driver.native._call = call
    return protocol, inventories


def test_exact_unchanged_bytes_need_only_one_native_preparation(tmp_path, monkeypatch):
    driver, directory, checked, state, prepared, committed = driver_fixture(tmp_path, monkeypatch)
    _protocol, inventories = enable_exact_fast_path(driver, state, tmp_path)
    assert driver._publish(1, 'a'*64, directory, checked)
    assert len(prepared) == 1 and len(committed) == 1
    assert len(inventories) == 2 and inventories[0] == inventories[1]
    assert (driver.repo / 'Tablet/A.lean').read_text() == 'A checked'


@pytest.mark.parametrize('mutation', ['state', 'config', 'new_global', 'new_authority'])
def test_fast_path_detects_exact_byte_and_absence_changes(tmp_path, monkeypatch, mutation):
    driver, directory, checked, state, prepared, committed = driver_fixture(tmp_path, monkeypatch)
    protocol, inventories = enable_exact_fast_path(driver, state, tmp_path)
    original = driver.publisher.stage
    stages = []
    def changed(*a, **kw):
        packet = original(*a, **kw)
        stages.append(packet)
        if len(stages) == 1:
            with driver.publisher.locked():
                if mutation in ('state', 'config'):
                    import os
                    path = protocol if mutation == 'state' else driver.config_path
                    stat = path.stat()
                    content = path.read_bytes()
                    # Same size and mtime; a real administrative request change
                    # must force ordinary current reconstruction.
                    if mutation == 'state':
                        state['request_id'] = 2
                    path.write_bytes(candidate.canonical_json(state) if mutation == 'state' else b'[]')
                    assert path.stat().st_size == len(content)
                    os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
                else:
                    path = driver.repo / ('global.txt' if mutation == 'new_global' else '.trellis/scripts/new-helper')
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text('new captured binding')
        return packet
    monkeypatch.setattr(driver.publisher, 'stage', changed)
    assert driver._publish(1, 'a'*64, directory, checked)
    assert len(prepared) == 2 and len(committed) == 1
    assert len(stages) == 1, 'equivalent recomputed packet material needs no artifact recopy'
    assert len(inventories) == (1 if mutation in ('state', 'config') else 2)

import copy
import json
from pathlib import Path

import pytest

from trellis.dispatch import handoffs
from trellis.dispatch.candidate import canonical_json, seal, source_manifest
from trellis.dispatch.workspace import prepare
from trellis.worker_scratch import ensure_worker_scratch_workspace
from test_dispatch_workspace import project


def review_response(root, review_id, reason):
    response = dict(kind='parallel_review', assignments=[], action=None, reason=reason)
    data = canonical_json(response)
    path = root / f'review-{review_id}/execution/response.json'
    path.parent.mkdir(parents=True)
    path.write_bytes(data)
    return dict(review_id=review_id, response_sha256=handoffs.digest(data), decisions=[]), path


def test_recent_reviewer_rationales_are_receipt_bound_and_bounded(tmp_path):
    receipts = [review_response(tmp_path, i, 'Still waiting on ordinary dependency writer π.'
                               if i != 1001 else 'x' * 2500)[0] for i in range(998, 1002)]
    original = copy.deepcopy(receipts)
    # Real native map order crosses a digit boundary; use numeric recency.
    rows = handoffs.reviewer_rationales(tmp_path, sorted(receipts, key=lambda r: str(r['review_id'])))
    assert [r['review_id'] for r in rows] == [999, 1000, 1001]
    assert rows[0]['reason'] == 'Still waiting on ordinary dependency writer π.'
    assert rows[0]['response_sha256'] == receipts[1]['response_sha256']
    assert rows[0]['truncated'] is False
    assert rows[-1]['reason'] == 'x' * 2000 and rows[-1]['truncated'] is True
    assert receipts == original


@pytest.mark.parametrize('failure', ['missing', 'changed_reason', 'changed_assignment',
                                    'malformed', 'oversize', 'symlink', 'empty'])
def test_optional_reviewer_rationale_never_uses_unbound_history(tmp_path, failure):
    receipt, path = review_response(tmp_path, 3396, '' if failure == 'empty' else 'Wait on task 171.')
    if failure == 'missing':
        path.unlink()
    elif failure.startswith('changed_'):
        response = json.loads(path.read_text())
        response['reason' if failure == 'changed_reason' else 'assignments'] = 'changed'
        path.write_text(json.dumps(response))
    elif failure == 'malformed':
        path.write_text('{')
    elif failure == 'oversize':
        path.write_bytes(b' ' * (4 * 1024 * 1024 + 1))
    elif failure == 'symlink':
        retained = tmp_path / 'retained.json'
        path.rename(retained)
        path.symlink_to(retained)
    assert handoffs.reviewer_rationales(tmp_path, [receipt]) == []


def completed(root, task_id, *, content=b'prior helper', relative='.trellis/scratch/helpers.lean'):
    path = root / str(task_id) / 'repo' / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(content)
    return dict(id=task_id, status={'status': 'resolved'},
                report={'summary': 'Saved helper development.', 'comments': f'Read `{relative}`.'})


def test_worker_handoffs_copy_only_reported_completed_files_and_reach_next_worker(project):
    repo, runtime, config = project
    root = runtime / 'dispatch'
    reviewer = root / 'review-99/repo'; reviewer.mkdir(parents=True)
    tasks = [completed(root, 9), completed(root, 10, content=b'another helper')]
    live = completed(root, 11); live['status'] = {'status': 'running'}; tasks.append(live)
    unreferenced = root / '9/repo/.trellis/scratch/unreferenced.lean'
    unreferenced.write_text('unreferenced probe')
    original = copy.deepcopy(tasks)
    before = source_manifest(repo)
    copied = handoffs.worker_handoffs(root, reviewer, tasks)
    assert [entry['task_id'] for entry in copied] == [9, 10]
    paths = [entry['files'][0]['path'] for entry in copied]
    assert paths == [f'.trellis/scratch/worker-handoffs/task-{id}/helpers.lean' for id in (9, 10)]
    assert (reviewer / paths[0]).read_bytes() == b'prior helper'
    assert (reviewer / paths[1]).read_bytes() == b'another helper'
    assert not list(reviewer.rglob('unreferenced.lean'))
    assert tasks == original and source_manifest(repo) == before
    target = root / '12'
    comments = f'Reuse `{paths[0]}` within this task, checking the proof.'
    prepare(runtime, repo, config, target, {'in_flight_request': {'id': 100, 'reviewer_comments': comments}},
            expected_manifest=before, handoff_repo=reviewer, seed_packages=False)
    private = target / 'repo'
    ensure_worker_scratch_workspace(private, reset=True, reviewer_comments=comments)
    assert (private / paths[0]).read_bytes() == b'prior helper'
    assert not (private / paths[1]).exists()
    assert source_manifest(private) == before == source_manifest(repo)
    assert (private / paths[0]).stat().st_ino != (reviewer / paths[0]).stat().st_ino


@pytest.mark.parametrize('failure', ['missing', 'symlink', 'traversal', 'file_limit', 'total_limit'])
def test_worker_handoff_failures_are_explicit_and_bounded(tmp_path, monkeypatch, failure):
    root, reviewer = tmp_path / 'dispatch', tmp_path / 'reviewer'
    reviewer.mkdir()
    task = completed(root, 9, content=b'helper')
    source = root / '9/repo/.trellis/scratch/helpers.lean'
    if failure == 'missing':
        source.unlink()
    elif failure == 'symlink':
        source.unlink(); external = tmp_path / 'outside'; external.write_text('private')
        source.symlink_to(external)
    elif failure == 'traversal':
        task['report']['comments'] = 'Read `.trellis/scratch/../../outside`.'
    elif failure == 'file_limit':
        monkeypatch.setattr(handoffs, 'MAX_FILES', 0)
    else:
        monkeypatch.setattr(handoffs, 'MAX_BYTES', 2)
    result = handoffs.worker_handoffs(root, reviewer, [task])
    assert result[0]['files'] == [] and result[0]['unavailable']
    assert list(reviewer.rglob('*.lean')) == []


def test_partial_handoff_preparation_preserves_existing_bytes(tmp_path):
    root, reviewer = tmp_path / 'dispatch', tmp_path / 'reviewer'
    reviewer.mkdir()
    task = completed(root, 9)
    first = handoffs.worker_handoffs(root, reviewer, [task])
    path = reviewer / first[0]['files'][0]['path']
    inode = path.stat().st_ino
    assert handoffs.worker_handoffs(root, reviewer, [task]) == first
    assert path.stat().st_ino == inode
    (root / '9/repo/.trellis/scratch/helpers.lean').write_text('changed later')
    again = handoffs.worker_handoffs(root, reviewer, [task])
    assert again[0]['files'] == [] and 'different bytes' in again[0]['unavailable'][0]
    assert path.read_bytes() == b'prior helper'


def test_worker_handoff_reports_unscanned_reference_text(tmp_path, monkeypatch):
    root, reviewer = tmp_path / 'dispatch', tmp_path / 'reviewer'
    reviewer.mkdir()
    task = completed(root, 9)
    monkeypatch.setattr(handoffs, 'MAX_REPORT_CHARS', 8)
    result = handoffs.worker_handoffs(root, reviewer, [task])
    assert result[0]['files'] == []
    assert 'later file references were not copied' in result[0]['unavailable'][0]


@pytest.mark.parametrize('adopting', [False, True])
def test_stuck_audit_handoff_is_bounded_and_frozen_before_launch(project, adopting):
    import json
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver

    repo, runtime, config = project
    root = runtime / 'dispatch'
    latest = completed(root, 30)
    old = completed(root, 1, content=b'old note')
    running = completed(root, 29, content=b'live note'); running['status'] = {'status': 'running'}
    missing = completed(root, 28); (root / '28/repo/.trellis/scratch/helpers.lean').unlink()
    status = dict(request={'kind': 'StuckMathAudit', 'id': 100},
                  tasks={str(t['id']): t for t in (old, missing, running, latest)})
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root, driver.runtime, driver.repo = root, runtime, repo
    driver.state = lambda: dict(dispatch={}, in_flight_request=status['request'])
    driver.native = SimpleNamespace(locked=nullcontext, _call=lambda _: {'source': 'frozen'})
    driver._prepare = lambda directory, private, **kwargs: prepare(runtime, repo, config, directory, private,
        expected_manifest=source_manifest(repo), seed_packages=False, **kwargs)
    directory = root / 'exclusive-100'
    if adopting:
        driver._prepare(directory, dict(dispatch=None, in_flight_request=status['request']))
        (directory / 'execution').mkdir()
        (directory / 'execution/recipe.json').write_text('{}')
    original = source_manifest(repo)
    result, request, inputs = driver._prepare_exclusive(status)
    assert result == directory and request == status['request'] and inputs == {'source': 'frozen'}
    manifest = directory / 'worker-handoffs.json'
    if adopting:
        assert not manifest.exists()
        assert not (directory / 'repo/.trellis/scratch/worker-handoffs').exists()
        return
    frozen = manifest.read_bytes()
    copied = json.loads(frozen)
    assert [entry['task_id'] for entry in copied] == [28, 30]
    assert copied[0]['unavailable'] and not copied[0]['files']
    helper = directory / 'repo' / copied[1]['files'][0]['path']
    assert helper.read_bytes() == b'prior helper'
    assert source_manifest(directory / 'repo') == original == source_manifest(repo)
    (root / '30/repo/.trellis/scratch/helpers.lean').write_text('changed after preparation')
    driver._prepare_exclusive(status)
    assert manifest.read_bytes() == frozen and helper.read_bytes() == b'prior helper'


def test_empty_audit_handoff_manifest_is_not_recollected(project):
    import json
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver

    repo, runtime, config = project
    root = runtime / 'dispatch'
    status = dict(request={'kind': 'StuckMathAudit', 'id': 100}, tasks={})
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root, driver.runtime, driver.repo = root, runtime, repo
    driver.state = lambda: dict(dispatch={}, in_flight_request=status['request'])
    driver.native = SimpleNamespace(locked=nullcontext, _call=lambda _: {'source': 'frozen'})
    driver._prepare = lambda directory, private, **kwargs: prepare(runtime, repo, config, directory, private,
        expected_manifest=source_manifest(repo), seed_packages=False, **kwargs)
    directory, _, _ = driver._prepare_exclusive(status)
    assert json.loads((directory / 'worker-handoffs.json').read_text()) == []
    status['tasks']['30'] = completed(root, 30)
    driver._prepare_exclusive(status)
    assert json.loads((directory / 'worker-handoffs.json').read_text()) == []
    assert not (directory / 'repo/.trellis/scratch/worker-handoffs').exists()


def findings_task(root, *, turn=None):
    directory = root / '9'
    repo = directory / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('original')
    (repo / 'Tablet/B.lean').write_text('out of scope')
    base = source_manifest(repo)
    (directory / 'base-manifest.json').write_bytes(canonical_json(base))
    (directory / 'prepared.json').write_bytes(canonical_json(dict(request_id=99, source_head='base-head')))
    (repo / 'Tablet/A.lean').write_text('useful partial proof')
    execution = directory / ('execution' if turn is None else f'turns/{turn}/execution')
    execution.mkdir(parents=True)
    process = dict(pid=123, start_ticks=456, boot_id='fixture')
    for name in ('process', 'cleanup'):
        (execution / f'{name}.json').write_bytes(canonical_json(process))
    (execution / 'clock').mkdir()
    (execution / 'clock/config.json').write_bytes(canonical_json(dict(binding=dict(
        model='test-sol', provider='codex', reasoning_effort='high', home='private-home'))))
    raw_name = f'trellis_worker_99_{"result" if turn is None else f"submission-{turn}"}.raw.json'
    raw = repo / '.trellis/runtime/runtime/staging' / raw_name
    raw.parent.mkdir(parents=True)
    raw.write_bytes(canonical_json(dict(outcome='valid', summary='Still unaccepted')))
    diagnostic = repo / '.trellis/check-results/check.txt'
    diagnostic.parent.mkdir(parents=True)
    diagnostic.write_text('unsolved goal: x = x')
    task = dict(id=9, status=dict(status='resolved', outcome='timed_out'), report=None,
                assignment=dict(profile='sol-high', task_kind='lean_closure', base_snapshot='base-state',
                                scope=dict(nodes=['A'])))
    return directory, execution, base, task


def test_transport_findings_use_final_execution_and_preserve_turn_root_candidate(tmp_path, monkeypatch):
    root, reviewer = tmp_path / 'dispatch', tmp_path / 'reviewer'
    reviewer.mkdir()
    directory, original, base, task = findings_task(root, turn=1)
    monkeypatch.setattr(handoffs, 'alive', lambda _: False)
    resumed = directory / 'turns/1/transports/1/execution'
    resumed.mkdir(parents=True)
    for name in ('process.json', 'cleanup.json'):
        (resumed / name).write_bytes((original / name).read_bytes())
    (resumed / 'response.json').write_bytes(canonical_json(dict(raw=dict(outcome='invalid', summary='final transport diagnostics'))))
    staging = directory / 'repo/.trellis/runtime/runtime/staging'
    (staging / 'trellis_worker_99_submission-1-transport-1.raw.json').write_bytes(canonical_json(dict(outcome='invalid', summary='final raw')))
    sealed = directory / 'turns/1/candidate'
    identity = seal(directory / 'repo', sealed, task_id=9, base_manifest=base, inputs={}, response=dict(outcome='Invalid'))
    result = handoffs.worker_handoffs(root, reviewer, [task])
    value = json.loads((reviewer / result[0]['files'][0]['path']).read_bytes())
    assert value['candidate_sha256'] == identity
    diagnostics = {row['kind']: row['text'] for row in value['diagnostics']}
    assert 'final transport diagnostics' in diagnostics['worker_response']
    assert 'final raw' in diagnostics['raw_submission']
    assert 'Still unaccepted' not in diagnostics['raw_submission']


@pytest.mark.parametrize('turn', [None, 2])
def test_timeout_findings_are_bounded_cached_unaccepted_source_and_diagnostics(tmp_path, monkeypatch, turn):
    root, reviewer = tmp_path / 'dispatch', tmp_path / 'reviewer'
    reviewer.mkdir()
    directory, execution, _, task = findings_task(root, turn=turn)
    monkeypatch.setattr(handoffs, 'alive', lambda _: False)
    result = handoffs.worker_handoffs(root, reviewer, [task])
    assert len(result) == 1 and len(result[0]['files']) == 1
    assert not result[0]['unavailable']
    path = reviewer / result[0]['files'][0]['path']
    data = path.read_bytes()
    value = json.loads(data)
    assert value['accepted'] is False and value['advisory'] is True
    assert value['terminal_status']['outcome'] == 'timed_out'
    assert value['base_snapshot'] == 'base-state' and value['source_head'] == 'base-head'
    assert value['model'] == 'test-sol' and 'private-home' not in data.decode()
    assert [source['path'] for source in value['sources']] == ['Tablet/A.lean']
    assert value['sources'][0]['content'] == 'useful partial proof'
    assert {row['kind'] for row in value['diagnostics']} == {'raw_submission', 'check_diagnostic'}
    assert not (reviewer / 'Tablet').exists() and not (reviewer / 'last_invalid').exists()
    assert len(data) <= handoffs.MAX_FINDING_BYTES
    (directory / 'repo/Tablet/A.lean').write_text('later mutation must not replace evidence')
    second = tmp_path / 'reviewer2'; second.mkdir()
    again = handoffs.worker_handoffs(root, second, [task])
    assert (second / again[0]['files'][0]['path']).read_bytes() == data
    assert (directory / 'unaccepted-findings.json').read_bytes() == data


@pytest.mark.parametrize('turn', [None, 2])
def test_findings_prefer_sealed_candidate_over_retained_mutable_sources(tmp_path, monkeypatch, turn):
    root, reviewer = tmp_path / 'dispatch', tmp_path / 'reviewer'
    reviewer.mkdir()
    directory, execution, base, task = findings_task(root, turn=turn)
    monkeypatch.setattr(handoffs, 'alive', lambda _: False)
    candidate = execution.parent / 'candidate'
    identity = seal(directory / 'repo', candidate, task_id=9, base_manifest=base,
                    inputs={}, response=dict(outcome='Valid'))
    (directory / 'repo/Tablet/A.lean').write_text('later mutable source')
    result = handoffs.worker_handoffs(root, reviewer, [task])
    value = json.loads((reviewer / result[0]['files'][0]['path']).read_bytes())
    assert value['candidate_sha256'] == identity and value['source_origin'] == 'sealed_candidate'
    assert value['sources'][0]['content'] == 'useful partial proof'


@pytest.mark.parametrize('failure', ['live', 'cleanup', 'symlink', 'source_limit', 'byte_limit'])
def test_findings_refuse_live_or_unreaped_source_and_bound_large_captures(tmp_path, monkeypatch, failure):
    root, reviewer = tmp_path / 'dispatch', tmp_path / 'reviewer'
    reviewer.mkdir()
    directory, execution, _, task = findings_task(root)
    monkeypatch.setattr(handoffs, 'alive', lambda _: failure == 'live')
    if failure == 'cleanup':
        (execution / 'cleanup.json').write_text('{}')
    if failure == 'symlink':
        source = directory / 'repo/Tablet/A.lean'
        source.unlink(); source.symlink_to(directory / 'repo/Tablet/B.lean')
    if failure == 'source_limit':
        monkeypatch.setattr(handoffs, 'MAX_SOURCE_BYTES', 4)
    if failure == 'byte_limit':
        monkeypatch.setattr(handoffs, 'MAX_FINDING_BYTES', 16)
    result = handoffs.worker_handoffs(root, reviewer, [task])
    if failure == 'source_limit':
        value = json.loads((reviewer / result[0]['files'][0]['path']).read_bytes())
        assert value['sources'][0]['truncated'] and value['sources'][0]['content'] == 'usef'
    else:
        assert not result or not result[0]['files']
        assert not list(reviewer.rglob('*.json'))
        if failure != 'byte_limit':
            assert result[0]['unavailable']


def test_relevant_findings_survive_a_long_tail_of_unrelated_tasks():
    def task(number, node):
        return dict(id=number, status=dict(status='resolved'), assignment=dict(scope=dict(nodes=[node])))
    tasks = [task(1, 'A'), task(2, 'A'), task(3, 'A'), *[task(i, 'B') for i in range(4, 80)]]
    chosen = handoffs.relevant_tasks(tasks, ['A'])
    assert [t['id'] for t in chosen] == [3, 2]
    assert len(handoffs.relevant_tasks(tasks, ['A'], recent=20)) == 22


def test_fresh_worker_keeps_automatic_findings_and_correction_keeps_scratch(tmp_path):
    from trellis.runtime.bridge import _prepare_worker_support_files
    relative = '.trellis/scratch/worker-handoffs/task-9/unaccepted-findings.json'
    path = tmp_path / relative
    path.parent.mkdir(parents=True)
    path.write_text('{"sources": [{"path": "Tablet/A.lean", "content": "partial proof"}]}')
    request = dict(fresh_context=True, worker_handoffs=[dict(files=[dict(path=relative, advisory=True)])])
    first = _prepare_worker_support_files(repo_path=tmp_path, request=request)
    assert path.exists()
    assert relative in first['reviewer_handoff_status_text']
    assert 'Unaccepted prior work' in first['reviewer_handoff_status_text']
    note = tmp_path/'.trellis/scratch/continued.lean'
    note.write_text('retained correction work')
    _prepare_worker_support_files(repo_path=tmp_path, request=request, correction=True)
    assert note.read_text() == 'retained correction work'


@pytest.mark.parametrize('mode', ['exact', 'tampered', 'symlink', 'oversized'])
def test_sealed_evidence_handoff_is_advisory_exact_and_never_canonical(tmp_path, monkeypatch, mode):
    root, reviewer = tmp_path / 'dispatch', tmp_path / 'reviewer'; reviewer.mkdir()
    directory, _, base, task = findings_task(root)
    monkeypatch.setattr(handoffs, 'alive', lambda _: False)
    name = 'reference/model-evidence/P89_T2_probe.rs'
    source = directory / 'repo' / name; source.parent.mkdir(parents=True)
    content = 'fn main() { println!("observed"); }\n'
    source.write_text(content)
    identity = seal(directory / 'repo', directory / 'candidate', task_id=9,
                    base_manifest=base, inputs={}, response={'outcome': 'valid'})
    sealed = directory / 'candidate/files' / name
    if mode == 'tampered':
        sealed.write_text('altered after seal')
    elif mode == 'symlink':
        sealed.unlink(); sealed.symlink_to(source)
    elif mode == 'oversized':
        monkeypatch.setattr(handoffs, 'MAX_SOURCE_BYTES', 4)
    source.write_text('mutable worker copy is not the retained evidence')
    result = handoffs.worker_handoffs(root, reviewer, [task])[0]
    if mode in ('tampered', 'symlink'):
        assert result['unavailable'] and not result['files']
        return
    value = json.loads((reviewer / result['files'][0]['path']).read_text())
    assert value['candidate_sha256'] == identity and value['accepted'] is False and value['advisory'] is True
    row, = value['evidence']
    assert row['path'] == name and row['after'] == handoffs.digest(content.encode())
    if mode == 'exact':
        assert row['content'] == content
    else:
        assert 'content' not in row and 'size limit' in row['content_unavailable']
    assert not (reviewer / 'reference').exists(), 'handoff grants no publication or write authority'


def test_evidence_only_handoff_retains_exact_seal_and_bounded_metadata(tmp_path, monkeypatch):
    root = tmp_path / 'dispatch'
    directory, _, base, task = findings_task(root)
    monkeypatch.setattr(handoffs, 'alive', lambda _: False)
    (directory / 'repo/Tablet/A.lean').write_text('original')
    for n in range(18):
        path = directory / 'repo/reference/model-evidence' / f'{n:02}.txt'
        path.parent.mkdir(parents=True, exist_ok=True); path.write_text('evidence' * 100)
    seal(directory / 'repo', directory / 'candidate', task_id=9, base_manifest=base, inputs={}, response={})
    monkeypatch.setattr(handoffs, 'MAX_FINDING_BYTES', 5000)
    value = json.loads(handoffs._findings(directory, task))
    assert value['sources'] == [] and len(value['evidence']) == 16 and value['evidence_scan_truncated']
    assert any('content_unavailable' in row for row in value['evidence'])
    assert all(row['after'] == handoffs.digest(('evidence' * 100).encode()) for row in value['evidence'])
    assert len(canonical_json(value)) <= 5000


def test_new_evidence_handoff_preserves_older_cached_findings(tmp_path, monkeypatch):
    root = tmp_path / 'dispatch'
    directory, _, base, task = findings_task(root)
    monkeypatch.setattr(handoffs, 'alive', lambda _: False)
    old = canonical_json(dict(schema=1, advisory=True, accepted=False, sources=[{'content': 'old partial proof'}]))
    (directory / 'unaccepted-findings.json').write_bytes(old)
    path = directory / 'repo/reference/model-evidence/probe.txt'; path.parent.mkdir(parents=True)
    path.write_text('sealed observation')
    identity = seal(directory / 'repo', directory / 'candidate', task_id=9, base_manifest=base, inputs={}, response={})
    value = json.loads(handoffs._findings(directory, task))
    assert value['candidate_sha256'] == identity
    assert value['evidence'][0]['content'] == 'sealed observation'
    assert (directory / 'unaccepted-findings.json').read_bytes() == old
    assert (directory / 'unaccepted-evidence-findings.json').exists()


def test_large_sealed_bridge_snapshot_does_not_hide_bounded_evidence(tmp_path, monkeypatch):
    root = tmp_path / 'dispatch'
    directory, _, base, task = findings_task(root)
    monkeypatch.setattr(handoffs, 'alive', lambda _: False)
    path = directory / 'repo/reference/model-evidence/probe.txt'; path.parent.mkdir(parents=True)
    path.write_text('small sealed observation')
    # Real task8/9 manifests embed full checked snapshots and exceed 5MiB;
    # that host metadata must not consume the advisory source/output allowance.
    bridge = dict(status='Ok', response=dict(outcome='Valid', snapshot=dict(
        corr_current_fingerprints={f'N{n}': 'f' * 2048 for n in range(2200)})))
    identity = seal(directory / 'repo', directory / 'candidate', task_id=9,
                    base_manifest=base, inputs={}, response={'outcome': 'valid'}, bridge_response=bridge)
    manifest = directory / 'candidate/candidate.json'
    assert 4 * 1024 * 1024 < manifest.stat().st_size < handoffs.MAX_CANDIDATE_MANIFEST_BYTES
    data = handoffs._findings(directory, task)
    value = json.loads(data)
    assert value['candidate_sha256'] == identity and value['accepted'] is False
    row, = value['evidence']
    assert row['content'] == 'small sealed observation'
    assert row['after'] == handoffs.digest(row['content'].encode())
    assert 'corr_current_fingerprints' not in data.decode()
    assert len(data) < handoffs.MAX_FINDING_BYTES == 512 * 1024
    assert handoffs.MAX_SOURCE_BYTES == 128 * 1024
    with monkeypatch.context() as bounded:
        bounded.setattr(handoffs, 'MAX_CANDIDATE_MANIFEST_BYTES', manifest.stat().st_size - 1)
        with pytest.raises(ValueError, match='file size limit'):
            handoffs._findings(directory, task)
    # The manifest's complete hash remains mandatory, even when a cached
    # advisory capture already exists.
    manifest.write_bytes(manifest.read_bytes() + b' ')
    with pytest.raises(ValueError, match='candidate identity mismatch'):
        handoffs._findings(directory, task)


@pytest.mark.parametrize('origin', ['matching', 'missing', 'changed', 'bad_id', 'retained'])
def test_ordinary_review_delivers_only_native_bound_advisory_and_preserves_capture(project, origin):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver
    from trellis.dispatch.candidate import digest

    repo, runtime, config = project
    root = runtime / 'dispatch'
    relative = '.trellis/stuck-math-audit/cycle-937-request-3313/adjacent_interface_probe.lean'
    producer = root / 'review-3313/repo' / relative
    producer.parent.mkdir(parents=True)
    producer.write_bytes(b'exact original advisory\n')
    (producer.parent / 'unreferenced.lean').write_text('not requested')
    comments = f'Read `{relative}` — "quoted"\nnotes'
    handoff = dict(review_id=3313, comments_sha256=digest(
        json.dumps(comments, ensure_ascii=False, separators=(',', ':')).encode()))
    if origin == 'changed':
        handoff['comments_sha256'] = 'wrong'
    elif origin == 'bad_id':
        handoff['review_id'] = True
    request = dict(id=3317, kind='Review', reviewer_comments=comments)
    state = dict(in_flight_request=request, dispatch=dict(
        worker_handoff=None if origin == 'missing' else handoff))
    frozen = root / 'reviews/3317/state.json'
    frozen.parent.mkdir(parents=True)
    frozen.write_bytes(canonical_json(state))
    cursor = dict(request_id=3317)
    status = dict(reviewer=cursor, config=dict(profiles={}, compiler_slots=2, verifier_slots=1),
                  available_slots=1, draining=None, tasks={})
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root, driver.runtime, driver.repo = root, runtime, repo
    driver.native = SimpleNamespace(locked=nullcontext, _start_review_locked=lambda: status)
    driver.require_no_halt = lambda: None
    driver.measurements = SimpleNamespace(value={})
    driver._prepare = lambda directory, private, **kwargs: prepare(runtime, repo, config,
        directory, private, expected_manifest=source_manifest(repo), seed_packages=False, **kwargs)
    directory = root / 'review-3317'
    if origin == 'retained':
        private = copy.deepcopy(state); private['dispatch'] = None
        # A previously captured attempt remains byte-exact, even when its
        # advisory delivery was incomplete. Never inject into a retained run.
        driver._prepare(directory, private)
        receipt = (directory / 'prepared.json').read_bytes()
    if origin in ('changed', 'bad_id'):
        with pytest.raises(RuntimeError, match='native reviewer origin'):
            driver._start_review()
        assert not directory.exists()
        return
    assert driver._start_review() == cursor
    target = directory / 'repo' / relative
    assert target.exists() == (origin == 'matching')
    metadata = json.loads((directory / 'prepared.json').read_bytes())
    if origin == 'matching':
        assert target.read_bytes() == producer.read_bytes()
        assert metadata['reviewer_handoffs'][relative] == digest(producer.read_bytes())
    elif origin == 'retained':
        assert (directory / 'prepared.json').read_bytes() == receipt
    else:
        assert metadata['missing_reviewer_handoffs'] == [relative]
    assert not (directory / 'repo' / producer.parent.relative_to(root / 'review-3313/repo') / 'unreferenced.lean').exists()
    assert json.loads(frozen.read_bytes()) == state


def test_worker_materialization_delivers_audit_note_from_exact_assignment_review(project):
    from trellis.dispatch.driver import ProductionDriver
    repo, runtime, config = project
    root = runtime / 'dispatch'
    relative = '.trellis/stuck-math-audit/cycle-937-request-3313/probe.lean'
    producer = root / 'review-3313/repo' / relative
    producer.parent.mkdir(parents=True)
    producer.write_bytes(b'original assignment advisory\n')
    wrong = root / 'review-3317/repo' / relative
    wrong.parent.mkdir(parents=True)
    wrong.write_bytes(b'newest review is not the assignment producer\n')
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = root
    driver.state = lambda: dict(dispatch=dict(tasks={}))
    driver._prepare = lambda directory, private, **kwargs: prepare(runtime, repo, config,
        directory, private, expected_manifest=source_manifest(repo), seed_packages=False, **kwargs)
    assignment = dict(review=dict(request_id=3313), scope=dict(nodes=['A']))
    request = dict(id=3314, kind='Worker', reviewer_comments=f'Read `{relative}`.')
    driver._materialize({'144': dict(private_state=dict(in_flight_request=request), assignment=assignment)})
    assert (root / '144/repo' / relative).read_bytes() == producer.read_bytes()
    assert json.loads((root / '144/assignment.json').read_text()) == assignment

import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest

from trellis.chat_history import commit_chat_checkpoint, ensure_chat_repo, inspect_transcript
from trellis.dispatch.candidate import canonical_json, digest, source_manifest
from trellis.dispatch.transcripts import prepare_history, readonly_paths
from trellis.dispatch.workspace import prepare


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(canonical_json(value))


def completed(runtime, name, request_id, *, role='worker'):
    directory = runtime / 'dispatch' / name
    request = dict(id=request_id, cycle=7, kind=role, active_node='A', mode='local',
                   private_contract='do not publish raw requests')
    response = dict(kind=role, request_id=request_id, cycle=7,
                    status='Ok', outcome='Invalid', summary='Tried the direct proof.',
                    snapshot={'private': 'do not publish full state'})
    write(directory / 'prepared.json', dict(request_id=request_id))
    write(directory / 'dispatch-request.json', request)
    write(directory / 'execution/response.json', response)
    write(directory / 'execution/result.json', dict(returncode=0,
          response_sha256=digest(canonical_json(response))))
    process = dict(pid=123, start_ticks=456, boot_id='fixture')
    write(directory / 'execution/process.json', process)
    write(directory / 'execution/cleanup.json', process)
    artifact = f'trellis_{role}_{request_id}_' + ('decision' if role == 'review' else 'result')
    chat = directory / 'repo/.trellis/chats/live' / artifact
    write(chat / 'output.log', dict(type='item.completed', item=dict(type='agent_message',
          text=f'Actual {name} transcript: direct proof failed.')))
    (chat / 'prompt.txt').write_text(f'Actual {name} prompt')
    (chat / 'call.json').write_text('provider metadata must not be published')
    (directory / 'repo/secret.txt').write_text('sibling repository must not be mounted')
    return directory, chat


@pytest.fixture
def project(tmp_path):
    repo, runtime = tmp_path / 'canonical', tmp_path / 'runtime'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('accepted source')
    runtime.mkdir()
    config = repo / 'trellis.config.json'
    write(config, dict(repo_path=str(repo)))
    write(runtime / 'runtime_metadata.json', dict(repo_path=str(repo), config_path=str(config)))
    for args in [('init', '-q'), ('config', 'user.name', 'Test'),
                 ('config', 'user.email', 'test@example.invalid'), ('add', '.'),
                 ('commit', '-qm', 'base')]:
        subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)
    return repo, runtime, config


def test_recent_completed_chats_use_existing_paths_and_are_copied_only_once(project, monkeypatch):
    repo, runtime, config = project
    old, source = completed(runtime, '9', 783)
    # History must not parse multi-megabyte frozen worker requests.
    (old / 'dispatch-request.json').unlink()
    completed(runtime, 'review-798', 798, role='review')
    completed(runtime, 'exclusive-798', 798, role='stuck_math_audit')
    history = repo / '.trellis/logs/burst-history.jsonl'
    write(history, dict(request_id=778, kind='Review'))
    history.write_bytes(history.read_bytes() + b'\n')
    target = runtime / 'dispatch/review-800'
    state = dict(in_flight_request=dict(id=800, kind='Review'))
    prepare(runtime, repo, config, target, state, expected_manifest=source_manifest(repo), seed_packages=False)
    private = target / 'repo'
    rows = [json.loads(line) for line in (private / '.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert [row['request_id'] for row in rows] == [778, 783, 798, 798]
    assert all(row['advisory'] for row in rows[1:])
    assert rows[1]['kind'] == 'Worker' and rows[1]['cycle'] == 7
    assert 'active_node' not in rows[1]  # Legacy preparation has no compact context.
    linked = private / rows[1]['chats'][0]
    assert linked.is_symlink()
    assert inspect_transcript(linked)['entries'][0]['path'].endswith('/output.log')
    page = inspect_transcript(linked / 'output.log', contains='direct proof', limit=1)
    assert 'Actual 9 transcript' in page['entries'][0]['text']
    assert not (linked / 'call.json').exists()
    assert (linked / 'output.log').stat().st_ino != (source / 'output.log').stat().st_ino
    assert source_manifest(private) == source_manifest(repo)
    assert len(list((private / '.trellis/chats/live').iterdir())) == 3
    published = linked.resolve()
    assert not (published.parent / 'response.json').exists()
    assert 'private_contract' not in (published.parent / 'history.json').read_text()
    assert 'snapshot' not in (published.parent / 'history.json').read_text()
    (source / 'output.log').write_text('Later source mutation must not change saved history.')
    monkeypatch.setattr('trellis.dispatch.transcripts.shutil.copyfile',
                        lambda *args: pytest.fail('published transcripts must not be recopied'))
    other = runtime / 'dispatch/10/repo'
    prepare_history(runtime, repo, other)
    assert (other / rows[1]['chats'][0]).resolve() == published
    assert 'Actual 9 transcript' in (linked / 'output.log').read_text()
    # Restart/adoption preserves the complete frozen workspace, including history.
    marker = target / 'prepared.json'
    prepared = json.loads(marker.read_text())
    assert prepared['history'] == dict(cycle=None, kind='Review', active_node=None, mode=None)
    prepared['history']['active_node'] = 'advisory only'
    write(marker, prepared)
    prepare(runtime, repo, config, target, state, expected_manifest=source_manifest(repo), seed_packages=False)
    assert (private / '.trellis/logs/burst-history.jsonl').read_text().count('"attempt"') == 3


def test_selection_keeps_long_worker_finishing_after_40_reviews_and_requires_reaping(project):
    repo, runtime, _ = project
    completed(runtime, '1', 9)
    for request_id in range(100, 141):
        directory, _ = completed(runtime, f'review-{request_id}', request_id, role='review')
        os.utime(directory / 'execution/result.json', ns=(200, 200))
    # A long worker with a low request ID finishes after newer reviews.
    os.utime(runtime / 'dispatch/1/execution/result.json', ns=(300, 300))
    unfinished, _ = completed(runtime, '4', 1001)
    (unfinished / 'execution/result.json').unlink()
    unreaped, _ = completed(runtime, '5', 1002)
    write(unreaped / 'execution/cleanup.json', {'different': 'process'})
    alias = runtime / 'dispatch/compiler-view/view'
    alias.parent.mkdir()
    alias.symlink_to('/')
    work = runtime / 'dispatch/review-1003/repo'
    prepare_history(runtime, repo, work)
    rows = [json.loads(line) for line in (work / '.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert [row['request_id'] for row in rows] == [*range(102, 141), 9]
    assert len(list((repo / '.trellis/chats/parallel').iterdir())) == 40


def test_parallel_envelope_uses_compact_prepared_context_and_preserves_artifact_collisions(project):
    repo, runtime, _ = project
    directory, _ = completed(runtime, 'review-10', 10, role='review')
    write(directory / 'prepared.json', dict(request_id=10,
          history=dict(kind='Review', cycle=7, active_node='A', mode='local')))
    # A parallel review envelope has no top-level request ID or cycle.
    response = dict(kind='parallel_review', assignments=[{'opaque': 'not copied'}])
    write(directory / 'execution/response.json', response)
    write(directory / 'execution/result.json', dict(returncode=0, response_sha256=digest(canonical_json(response))))
    (directory / 'dispatch-request.json').unlink()
    completed(runtime, '1', 11)
    completed(runtime, '2', 11)
    work = runtime / 'dispatch/review-12/repo'
    prepare_history(runtime, repo, work)
    rows = [json.loads(line) for line in (work / '.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert rows[0]['kind'] == 'Review' and rows[0]['cycle'] == 7
    assert rows[0]['active_node'] == 'A' and rows[0]['assignments_count'] == 1
    assert 'opaque' not in json.dumps(rows)
    assert 'Actual 1 transcript' in (work / rows[1]['chats'][0] / 'output.log').read_text()
    assert 'Actual 2 transcript' in (Path(rows[2]['chats'][0]) / 'output.log').read_text()


def test_same_id_review_does_not_republish_inherited_audit_links_or_overwrite_same_role_history(project):
    repo, runtime, _ = project
    completed(runtime, 'exclusive-798', 798, role='stuck_math_audit')
    current = runtime / 'dispatch/review-798/repo'
    prepare_history(runtime, repo, current, request=dict(id=798, kind='Review'))
    inherited = current / '.trellis/chats/live/trellis_stuck_math_audit_798_result'
    assert inherited.is_symlink()
    _, own = completed(runtime, 'review-798', 798, role='review')
    following = runtime / 'dispatch/review-799/repo'
    prepare_history(runtime, repo, following, request=dict(id=799, kind='Review'))
    rows = [json.loads(line) for line in (following / '.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert len(rows) == 2 and not any('history_unavailable' in row for row in rows)
    assert (following / '.trellis/chats/live' / own.name / 'output.log').read_text() == (own / 'output.log').read_text()
    # Reused same-role IDs must leave the new provider's artifact path free.
    reused = runtime / 'dispatch/exclusive-799/repo'
    prepare_history(runtime, repo, reused, request=dict(id=798, kind='Review'))
    assert not (reused / '.trellis/chats/live' / own.name).exists()
    reused_rows = [json.loads(line) for line in (reused / '.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert Path(reused_rows[1]['chats'][0]).is_absolute()
    assert 'Actual review-798 transcript' in (Path(reused_rows[1]['chats'][0]) / 'output.log').read_text()


@pytest.mark.parametrize('failure', ['response', 'request_id', 'file_symlink', 'directory_symlink', 'oversize'])
def test_unavailable_history_does_not_publish_unsafe_or_unreceipted_bytes(project, monkeypatch, failure):
    repo, runtime, _ = project
    directory, chat = completed(runtime, '1', 10)
    if failure == 'response':
        (directory / 'execution/response.json').write_text('{}')
    elif failure == 'request_id':
        write(directory / 'prepared.json', dict(request_id=12))
    elif failure == 'file_symlink':
        (chat / 'output.log').unlink()
        (chat / 'output.log').symlink_to(directory / 'repo/secret.txt')
    elif failure == 'directory_symlink':
        moved = chat.with_name('other')
        chat.rename(moved)
        chat.symlink_to(moved)
    else:
        monkeypatch.setattr('trellis.dispatch.transcripts._CHAT_LIMIT', 1)
    work = runtime / 'dispatch/review-11/repo'
    prepare_history(runtime, repo, work)
    row = json.loads((work / '.trellis/logs/burst-history.jsonl').read_text())
    assert row['history_unavailable']
    assert not list((repo / '.trellis/chats/parallel').iterdir())
    assert not list((work / '.trellis/chats/live').iterdir())


def test_published_receipt_cannot_be_rebound(project):
    repo, runtime, _ = project
    directory, _ = completed(runtime, '1', 10)
    prepare_history(runtime, repo, runtime / 'dispatch/review-11/repo')
    write(directory / 'execution/result.json', dict(response_sha256='changed'))
    work = runtime / 'dispatch/review-12/repo'
    prepare_history(runtime, repo, work)
    row = json.loads((work / '.trellis/logs/burst-history.jsonl').read_text())
    assert 'different completion receipt' in row['history_unavailable']


def test_submission_history_is_per_turn_reaped_and_has_bounded_model_labels(project):
    repo, runtime, _ = project
    directory, chat = completed(runtime, '1', 10)
    (directory / 'execution').rename(directory / 'old-execution')
    for turn in (1, 2, 3):
        execution = directory / 'turns' / str(turn) / 'execution'
        shutil.copytree(directory / 'old-execution', execution)
        response = dict(kind='worker_submission', request_id=10, cycle=7, turn=turn,
                        raw=dict(outcome='Valid', summary=f'Candidate {turn}'),
                        provider_session_id='private-session', provider_execution={'private': 'receipt'})
        write(execution / 'response.json', response)
        write(execution / 'result.json', dict(returncode=0, response_sha256=digest(canonical_json(response))))
        os.utime(execution / 'result.json', ns=(turn, turn))
        write(execution / 'clock/config.json', dict(binding=dict(provider='codex', model='gpt-test',
              reasoning_effort='high', home='/private/home', thread_id='private-thread')))
        artifact = chat.with_name(f'trellis_worker_10_submission-{turn}')
        shutil.copytree(chat, artifact)
        (artifact / 'output.log').write_text(f'Turn {turn}')
        write(artifact / 'call.json', dict(provider='codex', model='gpt-test', session_id='private-session'))
    (directory / 'turns/3/execution/cleanup.json').unlink()
    work = runtime / 'dispatch/review-11/repo'
    prepare_history(runtime, repo, work)
    rows = [json.loads(line) for line in (work / '.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert [row['submission_turn'] for row in rows] == [1, 2]
    assert [row['summary'] for row in rows] == ['Candidate 1', 'Candidate 2']
    assert all(row['kind'] == 'Worker' and row['unaccepted'] for row in rows)
    assert all(row['model'] == 'gpt-test' and row['reasoning_effort'] == 'high' for row in rows)
    assert 'private' not in json.dumps(rows)
    for turn, row in enumerate(rows, 1):
        assert len(row['chats']) == 1
        saved = work / row['chats'][0]
        assert (saved / 'output.log').read_text() == f'Turn {turn}'
        assert not (saved / 'call.json').exists()
    (chat.with_name('trellis_worker_10_submission-1') / 'output.log').write_text('changed')
    again = runtime / 'dispatch/review-12/repo'
    prepare_history(runtime, repo, again)
    assert (again / rows[0]['chats'][0] / 'output.log').read_text() == 'Turn 1'


def test_submission_history_rejects_receipt_rebound_to_another_turn(project):
    repo, runtime, _ = project
    directory, _ = completed(runtime, '1', 10)
    turn = directory / 'turns/2'
    turn.mkdir(parents=True)
    (directory / 'execution').rename(turn / 'execution')
    response = dict(kind='worker_submission', request_id=10, turn=1, raw={})
    write(turn / 'execution/response.json', response)
    write(turn / 'execution/result.json', dict(response_sha256=digest(canonical_json(response))))
    work = runtime / 'dispatch/review-11/repo'
    prepare_history(runtime, repo, work)
    row = json.loads((work / '.trellis/logs/burst-history.jsonl').read_text())
    assert 'does not match its worker turn' in row['history_unavailable']


@pytest.mark.parametrize('kind, shared', [('Worker', True), ('review', True),
                                        ('stuck_math_audit', True), ('Sound', False)])
def test_launcher_passes_only_published_history_mounts(project, kind, shared):
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver
    repo, runtime, _ = project
    published = repo / '.trellis/chats/parallel'
    published.mkdir(parents=True)
    directory = runtime / 'dispatch/review-11'
    write(directory / 'dispatch-request.json', dict(id=11, kind=kind))
    seen = []
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = runtime / 'dispatch'
    driver.runtime, driver.view, driver.repo = runtime, runtime / 'compiler-view', repo
    driver.environment = {'TRELLIS_SANDBOX_EXTRA_RO_PATHS': '/existing/support'}
    driver.process_factory = lambda _: SimpleNamespace(start_or_adopt=
        lambda *args, **kwargs: seen.append(kwargs['environment']) or {'pid': 1})
    driver._process(directory, compiler='none', seconds=3600)
    assert seen[0]['TRELLIS_SANDBOX_EXTRA_RO_PATHS'] == '/existing/support' + (
        os.pathsep + str(published) if shared else '')
    assert driver.environment == {'TRELLIS_SANDBOX_EXTRA_RO_PATHS': '/existing/support'}


@pytest.mark.parametrize('role', ['worker', 'reviewer', 'stuck_math_audit'])
def test_isolated_agent_reads_actual_and_archived_chats_without_shared_write_access(project, monkeypatch, role):
    from trellis.config import SandboxConfig
    from trellis.sandbox import wrap_command
    if not shutil.which('bwrap'):
        pytest.skip('requires bubblewrap')
    probe = subprocess.run(['bwrap', '--ro-bind', '/', '/', '--unshare-pid', '/bin/true'],
                           capture_output=True, text=True, timeout=10)
    if probe.returncode:
        pytest.skip('bubblewrap namespaces unavailable: ' + probe.stderr.strip())
    repo, runtime, config = project
    directory, source = completed(runtime, '1', 10)
    chats = ensure_chat_repo(repo)
    archive = chats / 'cycle-0001/trellis_worker_1_result/output.log'
    archive.parent.mkdir(parents=True)
    archive.write_text('archived actual transcript')
    commit_chat_checkpoint(repo, tag='cycle-1')
    target = runtime / 'dispatch/review-11'
    request_kind = {'worker': 'Worker', 'reviewer': 'Review', 'stuck_math_audit': 'StuckMathAudit'}[role]
    prepare(runtime, repo, config, target, dict(in_flight_request=dict(id=11, kind=request_kind)),
            expected_manifest=source_manifest(repo), seed_packages=False)
    private = target / 'repo'
    mounts = readonly_paths(repo)
    assert mounts == [chats / 'parallel', chats / '.git/objects']
    monkeypatch.setenv('TRELLIS_SANDBOX_EXTRA_RO_PATHS', os.pathsep.join(map(str, mounts)))
    # Keep the fixture's operator home outside both repository trees.
    operator = runtime.parent / 'operator'
    operator.mkdir()
    monkeypatch.setattr(Path, 'home', lambda: operator)
    monkeypatch.setattr('trellis.sandbox._reviewer_source_snapshot', lambda: None)
    linked = private / '.trellis/chats/live' / source.name / 'output.log'
    program = (
        'import json, pathlib, subprocess; '
        f'p=pathlib.Path({str(linked)!r}); '
        'assert "Actual 1 transcript" in p.read_text(); '
        f'assert not pathlib.Path({str(directory / "repo/secret.txt")!r}).exists(); '
        f'assert not pathlib.Path({str(repo / "Tablet/A.lean")!r}).exists(); '
        f'assert not pathlib.Path({str(directory / "execution/result.json")!r}).exists(); '
        f'out=subprocess.check_output(["git", "--no-optional-locks", "-C", {str(private / ".trellis/chats")!r}, '
        '"show", "cycle-1:cycle-0001/trellis_worker_1_result/output.log"], text=True); '
        'assert out == "archived actual transcript"; '
        '\ntry: p.write_text("tamper")\nexcept OSError as e: assert e.errno == 30\nelse: raise AssertionError("shared history writable")\n'
        'print("history-readable-and-readonly")'
    )
    home = runtime.parent / f'home-{role}'
    home.mkdir()
    cmd = wrap_command(['/usr/bin/python3', '-c', program], role=role,
                       sandbox=SandboxConfig(enabled=True, backend='bwrap'), work_dir=private,
                       burst_home=home)
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=20)
    assert result.returncode == 0, result.stderr
    assert 'history-readable-and-readonly' in result.stdout


def test_nested_finalizers_do_not_duplicate_provider_transcripts(project):
    repo, runtime, _ = project
    directory, _ = completed(runtime, '1', 101)
    final = directory / 'acceptance/1/finalization/1/execution'
    for filename in ('response.json', 'result.json', 'process.json', 'cleanup.json'):
        final.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(directory / 'execution' / filename, final / filename)
    work = runtime / 'history-reader/repo'
    prepare_history(runtime, repo, work)
    rows = [json.loads(line) for line in (work / '.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert len(rows) == 1
    assert rows[0]['request_id'] == 101
    assert rows[0]['attempt'] == '1'


def test_archived_host_responses_do_not_change_historical_viewer_chats(project, monkeypatch):
    from trellis.dispatch import retained_files as retained
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    repo, runtime, _ = project
    directory, _ = completed(runtime, '1', 101)
    for relative in ('acceptance/1', 'acceptance/1/finalization/1'):
        execution = directory / relative / 'execution'
        for name in ('response.json', 'result.json', 'process.json', 'cleanup.json'):
            execution.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(directory / 'execution' / name, execution / name)
        response = execution / 'response.json'
        response.write_bytes(response.read_bytes() + b' ' * 10000)
        write(execution / 'result.json', dict(returncode=0, response_sha256=digest(response.read_bytes())))
        assert retained.archive(execution / 'response.json')
    work = runtime / 'history-reader/repo'
    prepare_history(runtime, repo, work)
    rows = [json.loads(line) for line in (work / '.trellis/logs/burst-history.jsonl').read_text().splitlines()]
    assert len(rows) == 1 and rows[0]['request_id'] == 101 and rows[0]['attempt'] == '1'
    assert (directory / 'execution/response.json').is_file()

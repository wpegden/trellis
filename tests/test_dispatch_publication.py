import json
import shutil
import pytest

from trellis.dispatch.candidate import CandidateError, source_manifest
from trellis.dispatch.publication import Publisher


@pytest.mark.parametrize('crash_at', ['source:Tablet/A.lean', 'source:Tablet/B.lean', 'kernel_committed'])
def test_recovery_completes_one_transaction_without_losing_other_work(tmp_path, crash_at):
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    for node in ['A', 'B', 'Unrelated']:
        (repo / f'Tablet/{node}.lean').write_text(node + ' accepted')
    before = source_manifest(repo)
    staged = tmp_path / 'checked'
    shutil.copytree(repo, staged)
    (staged / 'Tablet/A.lean').write_text('A new')
    (staged / 'Tablet/B.lean').unlink()
    publisher = Publisher(tmp_path / 'runtime', repo)
    applied = {}
    calls = []
    def commit(task, candidate, event):
        if task not in applied:
            calls.append(event)
            applied[task] = {'task_id': task, 'candidate_sha256': candidate, 'event_id': 50}
        return applied[task]
    def crash(stage):
        if stage == crash_at:
            raise RuntimeError('simulated power loss')
    with publisher.locked():
        publisher.prepare(1, 'a' * 64, staged, expected_manifest=before, kernel_event={'checked': 'kernel payload'})
        with pytest.raises(RuntimeError, match='power loss'):
            publisher.recover(commit, checkpoint=crash)
    reopened = Publisher(tmp_path / 'runtime', repo)
    with reopened.locked():
        assert reopened.recover(commit)['event_id'] == 50
        assert reopened.recover(commit) is None
    assert len(calls) == 1
    assert source_manifest(repo) == source_manifest(staged)
    assert (repo / 'Tablet/Unrelated.lean').read_text() == 'Unrelated accepted'


def test_recovery_refuses_unknown_writer_without_touching_anything(tmp_path):
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('old')
    staged = tmp_path / 'checked'
    shutil.copytree(repo, staged)
    (staged / 'Tablet/A.lean').write_text('new')
    publisher = Publisher(tmp_path / 'runtime', repo)
    with publisher.locked():
        publisher.prepare(1, 'a' * 64, staged, expected_manifest=source_manifest(repo), kernel_event={})
        (repo / 'Tablet/B.lean').write_text('unexpected writer')
        before = source_manifest(repo)
        with pytest.raises(CandidateError, match='unrelated source change'):
            publisher.recover(lambda *_: pytest.fail('must not commit'))
        assert source_manifest(repo) == before


def test_checked_publication_retains_work_until_native_commit(tmp_path):
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('old')
    staged = tmp_path / 'checked'
    shutil.copytree(repo, staged)
    (staged / 'Tablet/A.lean').write_text('new')
    publisher = Publisher(tmp_path / 'runtime', repo)
    before = source_manifest(repo)
    with publisher.locked():
        publisher.prepare(1, 'a' * 64, staged, expected_manifest=before, kernel_event={})
        assert (publisher.root / 'pending.json').exists()
        assert source_manifest(repo) == before
        assert publisher.recover(lambda *_: {'event_id': 5}) == {'event_id': 5}
        assert source_manifest(repo) == source_manifest(staged)


def independent_publications(tmp_path):
    repo = tmp_path / 'repo'
    (repo / 'Tablet').mkdir(parents=True)
    for node in ('A', 'Sibling', 'Removed'):
        (repo / f'Tablet/{node}.lean').write_text(node + ' old proof')
    (repo / 'Tablet/header.tex').write_text('exact existing header')
    for name in ('INDEX.md', 'README.md'):
        (repo / 'Tablet' / name).write_text('old generated support')
    baseline = source_manifest(repo)
    staged = tmp_path / 'checked'
    shutil.copytree(repo, staged)
    (staged / 'Tablet/A.lean').write_text('A checked proof')
    (staged / 'Tablet/Helper.lean').write_text('Helper checked proof')
    (staged / 'Tablet/Removed.lean').unlink()
    for name in ('INDEX.md', 'README.md'):
        (staged / 'Tablet' / name).write_text('checked docs without concurrent birth')
    (repo / 'Tablet/Sibling.lean').write_text('Sibling concurrently accepted proof')
    (repo / 'Tablet/Birth.lean').write_text('Birth concurrently accepted proof')
    for name in ('INDEX.md', 'README.md'):
        (repo / 'Tablet' / name).write_text('current docs without checked helper')
    return repo, staged, baseline


def merged_support_renderer(monkeypatch):
    from pathlib import Path
    calls = []
    def render(request):
        assert request['action'] == 'dispatch_render_tablet_support'
        merged = Path(request['repo_path'])
        assert not (merged / '.lake').exists() and not (merged / '.trellis').exists()
        assert (merged / 'Tablet/Sibling.lean').read_text() == 'Sibling concurrently accepted proof'
        assert (merged / 'Tablet/A.lean').read_text() == 'A checked proof'
        assert (merged / 'Tablet/header.tex').read_text() == 'exact existing header'
        nodes = sorted(path.stem for path in (merged / 'Tablet').glob('*.lean'))
        assert nodes == ['A', 'Birth', 'Helper', 'Sibling']
        calls.append(merged)
        return dict(status='dispatch_ok', output=dict(index_md_content='INDEX: ' + ','.join(nodes),
            readme_md_content='README: ' + ','.join(nodes), header_tex_content='must never replace header'))
    monkeypatch.setattr('trellis.runtime.kernel_cli.run_kernel_cli', render)
    return calls


def test_checked_delta_preserves_sibling_and_renders_merged_indexes(tmp_path, monkeypatch):
    repo, staged, baseline = independent_publications(tmp_path)
    expected, checked = source_manifest(repo), source_manifest(staged)
    calls = merged_support_renderer(monkeypatch)
    publisher = Publisher(tmp_path / 'runtime', repo)
    with publisher.locked():
        identity = publisher.prepare(1, 'a' * 64, staged, expected_manifest=expected,
            checked_baseline=baseline, kernel_event={'checked': 'native current response'})
        journal = publisher.root / identity
        record = json.loads((journal / 'record.json').read_text())
        assert record['before'] == expected
        assert record['after']['Tablet/Sibling.lean'] == expected['Tablet/Sibling.lean']
        assert record['after']['Tablet/Birth.lean'] == expected['Tablet/Birth.lean']
        assert 'Tablet/Removed.lean' not in record['after']
        assert 'Tablet/Sibling.lean' not in record['changes']
        assert 'Tablet/Birth.lean' not in record['changes']
        assert set(record['changes']) == {'Tablet/A.lean', 'Tablet/Helper.lean', 'Tablet/Removed.lean',
                                          'Tablet/INDEX.md', 'Tablet/README.md'}
        assert len(calls) == 1 and not calls[0].exists()
        assert not (journal / 'merged').exists()
        assert source_manifest(repo) == expected and source_manifest(staged) == checked
        publisher.recover(lambda *_: {'event_id': 6})
    assert source_manifest(repo) == record['after']
    assert source_manifest(staged) == checked, 'original receipt bytes remain immutable'
    assert (repo / 'Tablet/header.tex').read_text() == 'exact existing header'
    assert (repo / 'Tablet/INDEX.md').read_text() == 'INDEX: A,Birth,Helper,Sibling'
    assert (repo / 'Tablet/README.md').read_text() == 'README: A,Birth,Helper,Sibling'


@pytest.mark.parametrize('path,canonical_content', [
    ('Tablet/A.lean', 'independent conflicting proof'),
    ('Tablet/A.lean', 'A checked proof'),  # Equal after-bytes do not excuse a changed before identity.
    ('Tablet/Removed.lean', 'independent edited deletion target'),
    ('Tablet/Helper.lean', 'independent conflicting birth'),
    ('Tablet/header.tex', 'independent header edit'),
])
def test_checked_delta_rejects_own_before_mismatch(tmp_path, monkeypatch, path, canonical_content):
    repo, staged, baseline = independent_publications(tmp_path)
    if path.endswith('header.tex'):
        (staged / path).write_text('checked header edit')
    (repo / path).write_text(canonical_content)
    expected, checked = source_manifest(repo), source_manifest(staged)
    monkeypatch.setattr('trellis.runtime.kernel_cli.run_kernel_cli',
                        lambda _: pytest.fail('conflicting delta must not render'))
    publisher = Publisher(tmp_path / 'runtime', repo)
    with publisher.locked(), pytest.raises(CandidateError, match='checked source conflicts'):
        publisher.prepare(1, 'a' * 64, staged, expected_manifest=expected,
            checked_baseline=baseline, kernel_event={})
    assert source_manifest(repo) == expected and source_manifest(staged) == checked
    assert not (publisher.root / 'pending.json').exists()
    assert not list(publisher.root.glob('.prepare-*'))


@pytest.mark.parametrize('crash_at', ['source:Tablet/A.lean', 'source:Tablet/INDEX.md', 'kernel_committed'])
def test_merged_publication_recovers_without_original_checked_workspace(tmp_path, monkeypatch, crash_at):
    repo, staged, baseline = independent_publications(tmp_path)
    calls = merged_support_renderer(monkeypatch)
    publisher = Publisher(tmp_path / 'runtime', repo)
    committed = {}
    def commit(task, identity, event):
        return committed.setdefault(task, dict(candidate_sha256=identity, event_id=7))
    def crash(stage):
        if stage == crash_at:
            raise RuntimeError('power loss')
    with publisher.locked():
        identity = publisher.prepare(1, 'a' * 64, staged, expected_manifest=source_manifest(repo),
            checked_baseline=baseline, kernel_event={})
        after = json.loads((publisher.root / identity / 'record.json').read_text())['after']
        with pytest.raises(RuntimeError, match='power loss'):
            publisher.recover(commit, checkpoint=crash)
    shutil.rmtree(staged)
    reopened = Publisher(tmp_path / 'runtime', repo)
    with reopened.locked():
        assert reopened.recover(commit)['event_id'] == 7
        assert reopened.recover(commit) is None
    assert len(committed) == len(calls) == 1
    assert source_manifest(repo) == after
    assert (repo / 'Tablet/Sibling.lean').read_text() == 'Sibling concurrently accepted proof'
    assert (repo / 'Tablet/INDEX.md').read_text() == 'INDEX: A,Birth,Helper,Sibling'


@pytest.mark.parametrize('intact', [False, True])
def test_checked_delta_requires_original_authenticated_baseline(tmp_path, monkeypatch, intact):
    from trellis.dispatch import acceptance_receipts, publication_artifacts
    repo, staged, baseline = independent_publications(tmp_path)
    publisher = Publisher(tmp_path / 'runtime', repo)
    attempt = publisher.root.parent / '1/acceptance/1'
    attempt.mkdir(parents=True)
    shutil.move(staged, attempt / 'repo')
    (attempt / 'canonical.json').write_text(json.dumps({'key': {'task_id': 1}}))
    (attempt / 'base-manifest.json').write_text(json.dumps(baseline))
    monkeypatch.setattr(acceptance_receipts, 'reusable', lambda *_: {'checked': True} if intact else None)
    monkeypatch.setattr(publication_artifacts, 'stage', lambda *_: pytest.fail('must authenticate before staging'))
    monkeypatch.setattr('trellis.runtime.kernel_cli.run_kernel_cli', lambda _: pytest.fail('must authenticate before rendering'))
    expected = source_manifest(repo)
    with publisher.locked(), pytest.raises(CandidateError, match='authenticated host acceptance' if intact else 'intact host acceptance'):
        publisher.prepare(1, 'a' * 64, attempt / 'repo', expected_manifest=expected,
            checked_baseline={**baseline, 'Tablet/A.lean': 'b' * 64}, artifact_source=attempt, kernel_event={})
    assert source_manifest(repo) == expected and not (publisher.root / 'pending.json').exists()


def test_merged_publication_stages_artifacts_from_immutable_checked_repo(tmp_path, monkeypatch):
    from trellis.dispatch import acceptance_receipts, publication_artifacts
    repo, staged, baseline = independent_publications(tmp_path)
    publisher = Publisher(tmp_path / 'runtime', repo)
    attempt = publisher.root.parent / '1/acceptance/1'
    attempt.mkdir(parents=True)
    shutil.move(staged, attempt / 'repo')
    staged = attempt / 'repo'
    checked = source_manifest(staged)
    (attempt / 'canonical.json').write_text(json.dumps({'key': {'task_id': 1}}))
    (attempt / 'base-manifest.json').write_text(json.dumps(baseline))
    monkeypatch.setattr(acceptance_receipts, 'reusable', lambda *_: {'checked': True})
    merged_support_renderer(monkeypatch)
    response = {'current': 'native certificate response'}
    staged_calls = []
    def stage(source, canonical, journal, actual_response):
        assert source == staged and canonical == repo and actual_response == response
        assert source_manifest(source) == checked
        assert not (journal / 'merged').exists()
        staged_calls.append(True)
        return {'version': 1, 'nodes': {}}
    monkeypatch.setattr(publication_artifacts, 'stage', stage)
    with publisher.locked():
        identity = publisher.prepare(1, 'a' * 64, staged, expected_manifest=source_manifest(repo),
            checked_baseline=baseline, artifact_source=attempt, kernel_event={'response': response})
        record = json.loads((publisher.root / identity / 'record.json').read_text())
        assert record['version'] == 2 and record['artifacts'] == {'version': 1, 'nodes': {}}
        publisher.recover(lambda *_: {'event_id': 8}, publication_receipt=lambda *_: None)
    assert staged_calls == [True] and source_manifest(staged) == checked


@pytest.mark.parametrize('mutation', ['checked', 'canonical', 'renderer', 'invalid_output'])
def test_merged_preparation_refuses_changed_inputs_and_bad_renderer(tmp_path, monkeypatch, mutation):
    from pathlib import Path
    repo, staged, baseline = independent_publications(tmp_path)
    publisher = Publisher(tmp_path / 'runtime', repo)
    expected = source_manifest(repo)
    def render(request):
        if mutation == 'invalid_output':
            return {'status': 'dispatch_ok', 'output': {'index_md_content': 'missing README'}}
        target = staged if mutation == 'checked' else repo if mutation == 'canonical' else Path(request['repo_path'])
        (target / 'Tablet/Sibling.lean').write_text('changed during preparation')
        return {'status': 'dispatch_ok', 'output': {'index_md_content': 'index', 'readme_md_content': 'readme'}}
    monkeypatch.setattr('trellis.runtime.kernel_cli.run_kernel_cli', render)
    with publisher.locked(), pytest.raises(CandidateError, match='changed|invalid content'):
        publisher.prepare(1, 'a' * 64, staged, expected_manifest=expected,
            checked_baseline=baseline, kernel_event={})
    assert not (publisher.root / 'pending.json').exists()
    assert not list(publisher.root.glob('.prepare-*'))

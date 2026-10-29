import hashlib
import json
from pathlib import Path
import subprocess

import pytest

from trellis.dispatch.candidate import canonical_json
from trellis.dispatch.coarse_audits import materialize_recent, read_history, restrict_evidence


def store(repo, report):
    data = canonical_json(report)
    reference = dict(sha256=hashlib.sha256(data).hexdigest(), bytes=len(data))
    path = repo / f'.trellis-history/coarse-audits/{reference["sha256"]}.json'
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return reference, path


def test_bounded_cone_history_preserves_exact_bytes_and_verifies_git_fallback(tmp_path):
    repo = tmp_path / 'repo'; repo.mkdir()
    for args in [('init', '-q'), ('config', 'user.email', 'test@example.invalid'), ('config', 'user.name', 'Test')]:
        subprocess.run(['git', '-C', str(repo), *args], check=True)
    previous = None
    for cycle in range(5):
        report = dict(version=1, evidence=None, migrated_from='f'*64, context=dict(goals=['A'], previous={'A': previous} if previous else {}),
                      plan=dict(written_at_cycle=cycle, written_by_request=cycle, report=f'Advice {cycle}'), future_field='preserved')
        previous, path = store(repo, report)
    subprocess.run(['git', '-C', str(repo), 'add', '.'], check=True)
    subprocess.run(['git', '-C', str(repo), 'commit', '-qm', 'audit evidence'], check=True)
    data = path.read_bytes(); path.unlink()
    assert read_history(repo, previous) == (report, data)
    state = dict(in_flight_request={}, coarse_task_goal='A', coarse_goals=dict(goals=dict(A=dict(history_head=previous))))
    materialize_recent(repo, state)
    index = json.loads((repo / '.trellis/coarse-audit-history/index.json').read_text())
    assert list(index) == ['A']
    assert [row['cycle'] for row in index['A']['recent']] == [4, 3, 2]
    assert index['A']['older'] is not None
    assert (repo / index['A']['recent'][0]['path']).read_bytes() == data
    path.write_text('corrupt')
    with pytest.raises(ValueError, match='size'):
        read_history(repo, previous)
    path.unlink(); path.symlink_to(repo / 'somewhere')
    with pytest.raises(ValueError, match='symlink'):
        read_history(repo, previous)


def test_scoped_workspace_removes_unbound_current_sources_but_keeps_shared_authority(tmp_path):
    files = ['Tablet/A.lean', 'Tablet/A.tex', 'Tablet/B.lean', 'Tablet/B.tex',
             'Tablet/Preamble.lean', 'Tablet/Axioms.lean', 'Tablet/INDEX.md', 'Tablet/README.md',
             'process-memory/A/pm-a.md', 'process-memory/B/pm-b.md', 'process-memory/global/pm-g.md',
             'process-memory/INDEX.md', 'GOAL.md', 'paper.tex']
    for name in files:
        path = tmp_path / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_text(name)
    restrict_evidence(tmp_path, dict(goals=['A'], nodes=['A']))
    for name in ['Tablet/B.lean', 'Tablet/B.tex', 'Tablet/INDEX.md', 'Tablet/README.md',
                 'process-memory/B/pm-b.md', 'process-memory/INDEX.md']:
        assert not (tmp_path / name).exists()
    for name in ['Tablet/A.lean', 'Tablet/A.tex', 'Tablet/Preamble.lean', 'Tablet/Axioms.lean',
                 'process-memory/A/pm-a.md', 'process-memory/global/pm-g.md', 'GOAL.md', 'paper.tex']:
        assert (tmp_path / name).read_text() == name

import os

import pytest

from trellis.dispatch.artifacts import publish_probes
from trellis.dispatch.candidate import CandidateError


def test_exclusive_probe_publication_is_repeatable_and_preserves_old_evidence(tmp_path):
    attempt, canonical = tmp_path / 'attempt', tmp_path / 'canonical'
    name = '.trellis/stuck-math-audit/cycle-1-request-4/probe.lean'
    source = attempt / 'repo' / name
    source.parent.mkdir(parents=True)
    source.write_text('example : True := by trivial')
    response = dict(status='Ok', probe_paths=[name])
    publish_probes(attempt, canonical, response)
    publish_probes(attempt, canonical, response)
    assert (canonical / name).read_bytes() == source.read_bytes()
    source.write_text('changed')
    with pytest.raises(CandidateError, match='changed'):
        publish_probes(attempt, canonical, response)
    assert (canonical / name).read_text() == 'example : True := by trivial'


@pytest.mark.parametrize('form', ['symlink', 'hardlink', 'escape'])
def test_exclusive_probe_rejects_unsafe_references(tmp_path, form):
    attempt = tmp_path / 'attempt'
    name = '.trellis/stuck-math-audit/cycle-1-request-4/probe.lean'
    source = attempt / 'repo' / name
    source.parent.mkdir(parents=True)
    outside = tmp_path / 'outside'
    outside.write_text('evidence')
    if form == 'symlink':
        source.symlink_to(outside)
    elif form == 'hardlink':
        os.link(outside, source)
    else:
        name = '../outside'
    with pytest.raises(CandidateError):
        publish_probes(attempt, tmp_path / 'canonical', dict(status='Ok', probe_paths=[name]))

from contextlib import contextmanager
import json

import pytest
from test_dispatch_source_recovery import failed, worker_setup, setup, git
from trellis.dispatch import recoverable_source_retention as copies, source_recovery
from trellis.dispatch.storage import BackgroundCollector


@pytest.mark.parametrize('refuse_lease', [1, 2, 3])
def test_runtime_change_fences_each_root_and_completion(failed, monkeypatch, refuse_lease):
    collector, directory, observed, _ = failed
    lease = 0
    inside = False
    original = collector.retirement_lock
    @contextmanager
    def locked():
        nonlocal lease, inside
        with original() as acquired:
            lease += 1
            inside = True
            try: yield acquired
            finally: inside = False
    monkeypatch.setattr(collector, 'retirement_lock', locked)
    calls = []
    def admission():
        calls.append((lease, inside))
        if inside and lease == refuse_lease:
            raise RuntimeError('reader runtime changed')
    with pytest.raises(RuntimeError, match='reader runtime changed'):
        copies.retire(collector, directory, observed, apply=True, runtime_admission=admission)
    assert calls[0] == (0, False)
    assert calls[-1] == (refuse_lease, True)
    receipt = json.loads((directory/source_recovery.RECEIPT).read_bytes())
    assert not receipt.get('completed')
    remaining = sum((directory/path).exists() for path in ['repo/Tablet/A.lean','repo/reference/context.txt'])
    assert remaining == max(0, 3-refuse_lease)
    # A new compatible runtime completes the same durable recovery inventory.
    copies.retire(collector, directory, observed, apply=True, runtime_admission=lambda: None)
    assert json.loads((directory/source_recovery.RECEIPT).read_bytes())['completed']


def test_background_forwards_compatibility_gate_and_preserves_scope_on_refusal(failed):
    collector, directory, _, state = failed
    prepared=json.loads((directory/'prepared.json').read_text())
    git(directory/'repo','fetch','-q',str(collector.repo),prepared['source_head'])
    git(directory/'repo','update-ref','HEAD',prepared['source_head'])
    calls=[]
    def refuse():
        calls.append(True)
        raise RuntimeError('reader unavailable')
    background=BackgroundCollector(collector.root,repo=collector.repo,
        git_maintenance_threshold_bytes=None,retire_failed_sources=True,recoverable_source_admission=refuse)
    try:
        assert background.schedule(state)
        assert background.wait_idle(timeout=30)
        assert calls
        assert (directory/'repo/Tablet/A.lean').exists()
        assert (directory/'repo/reference/context.txt').exists()
        assert not (directory/source_recovery.INDEX).exists()
    finally: background.close()


def test_invalid_admission_rejected_before_starting_workers(tmp_path):
    with pytest.raises(ValueError,match='callable'):
        BackgroundCollector(tmp_path,recoverable_source_admission=True)

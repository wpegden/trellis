"""Host-protected package digest reuse must never replace evidence checks."""
import os
from pathlib import Path
import threading
import time

import pytest

from trellis.checker import portable_replay as portable
from test_portable_replay import imported, capture


@pytest.fixture(autouse=True)
def isolated_cache(monkeypatch):
    cache = portable._ProtectedDigestCache(capacity=4, stripes=2)
    monkeypatch.setattr(portable, '_PROTECTED_DIGESTS', cache)
    return cache


def test_hardlinked_packages_share_digest_but_mutable_inputs_do_not(tmp_path):
    one, two = tmp_path/'one', tmp_path/'two'
    one.write_bytes(b'exact package'); os.link(one, two)
    first = portable.HashBudget(); expected = first.file(one, protected_package=True)
    second = portable.HashBudget(); assert second.file(two, protected_package=True) == expected
    assert first.bytes == len(b'exact package') and second.bytes == 0
    assert second.cached_files == 1 and second.cached_bytes == len(b'exact package')
    mutable = portable.HashBudget(); assert mutable.file(two) == expected
    assert mutable.bytes == len(b'exact package') and mutable.cached_files == 0
    explicit = portable.HashBudget()
    assert explicit.file(two, expected=expected, protected_package=True) == expected
    assert explicit.bytes == len(b'exact package') and explicit.cached_files == 0


@pytest.mark.parametrize('mutation', ['rewrite_restore_mtime', 'replace', 'link_churn', 'mode'])
def test_metadata_and_inode_changes_never_borrow_digest(tmp_path, mutation):
    path = tmp_path/'artifact'; path.write_bytes(b'first')
    old = portable.HashBudget().file(path, protected_package=True); stamp = path.stat()
    if mutation == 'rewrite_restore_mtime':
        path.write_bytes(b'other'); os.utime(path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    elif mutation == 'replace':
        new = tmp_path/'new'; new.write_bytes(b'other'); os.replace(new, path)
    elif mutation == 'link_churn':
        os.link(path, tmp_path/'alias')
    else:
        path.chmod(0o600)
    budget = portable.HashBudget(); result = budget.file(path, protected_package=True)
    assert budget.bytes == 5 and budget.cached_files == 0
    assert (result == old) is (mutation in {'link_churn', 'mode'})


def test_hit_still_rechecks_path_and_original_inventory(tmp_path, monkeypatch, isolated_cache):
    path = tmp_path/'artifact'; path.write_bytes(b'first')
    before = portable._file_state(path.stat())
    portable.HashBudget().file(path, protected_package=True)
    get = isolated_cache.get
    def mutate(key):
        result = get(key); path.write_bytes(b'other'); return result
    monkeypatch.setattr(isolated_cache, 'get', mutate)
    with pytest.raises(portable.CaptureUnavailable, match='cached content binding'):
        portable.HashBudget().file(path, protected_package=True, initial_state=before)
    # A changed initial inventory cannot become a cached hit either.
    with pytest.raises(portable.Unavailable):
        portable.HashBudget().file(path, protected_package=True, initial_state=before)


def test_symlink_replacement_rejected_on_hit(tmp_path, monkeypatch, isolated_cache):
    path = tmp_path/'artifact'; path.write_bytes(b'first')
    target = tmp_path/'target'; target.write_bytes(b'first')
    portable.HashBudget().file(path, protected_package=True)
    get = isolated_cache.get
    def alias(key):
        result = get(key); path.unlink(); path.symlink_to(target); return result
    monkeypatch.setattr(isolated_cache, 'get', alias)
    with pytest.raises(portable.CaptureUnavailable):
        portable.HashBudget().file(path, protected_package=True)


def test_cached_reads_preserve_budget_and_returns_cannot_poison_cache(tmp_path):
    path = tmp_path/'artifact'; path.write_bytes(b'first')
    portable.HashBudget().file(path, protected_package=True)
    with pytest.raises(portable.CaptureUnavailable, match='byte limit'):
        portable.HashBudget(max_bytes=4).file(path, protected_package=True)
    budget = portable.HashBudget(max_bytes=5); value = budget.file(path, protected_package=True)
    assert budget.remaining == 0
    value['sha256'] = 'bad'
    assert portable.HashBudget().file(path, protected_package=True)['sha256'] != 'bad'


def test_cache_is_bounded_lru(tmp_path, isolated_cache):
    files = []
    for i in range(6):
        p = tmp_path/str(i); p.write_bytes(str(i).encode()); files.append(p)
        portable.HashBudget().file(p, protected_package=True)
    assert len(isolated_cache.values) == 4
    cold = portable.HashBudget(); cold.file(files[0], protected_package=True)
    assert cold.bytes == 1
    warm = portable.HashBudget(); warm.file(files[-1], protected_package=True)
    assert warm.cached_files == 1


def test_concurrent_captures_read_same_inode_once(tmp_path, monkeypatch):
    path = tmp_path/'artifact'; path.write_bytes(b'exact')
    entered, release = threading.Event(), threading.Event()
    original = portable.HashBudget._file
    reads = []
    def held(self, *args, **kwargs):
        reads.append(1); entered.set(); assert release.wait(5)
        return original(self, *args, **kwargs)
    monkeypatch.setattr(portable.HashBudget, '_file', held)
    results, errors = [], []
    def work():
        try:
            b = portable.HashBudget(); value = b.file(path, protected_package=True)
            results.append((value, b.bytes, b.cached_bytes))
        except BaseException as e: errors.append(e)
    threads = [threading.Thread(target=work) for _ in range(4)]
    for t in threads: t.start()
    assert entered.wait(5); release.set()
    for t in threads: t.join(5); assert not t.is_alive()
    assert not errors and len(reads) == 1 and len(results) == 4
    assert sum(x[1] for x in results) == 5 and sum(x[2] for x in results) == 15


def test_waiting_capture_deadline_does_not_revoke_owner_lock(tmp_path, isolated_cache):
    path = tmp_path/'artifact'; path.write_bytes(b'exact')
    key = portable._file_state(path.stat()); stripe = isolated_cache.stripes[hash(key)%2]
    with stripe:
        with pytest.raises(portable.CaptureUnavailable, match='deadline'):
            portable.HashBudget(seconds=0.01).file(path, protected_package=True)
        assert stripe.locked()
    assert portable.HashBudget().file(path, protected_package=True)['size_bytes'] == 5


def test_waiting_capture_cancellation_releases_no_foreign_lock(tmp_path, monkeypatch, isolated_cache):
    from trellis.checker import admission
    path = tmp_path/'artifact'; path.write_bytes(b'exact')
    key = portable._file_state(path.stat()); stripe = isolated_cache.stripes[hash(key)%2]
    calls = []
    def cancel():
        calls.append(1)
        if len(calls)>1: raise RuntimeError('cancelled')
    monkeypatch.setattr(admission, 'check_cancelled', cancel)
    with stripe:
        with pytest.raises(RuntimeError, match='cancelled'):
            portable.HashBudget().file(path, protected_package=True)
        assert stripe.locked()


def test_read_failure_releases_stripe_and_does_not_publish(tmp_path, monkeypatch, isolated_cache):
    path = tmp_path/'artifact'; path.write_bytes(b'exact')
    original = portable.HashBudget._file
    def fail(*args, **kwargs): raise OSError('read failed')
    monkeypatch.setattr(portable.HashBudget, '_file', fail)
    with pytest.raises(OSError): portable.HashBudget().file(path, protected_package=True)
    assert not isolated_cache.values
    monkeypatch.setattr(portable.HashBudget, '_file', original)
    assert portable.HashBudget().file(path, protected_package=True)['size_bytes'] == 5


def test_repeated_capture_preserves_exact_environment_and_observes_package_change(tmp_path, imported):
    setup, _ = imported; repo = setup(tmp_path/'repo')
    first, one = capture(repo); second, two = capture(repo)
    assert first == second and one['cached_files'] == 0 and two['cached_files'] == 1
    assert one['bytes_hashed'] - two['bytes_hashed'] == len(b'external artifact')
    path = repo/'.lake/packages/dependency/.lake/build/lib/lean/External.olean'
    stamp = path.stat(); path.write_bytes(b'changed artifact!')
    os.utime(path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    third, three = capture(repo)
    assert third != second and three['cached_files'] == 0


@pytest.mark.parametrize('mutation', ['bytes_restore_mtime', 'alias', 'new_resolution_candidate', 'link_churn'])
def test_warm_capture_keeps_final_path_absence_and_content_revalidation(tmp_path, imported, monkeypatch, mutation):
    setup, _ = imported; repo = setup(tmp_path/'repo')
    first, _ = capture(repo)
    artifact = repo/'.lake/packages/dependency/.lake/build/lib/lean/External.olean'
    before = artifact.stat(); original = portable._CapturePaths.validate
    def changed(paths, budget):
        assert budget.cached_files == 1
        if mutation == 'bytes_restore_mtime':
            artifact.write_bytes(b'changed artifact!')
            os.utime(artifact, ns=(before.st_atime_ns, before.st_mtime_ns))
        elif mutation == 'alias':
            target = tmp_path/'alias-target'; artifact.rename(target); artifact.symlink_to(target)
        elif mutation == 'new_resolution_candidate':
            (repo/'.lake/build/lib/lean/External.olean').write_bytes(b'unexpected import')
        else:
            os.link(artifact, tmp_path/'additional-package-link')
        return original(paths, budget)
    monkeypatch.setattr(portable._CapturePaths, 'validate', changed)
    actual, stats = portable.capture(repo, ['A', 'Preamble'], timeout_secs=60, bwrap_role='lake_compiler')
    if mutation == 'link_churn':
        assert actual == first
        assert stats['content_revalidation_reads'] == 1
        assert stats['cached_files'] == 1
    else:
        assert actual is None and stats['status'] == 'unavailable'


def test_host_custody_required_even_when_same_package_inode_is_cached(tmp_path, imported):
    setup, _ = imported; repo = setup(tmp_path/'repo')
    first, cold = capture(repo)
    actual, stats = portable.capture(repo, ['A', 'Preamble'], timeout_secs=60, bwrap_role='worker')
    assert actual == first
    assert stats['cached_files'] == stats['cached_bytes'] == 0
    assert stats['bytes_hashed'] == cold['bytes_hashed']


def test_warm_capture_reports_cache_hits_without_fabricating_reads(tmp_path, imported):
    from trellis.checker import telemetry
    setup, _ = imported; repo = setup(tmp_path/'repo'); capture(repo)
    measured = telemetry.Measurement(); token = telemetry._current.set(measured)
    try:
        _, stats = capture(repo)
    finally:
        telemetry._current.reset(token)
    row = measured.portable_captures[0]
    assert row['cached_files'] == stats['cached_files'] == 1
    assert row['cached_bytes'] == stats['cached_bytes'] == len(b'external artifact')
    assert row['bytes_hashed'] == row['timed_read_bytes'] == stats['bytes_hashed']

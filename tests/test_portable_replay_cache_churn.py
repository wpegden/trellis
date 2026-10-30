"""Package link churn requires fresh byte confirmation, never a relaxed cache key."""
import os
import threading

import pytest

from trellis.checker import portable_replay as portable
from test_portable_replay import imported, capture


@pytest.fixture(autouse=True)
def isolated_cache(monkeypatch):
    cache = portable._ProtectedDigestCache(capacity=8, stripes=2)
    monkeypatch.setattr(portable, '_PROTECTED_DIGESTS', cache)
    return cache


def inputs(tmp_path, change):
    path, alias = tmp_path / 'artifact', tmp_path / 'alias'
    path.write_bytes(b'original package')
    if change == 'unlink':
        os.link(path, alias)
    def mutate():
        if change == 'unlink':
            alias.unlink()
        else:
            os.link(path, alias)
    return path, mutate


@pytest.mark.parametrize('change', ['link', 'unlink'])
@pytest.mark.parametrize('warm', [False, True])
def test_stripe_wait_link_churn_confirms_original_bytes(tmp_path, isolated_cache, change, warm):
    path, mutate = inputs(tmp_path, change)
    before = portable._file_state(path.stat())
    expected = portable.HashBudget().file(path, protected_package=True) if warm else None
    index = hash(before) % len(isolated_cache.stripes)
    lock, waiting = threading.Lock(), threading.Event()
    class ObservedLock:
        def acquire(self, **kwargs):
            waiting.set()
            return lock.acquire(**kwargs)
        def release(self):
            lock.release()
    stripes = list(isolated_cache.stripes)
    stripes[index] = ObservedLock()
    isolated_cache.stripes = tuple(stripes)
    budget = portable.HashBudget()
    values, errors = [], []
    def work():
        try:
            values.append(budget.file(path, initial_state=before, protected_package=True))
        except BaseException as exc:
            errors.append(exc)
    lock.acquire()
    thread = threading.Thread(target=work)
    thread.start()
    try:
        assert waiting.wait(5)
        mutate()
    finally:
        lock.release()
        thread.join(5)
    assert not thread.is_alive() and not errors
    assert values[0] == (expected or portable.HashBudget().file(path))
    assert budget.files == (1 if warm else 2)
    assert budget.bytes == len(path.read_bytes()) * budget.files
    assert budget.cached_files == budget.cached_bytes == 0


@pytest.mark.parametrize('change', ['link', 'unlink'])
def test_cache_hit_link_churn_uses_charged_content_confirmation(tmp_path, monkeypatch, isolated_cache, change):
    path, mutate = inputs(tmp_path, change)
    expected = portable.HashBudget().file(path, protected_package=True)
    get = isolated_cache.get
    def changing_get(key):
        value = get(key)
        mutate()
        return value
    monkeypatch.setattr(isolated_cache, 'get', changing_get)
    budget = portable.HashBudget(max_bytes=path.stat().st_size)
    assert budget.file(path, protected_package=True) == expected
    assert budget.bytes == path.stat().st_size and budget.remaining == 0
    assert budget.content_revalidations == 1 and budget.cached_files == 0


@pytest.mark.parametrize('change', ['rewrite_restore_mtime', 'inode', 'mode', 'symlink'])
def test_changed_bytes_or_nonlink_identity_never_reuse_binding(tmp_path, monkeypatch, isolated_cache, change):
    path, _ = inputs(tmp_path, 'link')
    portable.HashBudget().file(path, protected_package=True)
    before = path.stat()
    get = isolated_cache.get
    def changing_get(key):
        value = get(key)
        if change == 'rewrite_restore_mtime':
            path.write_bytes(b'different bytes!')
            assert path.stat().st_size == before.st_size
            os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
        elif change == 'inode':
            other = tmp_path / 'replacement'; other.write_bytes(path.read_bytes())
            os.replace(other, path)
        elif change == 'mode':
            path.chmod(0o600)
        else:
            other = tmp_path / 'target'; other.write_bytes(path.read_bytes())
            path.unlink(); path.symlink_to(other)
        return value
    monkeypatch.setattr(isolated_cache, 'get', changing_get)
    with pytest.raises(portable.Unavailable):
        portable.HashBudget().file(path, protected_package=True)


def test_churn_fallback_preserves_deadline(tmp_path, monkeypatch, isolated_cache):
    path, mutate = inputs(tmp_path, 'link')
    portable.HashBudget().file(path, protected_package=True)
    budget = portable.HashBudget()
    get = isolated_cache.get
    def changing_get(key):
        value = get(key); mutate(); budget.deadline = 0
        return value
    monkeypatch.setattr(isolated_cache, 'get', changing_get)
    with pytest.raises(portable.CaptureUnavailable, match='deadline'):
        budget.file(path, protected_package=True)
    assert budget.bytes == 0


def test_cold_wait_churn_cannot_skip_second_read_budget(tmp_path, isolated_cache):
    path, mutate = inputs(tmp_path, 'link')
    before = portable._file_state(path.stat())
    class ChurningLock:
        def acquire(self, **kwargs):
            mutate()
            return True
        def release(self):
            pass
    isolated_cache.stripes = (ChurningLock(),)
    budget = portable.HashBudget(max_bytes=path.stat().st_size)
    with pytest.raises(portable.CaptureUnavailable, match='byte limit'):
        budget.file(path, initial_state=before, protected_package=True)
    assert budget.bytes == path.stat().st_size and budget.files == 1
    assert budget.remaining == 0 and not isolated_cache.values


def test_warm_wait_churn_rejects_rewritten_bytes_with_restored_mtime(tmp_path, isolated_cache):
    path, _ = inputs(tmp_path, 'link')
    portable.HashBudget().file(path, protected_package=True)
    before = path.stat()
    class RewritingLock:
        def acquire(self, **kwargs):
            path.write_bytes(b'different bytes!')
            os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
            return True
        def release(self):
            pass
    isolated_cache.stripes = (RewritingLock(),)
    budget = portable.HashBudget()
    with pytest.raises(portable.Unavailable, match='bytes changed'):
        budget.file(path, protected_package=True)
    assert budget.bytes == before.st_size and budget.cached_files == 0


def test_full_capture_keeps_exact_environment_when_package_link_changes(tmp_path, monkeypatch, isolated_cache, imported):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    first, _ = capture(repo)
    path = repo / '.lake/packages/dependency/.lake/build/lib/lean/External.olean'
    get = isolated_cache.get
    def changing_get(key):
        value = get(key)
        if value is not None:
            os.link(path, tmp_path / 'new_workspace_package')
        return value
    monkeypatch.setattr(isolated_cache, 'get', changing_get)
    second, stats = capture(repo)
    assert first == second and stats['status'] == 'captured'
    assert stats['content_revalidation_reads'] >= 1

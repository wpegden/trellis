import gzip
import json

import pytest

from trellis.dispatch import review_snapshot_files as files, review_snapshots as snapshots
from test_dispatch_review_snapshots import history, git
from test_dispatch_driver import reviewer_driver


def test_large_capture_is_directly_compressed_and_exact(tmp_path):
    source = tmp_path / 'canonical.json'
    raw = json.dumps({'payload': 'exact\\\n😀' * 20000}, ensure_ascii=False).encode()
    source.write_bytes(raw)
    path = tmp_path / 'reviews/7/state.json'
    archive = files.capture(path, source)
    assert archive.name == 'state.json.gz' and not path.exists()
    assert archive.stat().st_mode & 0o777 == 0o600
    assert archive.stat().st_size < len(raw) // 10
    assert gzip.decompress(archive.read_bytes()) == files.read_bytes(path) == raw
    assert files.read(path) == json.loads(raw)
    assert source.read_bytes() == raw
    before = archive.stat().st_ino
    assert files.capture(path, raw) == archive and archive.stat().st_ino == before
    with pytest.raises(RuntimeError, match='different source snapshot'):
        files.capture(path, raw + b' ')
    assert archive.stat().st_ino == before
    assert not list(path.parent.glob('.review-state-*'))


def test_small_and_legacy_plain_snapshots_remain_readable(tmp_path):
    path = tmp_path / 'state.json'
    assert files.capture(path, b'{"x":1}') == path
    assert files.read(path) == {'x': 1}
    path.write_bytes(b'{"large":"' + b'x' * 100000 + b'"}')
    before = path.stat().st_ino
    assert files.capture(path, path.read_bytes()) == path
    assert path.stat().st_ino == before and not path.with_suffix('.json.gz').exists()


@pytest.mark.parametrize('bad', ['dual', 'alias', 'hardlink', 'corrupt'])
def test_ambiguous_or_invalid_archives_are_not_adopted(tmp_path, monkeypatch, bad):
    monkeypatch.setattr(files, 'COMPRESS_AT', 1)
    path = tmp_path / 'state.json'; archive = files.capture(path, b'{"x":1}')
    if bad == 'dual':
        path.write_text('{"x":1}')
    elif bad == 'alias':
        saved = archive.with_name('saved.gz'); archive.rename(saved); archive.symlink_to(saved)
    elif bad == 'hardlink':
        archive.with_name('saved.gz').hardlink_to(archive)
    else:
        archive.write_bytes(archive.read_bytes()[:-5])
    with pytest.raises((ValueError, OSError, EOFError)):
        files.read(path)
    assert archive.exists()


def test_git_tracked_destination_is_never_compressed(tmp_path):
    git(tmp_path, 'init', '-q')
    path = tmp_path / 'state.json'; raw = b'{"x":"' + b'x' * 100000 + b'"}'
    path.write_bytes(raw); git(tmp_path, 'add', 'state.json')
    with pytest.raises(ValueError, match='Git tracked'):
        files.capture(path, raw)
    assert path.read_bytes() == raw and not path.with_suffix('.json.gz').exists()


def test_source_change_aborts_publication_and_removes_temporary(tmp_path, monkeypatch):
    source = tmp_path / 'source.json'; source.write_bytes(b'x' * 100000)
    path = tmp_path / 'review/state.json'; original = files.os.fsync
    changed = False
    def sync(fd):
        nonlocal changed
        if not changed:
            source.write_bytes(b'y' * 100000); changed = True
        return original(fd)
    monkeypatch.setattr(files.os, 'fsync', sync)
    with pytest.raises(ValueError, match='canonical input changed'):
        files.capture(path, source)
    assert files.location(path) is None and not list(path.parent.glob('.review-state-*'))


def test_alias_parent_is_rejected_before_creating_subdirectories(tmp_path):
    real = tmp_path / 'real'; real.mkdir()
    alias = tmp_path / 'alias'; alias.symlink_to(real, target_is_directory=True)
    with pytest.raises(ValueError, match='symlink'):
        files.capture(alias / 'new/state.json', b'{}')
    assert list(real.iterdir()) == []


@pytest.mark.parametrize('change', ['active', 'pin', 'tracked', 'record_write_failure'])
def test_compressed_retirement_preserves_guarded_inputs(history, monkeypatch, change):
    repo, runtime, captured, path = history
    archive = path.with_name('state.json.gz')
    archive.write_bytes(gzip.compress(path.read_bytes(), compresslevel=0)); path.unlink()
    original = archive.read_bytes()
    if change == 'active':
        current = runtime / 'protocol_state.json'; state = json.loads(current.read_bytes())
        state['dispatch']['reviewer'] = {'request_id': 7}; current.write_text(json.dumps(state))
    elif change == 'pin':
        path.with_name('retain-state.json').write_text('{}')
    elif change == 'tracked':
        git(runtime, 'init', '-q'); git(runtime, 'add', 'dispatch/reviews/7/state.json.gz')
    else:
        def fail(*args):
            raise OSError('write failed')
        monkeypatch.setattr(snapshots, 'atomic', fail)
    collector = snapshots.Collector(runtime / 'dispatch', repo)
    if change in ('active', 'record_write_failure'):
        with pytest.raises((ValueError, OSError)):
            collector.retire(7, apply=True)
    else:
        assert collector.retire(7, apply=True)['status'] in ('pinned', 'tracked')
    assert archive.read_bytes() == original and files.read(path) == captured


def test_compressed_review_adoption_uses_exact_original_inputs(reviewer_driver, monkeypatch):
    driver, _, _, preparations, _ = reviewer_driver
    monkeypatch.setattr(files, 'COMPRESS_AT', 1)
    cursor = driver._start_review()
    path = driver.root / 'reviews/7/state.json'; archive = files.location(path)
    assert archive.suffix == '.gz' and not path.exists()
    captured = files.read_bytes(path)
    identity = archive.stat().st_ino
    prepared = (driver.root / 'review-7/prepared.json').read_bytes()
    assert driver._start_review() == cursor
    assert files.read_bytes(path) == captured
    assert archive.stat().st_ino == identity
    assert (driver.root / 'review-7/prepared.json').read_bytes() == prepared
    assert len(preparations) == 2  # Adoption still validates the existing workspace.


def test_compressed_snapshot_retires_to_git_recovery(history):
    repo, runtime, captured, path = history
    # Use incompressible-but-Git-retained data so the recovery delta wins.
    raw = path.read_bytes()
    archive = path.with_name('state.json.gz')
    archive.write_bytes(gzip.compress(raw, compresslevel=0, mtime=0)); path.unlink()
    result = snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert result['status'] == 'retired' and not archive.exists()
    record = json.loads(path.with_name('state.git.json').read_bytes())
    assert record['original_encoding'] == 'gzip'
    assert snapshots.restore(repo, record) == captured


def test_compressed_snapshot_kept_until_commit_and_during_active_review(history):
    repo, runtime, captured, path = history
    archive = path.with_name('state.json.gz'); archive.write_bytes(gzip.compress(path.read_bytes(), compresslevel=0)); path.unlink()
    git(repo, 'reset', '--soft', 'HEAD^')
    assert snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)['status'] == 'not_committed'
    assert files.read(path) == captured


def test_gzip_recovery_never_discards_more_compact_original(history):
    repo, runtime, captured, path = history
    archive = path.with_name('state.json.gz'); archive.write_bytes(gzip.compress(path.read_bytes(), compresslevel=9)); path.unlink()
    result = snapshots.Collector(runtime / 'dispatch', repo).retire(7, apply=True)
    assert result['status'] == 'no_savings' and files.read(path) == captured

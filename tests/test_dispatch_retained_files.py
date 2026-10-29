import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

import pytest

from trellis.dispatch import retained_files as retained
from trellis.dispatch import acceptance_receipts as receipts
from trellis.dispatch.candidate import CandidateError


def put(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return path


@pytest.fixture
def task(tmp_path, monkeypatch):
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    root = tmp_path / 'runtime/dispatch'
    directory = root / '1'
    raw = b'{"cycle":3,"in_flight_request":{"id":7,"kind":"Worker"}}' + b' ' * 10000
    for attempt in ('', 'acceptance/1', 'acceptance/1/finalization/1'):
        put(directory / attempt / 'runtime/protocol_state.json', raw)
    put(root.parent / 'protocol_state.json', json.dumps(dict(dispatch=dict(tasks={
        '1': dict(status=dict(status='resolved'))}))).encode())
    return root, directory, raw


def test_terminal_archive_exact_bytes_authentication_and_restore(task):
    root, directory, raw = task
    before = {path: path.read_bytes() for path in retained.eligible_paths(directory)}
    assert retained.archive_terminal(root, directory, max_files=100) == 3
    for path, data in before.items():
        assert not path.exists()
        assert retained.read_bytes(path) == receipts.regular(path) == data
        assert receipts.regular_digest(path) == hashlib.sha256(data).hexdigest()
        assert retained.restore(path)
        assert path.read_bytes() == data and path.stat().st_nlink == 1
        assert not retained.restore(path)


@pytest.mark.parametrize('status', ['running', 'checking', 'reserved'])
def test_nonterminal_and_canonical_files_never_archived(task, status):
    root, directory, raw = task
    canonical = root.parent / 'protocol_state.json'
    canonical.write_text(json.dumps(dict(dispatch=dict(tasks={'1': dict(status=dict(status=status))}))))
    with pytest.raises(ValueError, match='native terminal'):
        retained.archive_terminal(root, directory)
    assert (directory / 'runtime/protocol_state.json').read_bytes() == raw
    with pytest.raises(ValueError, match='numbered task'):
        retained.archive_terminal(root, root.parent)
    assert canonical.exists()


@pytest.mark.parametrize('guard', ['pin', 'nested_pin', 'live', 'unacknowledged', 'wrong_cleanup'])
def test_archival_respects_all_attempt_pins_and_process_cleanup(task, monkeypatch, guard):
    root, directory, raw = task
    if guard.endswith('pin'):
        put(directory / ('acceptance/1/' if guard == 'nested_pin' else '') / 'retain-builds.json', b'{}')
    else:
        identity = dict(pid=123, boot_id='test', start_ticks=1)
        execution = directory / 'acceptance/1/finalization/1/execution'
        put(execution / 'process.json', json.dumps(identity).encode())
        if guard != 'unacknowledged':
            put(execution / 'cleanup.json', json.dumps(identity if guard == 'live' else {}).encode())
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: guard == 'live')
    with pytest.raises((ValueError, FileNotFoundError)):
        retained.archive_terminal(root, directory)
    assert (directory / 'runtime/protocol_state.json').read_bytes() == raw


@pytest.mark.parametrize('interruption', ['before_manifest', 'before_unlink'])
def test_interrupted_archive_retries_without_losing_original(task, monkeypatch, interruption):
    _, directory, raw = task
    path = directory / 'runtime/protocol_state.json'
    def interrupt(*_args):
        raise RuntimeError('injected interruption')
    with monkeypatch.context() as patch:
        if interruption == 'before_manifest':
            patch.setattr(retained, 'atomic', interrupt)
        with pytest.raises(RuntimeError, match='injected interruption'):
            retained.archive(path, recheck=interrupt if interruption == 'before_unlink' else lambda: None)
    assert path.read_bytes() == raw
    assert retained.archive(path)
    assert receipts.regular(path) == raw


@pytest.mark.parametrize('damage', ['truncated', 'bad_header', 'bad_checksum', 'changed_hash', 'small_length', 'large_length', 'missing_manifest'])
def test_archive_corruption_fails_closed(task, damage):
    _, directory, _ = task
    path = directory / 'runtime/protocol_state.json'
    assert retained.archive(path)
    archive, metadata = retained._paths(path)
    data = archive.read_bytes()
    if damage == 'truncated':
        archive.write_bytes(data[:-5])
    elif damage == 'bad_header':
        archive.write_bytes(b'broken' + data[6:])
    elif damage == 'bad_checksum':
        archive.write_bytes(data[:-8] + bytes([data[-8] ^ 255]) + data[-7:])
    elif damage == 'missing_manifest':
        metadata.unlink()
    else:
        record = json.loads(metadata.read_text())
        if damage == 'changed_hash':
            record['sha256'] = '0' * 64
        else:
            record['bytes'] = 1 if damage == 'small_length' else retained.MAX_BYTES + 1
        metadata.write_text(json.dumps(record))
    with pytest.raises((ValueError, FileNotFoundError)):
        retained.read_bytes(path)
    with pytest.raises((ValueError, FileNotFoundError)):
        retained.restore(path)
    assert not path.exists()


@pytest.mark.parametrize('alias', ['raw_symlink', 'raw_hardlink', 'archive_symlink', 'archive_hardlink', 'metadata_symlink', 'metadata_hardlink'])
def test_raw_and_archived_evidence_must_be_independent_files(task, alias):
    _, directory, _ = task
    path = directory / 'runtime/protocol_state.json'
    if not alias.startswith('raw'):
        retained.archive(path)
    archive, metadata = retained._paths(path)
    selected = path if alias.startswith('raw') else archive if alias.startswith('archive') else metadata
    other = selected.with_name(selected.name + '.other')
    if alias.endswith('symlink'):
        selected.rename(other)
        selected.symlink_to(other)
    else:
        os.link(selected, other)
    with pytest.raises(ValueError, match='symlink|independent'):
        retained.read_bytes(path)


def test_git_index_in_nested_repository_prevents_compression(task):
    root, directory, raw = task
    repo = directory / 'repo'
    environment = repo / '.trellis/kernel-replay-environments' / ('a' * 64 + '.json')
    put(environment, raw)
    subprocess.run(['git', 'init', '-q', str(repo)], check=True)
    subprocess.run(['git', '-C', str(repo), 'add', str(environment.relative_to(repo))], check=True)
    assert retained.archive_terminal(root, directory, max_files=100) == 3
    assert environment.read_bytes() == raw
    assert not retained._paths(environment)[0].exists()


def test_outer_git_index_prevents_private_state_compression(task):
    _, directory, raw = task
    outer = directory.parents[2]
    subprocess.run(['git', 'init', '-q', str(outer)], check=True)
    path = directory / 'runtime/protocol_state.json'
    subprocess.run(['git', '-C', str(outer), 'add', str(path.relative_to(outer))], check=True)
    assert not retained.archive(path)
    assert path.read_bytes() == raw


def test_restore_cli_retries_and_preserves_operator_pin(task):
    root, directory, raw = task
    retained.archive_terminal(root, directory, max_files=100)
    pin = put(directory / 'retain-builds.json', b'{"reason":"existing investigation","owner":"operator"}')
    before = pin.read_bytes()
    command = [sys.executable, '-m', 'trellis.dispatch.retained_files', '--runtime', str(root.parent), '--task', '1', '--restore']
    first = subprocess.run(command, capture_output=True, text=True, check=True)
    assert len(json.loads(first.stdout)['restored']) == 3
    second = subprocess.run(command, capture_output=True, text=True, check=True)
    assert json.loads(second.stdout)['restored'] == []
    assert pin.read_bytes() == before
    assert all(path.read_bytes() == raw for path in retained.eligible_paths(directory))
    pin.unlink()
    assert retained.archive_terminal(root, directory, max_files=100) == 3
    assert all(receipts.regular(path) == raw for path in retained.eligible_paths(directory))


def test_native_observed_terminal_witness_avoids_canonical_parse(task, monkeypatch):
    root, directory, _ = task
    (root.parent / 'protocol_state.json').write_bytes(b'not parsed by background collector')
    assert retained.archive_terminal(root, directory, terminal_tasks=frozenset({'1'})) == 2
    with pytest.raises(ValueError, match='terminal task witness'):
        retained.archive_terminal(root, directory, terminal_tasks=frozenset())


def test_archived_replay_environment_reader_preserves_content_binding(task):
    from trellis.checker import portable_replay as portable
    root, directory, _ = task
    environment = dict(version=portable.VERSION, padding='x' * 10000)
    identity = portable.digest(environment)
    repo = directory / 'repo'
    path = put(repo / portable.DIRECTORY / (identity + '.json'), json.dumps(environment).encode())
    assert retained.archive(path)
    assert portable.environment_for(repo, identity) == environment


def test_viewer_projects_archived_private_state(task):
    from importlib.util import spec_from_file_location, module_from_spec
    _, directory, _ = task
    path = directory / 'runtime/protocol_state.json'
    viewer = Path(__file__).resolve().parents[1] / 'viewer/parallel_state.py'
    spec = spec_from_file_location('retained_viewer_projection', viewer)
    module = module_from_spec(spec)
    spec.loader.exec_module(module)
    expected = module.project(path)
    assert retained.archive(path)
    assert module.project(path) == expected


@pytest.mark.parametrize('enabled', [False, True])
def test_legacy_archive_environment_does_not_enable_background_archival(task, monkeypatch, enabled):
    from trellis.dispatch.storage import BackgroundCollector
    root, directory, _ = task
    monkeypatch.setenv('TRELLIS_ARCHIVE_TERMINAL_EVIDENCE', '1' if enabled else '0')
    (root.parent / 'protocol_state.json').write_bytes(b'background must not parse this large snapshot again')
    collector = BackgroundCollector(root)
    state = dict(dispatch=dict(tasks={'1': dict(status=dict(status='resolved'))}, verifiers=dict(jobs={})))
    assert collector.schedule(state)
    collector._thread.join(timeout=10)
    assert not collector._thread.is_alive()
    paths = list(retained.eligible_paths(directory))
    assert sum(not path.exists() for path in paths) == 0


def test_operator_pin_or_changed_source_during_compression_keeps_original(task):
    root, directory, raw = task
    path = directory / 'runtime/protocol_state.json'
    guard = retained.terminal_guard(root, directory)
    def pinned():
        put(directory / 'retain-builds.json', b'{}')
        guard()
    with pytest.raises(ValueError, match='operator pin'):
        retained.archive(path, recheck=pinned)
    assert path.read_bytes() == raw
    (directory / 'retain-builds.json').unlink()
    def changed():
        path.write_bytes(b'changed during archive')
    with pytest.raises(ValueError, match='changed before'):
        retained.archive(path, recheck=changed)
    assert path.read_bytes() == b'changed during archive'


def test_compression_budget_counts_incompressible_attempts_across_tasks(task, monkeypatch):
    root, directory, raw = task
    second = root / '2'
    put(second / 'runtime/protocol_state.json', raw)
    attempted = []
    def no_savings(path, **_kwargs):
        attempted.append(path)
        return False
    monkeypatch.setattr(retained, 'archive', no_savings)
    budget = {'files': 2}
    for selected in (directory, second):
        retained.archive_terminal(root, selected, terminal_tasks={'1', '2'}, budget=budget)
    assert len(attempted) == 2


@pytest.mark.parametrize('damage', ['missing', 'symlink'])
def test_restore_cli_rejects_orphaned_archive_metadata(task, damage):
    root, directory, _ = task
    path = directory / 'runtime/protocol_state.json'
    retained.archive(path)
    archive, metadata = retained._paths(path)
    metadata.unlink()
    if damage == 'symlink':
        metadata.symlink_to(directory / 'nonexistent')
    result = subprocess.run([sys.executable, '-m', 'trellis.dispatch.retained_files', '--runtime',
        str(root.parent), '--task', '1', '--restore'], capture_output=True, text=True)
    assert result.returncode != 0
    assert archive.exists() and not path.exists()


@pytest.mark.parametrize('value', [None, [], 42, 'wrong'])
def test_archive_metadata_must_be_an_object(task, value):
    _, directory, _ = task
    path = directory / 'runtime/protocol_state.json'
    retained.archive(path)
    retained._paths(path)[1].write_text(json.dumps(value))
    with pytest.raises(CandidateError, match='invalid retained'):
        receipts.regular(path)
    with pytest.raises(ValueError, match='invalid retained'):
        retained.restore(path)


def test_incompressible_prefix_does_not_starve_later_files(task):
    root, directory, raw = task
    paths = list(retained.eligible_paths(directory))
    for path in paths[:2]:
        path.write_bytes(os.urandom(10000))
    assert retained.archive_terminal(root, directory, max_files=2) == 0
    assert all(path.exists() for path in paths)
    assert retained.archive_terminal(root, directory, max_files=2) == 1
    assert retained.read_bytes(paths[2]) == raw
    assert all(path.exists() for path in paths[:2])
    assert all(retained._unchanged_no_savings(path) for path in paths[:2])
    paths[0].write_bytes(raw)
    assert not retained._unchanged_no_savings(paths[0])

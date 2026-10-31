import json
import threading
from types import SimpleNamespace

import pytest

from trellis.checker.protocol import validate_request, ProtocolError
from trellis.checker.server import CheckerServer, _WorkspaceReaderWriterLock
from trellis.dispatch import candidate, publication_artifacts as artifacts
from trellis.dispatch.publication import Publisher, atomic
from trellis.runtime.bridge import _register_burst_token


def put(path, value):
    atomic(path, candidate.canonical_json(value))


def journal(tmp_path):
    repo = tmp_path / 'repo'; (repo / 'Tablet').mkdir(parents=True)
    (repo / 'Tablet/A.lean').write_text('new')
    runtime = tmp_path / 'runtime'
    publisher = Publisher(runtime, repo)
    after = candidate.source_manifest(repo)
    # Recovery tests exercise journal durability, not certificate issuance.
    files = {name: None for name in artifacts.paths('A')}
    files['lib/lean/Tablet/A.olean'] = candidate.digest(b'exact host artifact')
    record = dict(version=2, repo=str(repo), task_id=1, candidate_sha256='a' * 64,
        before={'Tablet/A.lean': candidate.digest(b'old')}, after=after,
        changes={'Tablet/A.lean': {'before': candidate.digest(b'old'), 'after': after['Tablet/A.lean']}},
        kernel_event={'response': {}}, artifacts={'version': 1, 'nodes': {'A': dict(
            source_closure_sha256='f' * 64, artifact_bundle=[], files=files)}})
    identity = candidate.digest(candidate.canonical_json(record))
    root = publisher.root / identity
    put(root / 'record.json', record)
    atomic(root / 'files/Tablet/A.lean', b'new')
    atomic(root / 'artifacts/lib/lean/Tablet/A.olean', b'exact host artifact')
    put(publisher.root / 'pending.json', {'identity': identity})
    return publisher, root, record, identity


@pytest.mark.parametrize('crash', ['source:Tablet/A.lean', 'artifacts_installed', 'kernel_committed'])
def test_v2_recovery_needs_only_durable_journal_not_original_attempt(tmp_path, monkeypatch, crash):
    publisher, root, record, identity = journal(tmp_path)
    (publisher.repo / 'Tablet/A.lean').write_text('old')
    calls = []
    def install(runtime, got_identity):
        assert runtime == tmp_path / 'runtime' and got_identity == identity
        assert candidate.source_manifest(publisher.repo) == record['after']
        artifacts.validate(root, record['artifacts'])
        calls.append('install')
    monkeypatch.setattr(artifacts, 'install_via_checker', install)
    committed = {}
    def commit(*args):
        committed.setdefault(identity, {'task_id': 1})
        return committed[identity]
    def checkpoint(stage):
        if stage == crash: raise RuntimeError('crash')
    with publisher.locked(), pytest.raises(RuntimeError, match='crash'):
        publisher.recover(commit, publication_receipt=lambda *_: committed.get(identity), checkpoint=checkpoint)
    assert not (tmp_path / 'runtime/dispatch/1').exists()
    with publisher.locked():
        assert publisher.recover(commit, publication_receipt=lambda *_: committed.get(identity)) == {'task_id': 1}
        assert publisher.recover(commit) is None
    assert len(committed) == 1 and calls


def test_corrupt_journal_artifact_refused_before_any_source_write(tmp_path):
    publisher, root, record, identity = journal(tmp_path)
    (publisher.repo / 'Tablet/A.lean').write_text('old')
    (root / 'artifacts/lib/lean/Tablet/A.olean').write_bytes(b'corrupt')
    with publisher.locked(), pytest.raises(candidate.CandidateError, match='artifact evidence changed'):
        publisher.recover(lambda *_: pytest.fail('must not commit'), publication_receipt=lambda *_: None)
    assert (publisher.repo / 'Tablet/A.lean').read_text() == 'old'


@pytest.mark.parametrize('kind', ['supervisor', 'worker', 'advisory', 'missing', 'scoped', 'wrong-runtime'])
def test_promotion_rpc_requires_current_canonical_supervisor_capability(tmp_path, monkeypatch, kind):
    repo = tmp_path / 'repo'; (repo / '.trellis').mkdir(parents=True)
    runtime = tmp_path / 'runtime'; runtime.mkdir()
    put(runtime / 'runtime_metadata.json', {'repo_path': str(repo)})
    server = CheckerServer(runtime)
    token = 'workspace:not-authorized' if kind == 'scoped' else 'test-token'
    if kind != 'missing':
        _register_burst_token(runtime, token=token, burst_id='test', kind=kind,
            request_id=0, cycle=0)
        if kind == 'wrong-runtime':
            registry = json.loads(server.burst_tokens_path.read_text()); registry['runtime_root'] = '/wrong'
            put(server.burst_tokens_path, registry)
        server._reload_burst_tokens_if_changed()
    invoked = []
    monkeypatch.setattr(artifacts, 'install_pending', lambda _: invoked.append(True) or {'identity': 'a' * 64})
    raw = dict(op='install_publication_artifacts', request_id=1)
    if kind != 'missing': raw['auth_token'] = token
    response = json.loads(server._dispatch_line(candidate.canonical_json(raw)))
    assert bool(invoked) == (kind == 'supervisor')
    if kind != 'supervisor': assert response['rpc_error']['kind'] == 'auth_required'


def test_promotion_rpc_does_not_accept_paths_or_source_selection():
    for field in ('repo_path', 'journal', 'source', 'command', 'nodes'):
        with pytest.raises(ProtocolError):
            validate_request(dict(op='install_publication_artifacts', request_id=1, **{field: '/untrusted'}))


def test_install_uses_workspace_then_sync_lock_without_abba_and_blocks_readers(tmp_path, monkeypatch):
    publisher, root, record, identity = journal(tmp_path)
    workspace = _WorkspaceReaderWriterLock()
    sync = threading.Lock()
    server = SimpleNamespace(runtime_root=tmp_path / 'runtime', worker_repo=publisher.repo,
        _workspace_lock=workspace, _sync_lock=sync, _invalidate_compile_cache_for_sync=lambda _: None)
    reader_entered, allow_readiness, read_done = threading.Event(), threading.Event(), threading.Event()
    installer_entered, allow_install, install_done = threading.Event(), threading.Event(), threading.Event()
    failure = []
    def reader():
        try:
            with workspace.read_lock():
                reader_entered.set()
                assert allow_readiness.wait(3)
                with sync: pass
            read_done.set()
        except BaseException as e: failure.append(e)
    def install(*args):
        installer_entered.set()
        assert allow_install.wait(3)
    monkeypatch.setattr(artifacts, 'install', install)
    monkeypatch.setattr('trellis.runtime.kernel_cli.run_kernel_cli', lambda _: {'status': 'dispatch_ok', 'output': None})
    def writer():
        try: artifacts.install_pending(server); install_done.set()
        except BaseException as e: failure.append(e)
    r = threading.Thread(target=reader); w = threading.Thread(target=writer)
    r.start(); assert reader_entered.wait(3); w.start()
    # The writer must wait for the reader without taking sync out from under
    # its currentness check. A sync→workspace writer deadlocks this scenario.
    allow_readiness.set(); assert read_done.wait(3)
    assert installer_entered.wait(3) and not install_done.is_set()
    late_read = threading.Event()
    def late_reader():
        with workspace.read_lock(): late_read.set()
    late = threading.Thread(target=late_reader); late.start()
    assert not late_read.wait(.05)
    allow_install.set()
    for thread in (r, w, late): thread.join(3); assert not thread.is_alive()
    assert install_done.is_set() and late_read.is_set() and not failure


def test_postcommit_recovery_preserves_native_rebuilt_artifacts(tmp_path, monkeypatch):
    publisher, root, record, identity = journal(tmp_path)
    final = tmp_path / 'canonical-artifact'
    calls = []
    def install(*_):
        calls.append('install'); final.write_bytes(b'old handoff packet')
    monkeypatch.setattr(artifacts, 'install_via_checker', install)
    receipt = None
    def commit(*_):
        nonlocal receipt
        final.write_bytes(b'new native replay-certified artifact')
        receipt = {'task_id': 1, 'candidate_sha256': record['candidate_sha256'], 'payload_sha256': 'exact'}
        return receipt
    def crash(stage):
        if stage == 'kernel_committed': raise RuntimeError('power loss after native commit')
    with publisher.locked(), pytest.raises(RuntimeError, match='power loss'):
        publisher.recover(commit, publication_receipt=lambda *_: receipt, checkpoint=crash)
    with publisher.locked():
        assert publisher.recover(lambda *_: pytest.fail('already committed'),
            publication_receipt=lambda *_: receipt) == receipt
    assert calls == ['install']
    assert final.read_bytes() == b'new native replay-certified artifact'


def test_delayed_checker_promotion_rechecks_native_commit_inside_locks(tmp_path, monkeypatch):
    publisher, root, record, identity = journal(tmp_path)
    workspace = _WorkspaceReaderWriterLock()
    sync = threading.Lock()
    server = SimpleNamespace(runtime_root=tmp_path / 'runtime', worker_repo=publisher.repo,
        _workspace_lock=workspace, _sync_lock=sync,
        _invalidate_compile_cache_for_sync=lambda _: pytest.fail('committed artifacts must stay untouched'))
    receipt = None
    def native(payload):
        assert payload['action'] == 'dispatch_publication_receipt'
        return {'status': 'dispatch_ok', 'output': receipt}
    monkeypatch.setattr('trellis.runtime.kernel_cli.run_kernel_cli', native)
    monkeypatch.setattr(artifacts, 'install', lambda *_: pytest.fail('late request must not install'))
    results = []
    with workspace:
        thread = threading.Thread(target=lambda: results.append(artifacts.install_pending(server)))
        thread.start()
        receipt = {'task_id': 1, 'payload_sha256': 'native-exact-receipt'}
    thread.join(3)
    assert not thread.is_alive() and results == [{'identity': identity}]

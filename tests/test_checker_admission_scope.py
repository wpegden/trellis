"""Compiler leases cover execution, not pure Lean observation preparation."""
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import threading
import pytest

from trellis.atomic_actions import observations
from trellis.checker import admission
from trellis.checker.protocol import validate_request
from trellis.checker.server import CheckerServer


def test_nonmathlib_prepare_and_compile_cache_hit_preserve_resident(tmp_path, monkeypatch):
    scheduler = admission.Admission(1)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    server = CheckerServer.__new__(CheckerServer)
    server.supervisor_repo = tmp_path
    server._lake_manifest_path = tmp_path / 'lake-manifest.json'
    server._lake_manifest_path.write_text(json.dumps(dict(packages=[])))
    server._last_successful_prepare_manifest = None
    server._dependency_cache_root = tmp_path / "host-cache"
    monkeypatch.setattr(server, '_maybe_synthesize_compile_hit', lambda *a, **kw: dict(returncode=0))
    monkeypatch.setattr(server, '_attach_replayed_cost', lambda *a: None)
    monkeypatch.setattr(observations, '_run_lake_command', lambda *a, **kw: (_ for _ in ()).throw(AssertionError('unexpected compiler')))
    evicted = []
    with scheduler.acquire('worker'):
        scheduler.retain('worker', lambda: evicted.append(True))
    for request in [dict(op='prepare_compiled_support', request_id=1),
                    dict(op='verify_node', node_name='A', request_id=2)]:
        assert server._dispatch_op(validate_request(request), {})[1] == 0
    assert not evicted
    assert 'worker' in scheduler.residents
    # A running compiler in a different thread must not block pure work.
    with ThreadPoolExecutor(1) as pool:
        with scheduler.acquire('worker'):
            result = pool.submit(server._dispatch_op,
                validate_request(dict(op='prepare_compiled_support', request_id=3)), {})
            assert result.result(timeout=2)[1] == 0


def test_lake_leaf_waits_for_admission_and_holds_it_until_reaped(tmp_path, monkeypatch):
    scheduler = admission.Admission(1)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    started, waiting = threading.Event(), threading.Event()
    original = observations._Wait4Popen
    owners = []

    class Process(original):
        def __init__(self, *args, **kwargs):
            owners.append(scheduler.active[threading.get_ident()]['owner'])
            started.set()
            super().__init__(*args, **kwargs)
        def __exit__(self, *args):
            assert threading.get_ident() in scheduler.active
            return super().__exit__(*args)

    monkeypatch.setattr(observations, '_Wait4Popen', Process)
    def run():
        with admission.authoritative_workspace('private'):
            waiting.set()
            return observations._run_lake_command(tmp_path, [], timeout_secs=5,
                lake_executable=Path('/bin/true'))
    with ThreadPoolExecutor(1) as pool:
        with scheduler.acquire('worker'):
            result = pool.submit(run)
            assert waiting.wait(2)
            assert not started.wait(.05)
        assert result.result(timeout=5)['returncode'] == 0
    assert owners == ['authoritative:private']
    assert not scheduler.active and not scheduler.cleanup_failed


def test_statement_cache_validates_real_provenance_without_reserving_compiler(tmp_path, monkeypatch):
    from test_local_closure_cache_prefilter import make_server
    from test_statement_transform_snapshot import request
    from trellis.checker import statement_transform
    server = make_server(tmp_path)
    # Populate the ordinary sidecar through its handler, using the fixture's
    # real source-closure and replay records and a synthetic Lean response.
    monkeypatch.setattr(observations, '_run_lake_command', lambda *a, **kw:
        dict(returncode=0, stdout=json.dumps(dict(status='ok', result=dict(refutation_type='¬ True')))))
    with server._workspace_lock:
        first, code, _ = statement_transform.handle(server, request())
    assert code == 0
    monkeypatch.setattr(observations, '_run_lake_command', lambda *a, **kw:
        (_ for _ in ()).throw(AssertionError('cached derivation compiled')))
    scheduler = admission.Admission(1)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    evicted = []
    with scheduler.acquire('worker'):
        scheduler.retain('worker', lambda: evicted.append(True))
    with server._workspace_lock:
        cached, code, extra = server._dispatch_op(request(), {})
    assert cached == first and code == 0
    assert extra['statement_transform_cache_hit'] is True
    assert not evicted and 'worker' in scheduler.residents


def test_isabelle_dispatch_keeps_coarse_admission(monkeypatch):
    scheduler = admission.Admission(1)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    server = CheckerServer.__new__(CheckerServer)
    def handle(request):
        assert scheduler.active[threading.get_ident()]['owner'] == 'authoritative'
        return dict(returncode=0), 0, {}
    monkeypatch.setattr(server, '_handle_isabelle_workspace_op', handle)
    response = server._dispatch_op(validate_request(dict(op='isabelle_build_session', request_id=1)), {})
    assert response[1] == 0
    assert not scheduler.active


def test_dependency_install_drains_readers_then_blocks_new_compilers():
    scheduler = admission.Admission(2)
    queued, started, release, follower_started = (threading.Event() for _ in range(4))
    evicted = []
    with scheduler.acquire("warm"):
        scheduler.retain("warm", lambda: evicted.append(True))
    def install():
        with scheduler.acquire("cache", priority=0, exclusive=True,
                               waiting=lambda _: queued.set()):
            started.set()
            assert release.wait(3)
    def follower():
        with scheduler.acquire("other", priority=0):
            follower_started.set()
    with ThreadPoolExecutor(2) as pool:
        with scheduler.acquire("first"):
            installing = pool.submit(install)
            assert queued.wait(2)
            following = pool.submit(follower)
            assert not started.wait(.05)
            assert not follower_started.is_set()
        try:
            assert started.wait(2)
            assert evicted == [True]
            assert not follower_started.wait(.05)
        finally:
            release.set()
        installing.result(timeout=3)
        following.result(timeout=3)
    assert follower_started.is_set()
    assert not scheduler.active and not scheduler.waiters


def test_dependency_install_cancellation_and_nested_upgrade_guard():
    scheduler = admission.Admission(2)
    queued, cancel = threading.Event(), threading.Event()
    def cancelled():
        if cancel.is_set():
            raise RuntimeError("cancelled")
    def install():
        with scheduler.acquire("cache", exclusive=True, cancelled=cancelled,
                               waiting=lambda _: queued.set()):
            pytest.fail("cancelled installer started")
    with ThreadPoolExecutor(1) as pool:
        with scheduler.acquire("first"):
            with pytest.raises(RuntimeError, match="cannot upgrade"):
                with scheduler.acquire("cache", exclusive=True):
                    pytest.fail("ordinary lease upgraded")
            result = pool.submit(install)
            assert queued.wait(2)
            cancel.set()
            with scheduler.condition:
                scheduler.condition.notify_all()
            with pytest.raises(RuntimeError, match="cancelled"):
                result.result(timeout=2)
    assert not scheduler.active and not scheduler.waiters
    with scheduler.acquire("cache", exclusive=True):
        with scheduler.acquire("cache", exclusive=True):
            assert scheduler.active[threading.get_ident()]["depth"] == 2
    assert not scheduler.active

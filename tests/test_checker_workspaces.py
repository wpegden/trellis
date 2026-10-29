"""Private checker contexts share admission without sharing sources or locks."""
from concurrent.futures import ThreadPoolExecutor
import json
import threading

import pytest

from trellis.checker import workspaces
from trellis.checker.protocol import ProtocolError
from trellis.checker.server import CheckerServer


def runtime_at(path, value):
    repo = path.parent / "repo"
    (repo / "Tablet").mkdir(parents=True)
    (repo / ".trellis").mkdir()
    (repo / "Tablet/A.lean").write_text(value)
    path.mkdir()
    (path / "runtime_metadata.json").write_text(json.dumps({"repo_path": str(repo)}))
    return path


@pytest.fixture
def roots(tmp_path):
    main = runtime_at(tmp_path / "runtime", "canonical")
    a = runtime_at(main / "dispatch/1/runtime", "first")
    b = runtime_at(main / "dispatch/2/runtime", "second")
    return main, a, b


def token(main, task):
    return workspaces.register(main, task)["TRELLIS_CHECKER_TOKEN"]


def test_owner_bound_authority_survives_clock_but_not_owner_change_or_revocation(roots, monkeypatch):
    main, task, _ = roots
    cap = workspaces.register(main, task, lifetime=None)['TRELLIS_CHECKER_TOKEN']
    finite = workspaces.register(main, task, lifetime=10)['TRELLIS_CHECKER_TOKEN']
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    monkeypatch.setattr(workspaces.time, 'time', lambda: 1e18)
    try:
        with router.acquire(cap):
            router.check_live(cap)
            with pytest.raises(ProtocolError, match='expired'):
                with router.acquire(finite): pass
            with monkeypatch.context() as changed:
                changed.setattr(workspaces, '_process_identity', lambda pid: dict(pid=pid, start_ticks='reused'))
                with pytest.raises(ProtocolError, match='expired'): router.check_live(cap)
            assert router.release(cap) == {'drained': False}
            with pytest.raises(ProtocolError, match='expired'): router.check_live(cap)
        assert router.release(cap) == {'drained': True}
    finally:
        router.shutdown()


def test_unreaped_dead_owner_cannot_keep_owner_bound_grant_alive():
    import os, subprocess, sys
    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
    try:
        grant = dict(owner=workspaces._process_identity(child.pid), expires=None)
        assert workspaces.capability_live(grant)
        child.kill()
        os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOWAIT)
        assert not workspaces.capability_live(grant)
    finally:
        child.kill(); child.wait()


def request(server, cap, op="verify_node"):
    return json.loads(server._dispatch_line(json.dumps(dict(
        op=op, request_id=17, node_name="A", auth_token=cap,
    )).encode()))


def test_workspace_registration_permanently_closes_uid_only_canonical_fallback(roots):
    main, a, _ = roots
    server = CheckerServer(main)
    cap = token(main, a)
    try:
        assert request(server, None)["rpc_error"]["kind"] == "auth_required"
        workspaces.revoke(main, cap)
        assert request(server, None)["rpc_error"]["kind"] == "auth_required"
    finally:
        server.shutdown()


def test_host_enrichment_is_bound_to_private_capability_and_shared_service(roots, monkeypatch):
    from trellis.checker import host_response
    main, a, _ = roots
    server = CheckerServer(main)
    service = object()
    server._job_service = service
    calls = []
    def enrich(workspace, scheduler):
        calls.append((workspace.runtime_root, scheduler))
        return {'kind': 'worker', 'status': 'Ok'}
    monkeypatch.setattr(host_response, 'enrich', enrich)
    try:
        assert request(server, None, 'host_response_enrichment')['rpc_error']['kind'] == 'auth_required'
        cap = token(main, a)
        assert request(server, cap, 'host_response_enrichment')['response']['status'] == 'Ok'
        assert calls == [(a, service)]
        workspaces.revoke(main, cap)
        assert request(server, cap, 'host_response_enrichment')['rpc_error']['kind'] == 'auth_required'
        assert len(calls) == 1
    finally:
        server._job_service = None
        server.shutdown()


def test_real_dispatch_uses_each_registered_source_and_shared_admission(roots, monkeypatch):
    from trellis.checker import admission, server as server_module
    main, a, b = roots
    server = CheckerServer(main)
    calls = []

    def compile_node(repo, node_name, **kw):
        text = (repo / "Tablet" / (node_name + ".lean")).read_text()
        calls.append((repo, kw["bwrap_role"], admission.ACTIVE))
        return dict(returncode=1, stdout=text, stderr="ordinary Lean diagnostic", timed_out=False, spawn_error="")

    monkeypatch.setattr(server_module.observations, "compile_node", compile_node)
    # No listener, job scheduler or compiler is started per workspace.
    monkeypatch.setattr(CheckerServer, "start", lambda self: pytest.fail("private checker must not start a service"))
    try:
        ca, cb = token(main, a), token(main, b)
        assert request(server, ca)["stdout"] == "first"
        assert request(server, cb)["stdout"] == "second"
        assert request(server, ca)["stdout"] == "first"
        assert all(role == "lake_compiler" and scheduler is admission.ACTIVE for _, role, scheduler in calls)
        assert calls[0][0] == calls[2][0] != calls[1][0]
        assert (server.worker_repo / "Tablet/A.lean").read_text() == "canonical"
        assert not (server.supervisor_repo / "Tablet/A.lean").exists()
    finally:
        server.shutdown()


def test_two_tokens_for_one_workspace_share_the_same_lock_and_cache(roots):
    main, a, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    first, second = token(main, a), token(main, a)
    try:
        with router.acquire(first) as x, router.acquire(second) as y:
            assert x is y
            assert x._workspace_lock is y._workspace_lock
        assert len(router.contexts) == 1
    finally:
        router.shutdown()


def test_release_ack_waits_for_active_authority_and_shared_tokens(roots):
    main, a, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    first, second = token(main, a), token(main, a)
    try:
        with router.acquire(first), router.acquire(second):
            assert router.release(first) == {'drained': False}
            with pytest.raises(ProtocolError, match='expired'):
                with router.acquire(first):
                    pytest.fail('revoked capability was reused')
        assert router.release(first) == {'drained': False}, 'other channel still owns the workspace'
        assert router.release(second) == {'drained': True}
        assert router.release(first) == {'drained': True}
        assert not router.contexts
    finally:
        router.shutdown()


def test_scoped_release_uses_real_rpc_without_canonical_fallback(roots, monkeypatch):
    main, a, _ = roots
    server = CheckerServer(main)
    cap = token(main, a)
    monkeypatch.setattr(server, '_handle_request', lambda _: pytest.fail('canonical fallback'))
    try:
        for _ in range(2):
            result = json.loads(server._dispatch_line(json.dumps(dict(
                op='release_workspace', request_id=21, auth_token=cap)).encode()))
            assert result == {'request_id': 21, 'drained': True}
        assert request(server, cap)['rpc_error']['kind'] == 'auth_required'
        missing = json.loads(server._dispatch_line(b'{"op":"release_workspace","request_id":22}'))
        assert missing['rpc_error']['kind'] == 'auth_required'
    finally:
        server.shutdown()


def test_release_never_treats_best_effort_backend_cleanup_as_acknowledged(roots):
    main, a, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    cap = token(main, a)
    try:
        with router.acquire(cap) as ctx:
            ctx._isabelle_session = object()
        with pytest.raises(ProtocolError, match='lacks verified process cleanup'):
            router.release(cap)
        assert a in router.contexts
        # No process was launched: clear the synthetic session fixture.
        ctx._isabelle_session = None
        assert router.release(cap)['drained']
    finally:
        router.shutdown()


def test_distinct_workspaces_do_not_serialize_on_one_workspace_lock(roots):
    main, a, b = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    caps = [token(main, a), token(main, b)]
    barrier = threading.Barrier(2)
    def check(cap):
        with router.acquire(cap) as ctx, ctx._workspace_lock:
            barrier.wait(timeout=5)
            return ctx.worker_repo
    try:
        with ThreadPoolExecutor(2) as executor:
            results = list(executor.map(check, caps))
        assert results[0] != results[1]
    finally:
        router.shutdown()


def test_distinct_authoritative_workspaces_share_two_compiler_slots(roots, monkeypatch):
    from trellis.checker import admission, server as server_module
    main, a, b = roots
    scheduler = admission.Admission(2)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    server = CheckerServer(main)
    barrier = threading.Barrier(2)
    owners = set()
    # This stub replaces the compiler leaf as well as compile_node's pure
    # preparation; real leaves acquire their own admission lease.
    @admission.admitted
    def compile_node(repo, node_name, **kw):
        with scheduler.condition:
            owners.update(r["owner"] for r in scheduler.active.values())
        barrier.wait(timeout=5)
        return dict(returncode=1, stdout=(repo / "Tablet/A.lean").read_text(), stderr="proof error")
    monkeypatch.setattr(server_module.observations, "compile_node", compile_node)
    try:
        caps = [token(main, a), token(main, b)]
        with ThreadPoolExecutor(2) as pool:
            results = list(pool.map(lambda cap: request(server, cap), caps))
        assert {r["stdout"] for r in results} == {"first", "second"}
        assert len(owners) == 2
        assert not scheduler.active
    finally:
        server.shutdown()


def test_revocation_cancels_queued_authority_and_leaves_canonical_lease_alone(roots, monkeypatch):
    from trellis.checker import admission
    main, a, _ = roots
    scheduler = admission.Admission(1)
    monkeypatch.setattr(admission, "ACTIVE", scheduler)
    server = CheckerServer(main)
    cap = token(main, a)
    entered = threading.Event()
    def queued():
        entered.set()
        return request(server, cap)
    try:
        with ThreadPoolExecutor(1) as pool:
            with scheduler.acquire("canonical"):
                future = pool.submit(queued)
                assert entered.wait(2)
                workspaces.revoke(main, cap)
                response = future.result(timeout=3)
                assert response["rpc_error"]["kind"] == "auth_required"
                assert len(scheduler.active) == 1
                assert next(iter(scheduler.active.values()))["owner"] == "canonical"
        assert not scheduler.waiters
    finally:
        server.shutdown()


def test_unknown_revoked_or_dead_owner_token_never_falls_back_to_canonical(roots, monkeypatch):
    main, a, _ = roots
    server = CheckerServer(main)
    monkeypatch.setattr(server, "_handle_request", lambda request: pytest.fail("canonical fallback"))
    cap = token(main, a)
    try:
        workspaces.revoke(main, cap)
        assert request(server, cap)["rpc_error"]["kind"] == "auth_required"
        cap = token(main, a)
        monkeypatch.setattr(workspaces, "_process_identity", lambda pid: {"pid": pid, "start": "different"})
        assert request(server, cap)["rpc_error"]["kind"] == "auth_required"
    finally:
        server.shutdown()


def test_registry_cannot_target_canonical_or_peer_repo(roots):
    main, a, b = roots
    (a / "runtime_metadata.json").write_text(json.dumps({"repo_path": str(main.parent / "repo")}))
    with pytest.raises(ValueError, match="own sibling"):
        token(main, a)
    with pytest.raises(ValueError, match="dispatch directory"):
        token(main, main)


def test_capacity_reclaims_only_idle_expired_contexts(roots):
    main, a, b = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer, max_contexts=1)
    ca, cb = token(main, a), token(main, b)
    try:
        with router.acquire(ca):
            workspaces.revoke(main, ca)
            with pytest.raises(ProtocolError, match="capacity is full"):
                with router.acquire(cb):
                    pass
        with router.acquire(cb) as ctx:
            assert ctx.runtime_root == b
        assert len(router.contexts) == 1
    finally:
        router.shutdown()


def test_metadata_change_does_not_retarget_an_active_check(roots):
    main, a, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    cap = token(main, a)
    try:
        with router.acquire(cap):
            p = a / "runtime_metadata.json"
            data = json.loads(p.read_text()); data["changed"] = True
            p.write_text(json.dumps(data))
            with pytest.raises(ProtocolError, match="binding changed"):
                with router.acquire(cap):
                    pass
    finally:
        router.shutdown()


def test_capacity_recovery_is_host_issued_and_token_specific(roots, monkeypatch):
    from trellis.checker import admission, host_response
    main, private, _ = roots
    server = CheckerServer(main)
    server._job_service = object()
    monkeypatch.setattr(host_response, 'enrich', lambda *args: {
        'capacity_recovery': admission._authoritative_capacity_failure.get() is not None})
    ordinary = workspaces.register(main, private)['TRELLIS_CHECKER_TOKEN']
    recovery = workspaces.register(main, private, capacity_recovery=True)['TRELLIS_CHECKER_TOKEN']
    try:
        assert request(server, ordinary, 'host_response_enrichment')['response'] == {'capacity_recovery': False}
        assert request(server, recovery, 'host_response_enrichment')['response'] == {'capacity_recovery': True}
        # Context reuse cannot promote the other token, and wire metadata is
        # not a substitute for the host-owned registry grant.
        response = json.loads(server._dispatch_line(json.dumps(dict(op='host_response_enrichment',
            request_id=18, auth_token=ordinary, capacity_recovery=True)).encode()))
        assert response['response'] == {'capacity_recovery': False}
    finally:
        server._job_service = None
        server.shutdown()


def test_capacity_recovery_registration_rejects_non_boolean(roots):
    main, private, _ = roots
    with pytest.raises(ValueError, match='host boolean'):
        workspaces.register(main, private, capacity_recovery='worker')


def test_capture_liveness_bounds_full_reads_and_expires_with_request(roots, monkeypatch):
    main, task, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    cap = token(main, task)
    now = [10.0]
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: now[0])
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            calls = []
            original = router._entry
            def read(key):
                calls.append(key)
                return original(key)
            monkeypatch.setattr(router, '_entry', read)
            for _ in range(19312):
                live()
            assert len(calls) == 1
            now[0] += .099
            live()
            assert len(calls) == 1
            now[0] += .001
            live()
            assert len(calls) == 2
            live.force()
            assert len(calls) == 3
        with pytest.raises(ProtocolError):
            live()
        with router.acquire(cap, with_liveness=True) as (_, _, fresh):
            assert fresh is not live
            fresh.force()
    finally:
        router.shutdown()


@pytest.mark.parametrize('change', ['revoke', 'expiry', 'owner', 'metadata', 'recovery'])
def test_capture_liveness_detects_external_changes_at_bound(roots, monkeypatch, change):
    import hashlib
    main, task, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    cap = token(main, task)
    now = [10.0]
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: now[0])
    registry = workspaces.registry_path(main) / (hashlib.sha256(cap.encode()).hexdigest() + '.json')
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            live()
            if change == 'revoke':
                workspaces.revoke(main, cap)
            elif change == 'expiry':
                monkeypatch.setattr(workspaces.time, 'time', lambda: 1e18)
            elif change == 'owner':
                monkeypatch.setattr(workspaces, '_process_identity', lambda _: {'pid': -1})
            elif change == 'metadata':
                path = task / 'runtime_metadata.json'
                data = json.loads(path.read_text()); data['changed'] = True
                path.write_text(json.dumps(data))
            else:
                data = json.loads(registry.read_text()); data['capacity_recovery'] = True
                registry.write_text(json.dumps(data))
            now[0] += .099
            live()
            now[0] += .001
            with pytest.raises(ProtocolError):
                live()
    finally:
        router.shutdown()


def test_capture_release_is_immediate_token_specific_and_cannot_ack_active_request(roots, monkeypatch):
    main, task, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    first = token(main, task)
    second = workspaces.register(main, task, capacity_recovery=True)['TRELLIS_CHECKER_TOKEN']
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    try:
        with router.acquire(first, with_liveness=True) as (_, recovery1, a), \
                router.acquire(second, with_liveness=True) as (_, recovery2, b):
            assert not recovery1 and recovery2
            a(); b()
            assert router.release(first) == {'drained': False}
            with pytest.raises(ProtocolError):
                a()
            b()
            assert router.release(second) == {'drained': False}
            with pytest.raises(ProtocolError):
                b()
        assert router.release(second) == {'drained': True}
        assert not router.contexts
    finally:
        router.shutdown()


@pytest.mark.parametrize('materialized', [False, True])
def test_capture_response_forces_fresh_authentication_inside_poll_interval(roots, monkeypatch, materialized):
    from trellis.checker import admission, portable_replay
    main, task, _ = roots
    server = CheckerServer(main)
    cap = token(main, task)
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    def capture(*args, **kwargs):
        admission.check_cancelled()
        workspaces.revoke(main, cap)
        admission.check_cancelled()  # Cached poll is intentionally within100ms.
        return {'environment': {'must_not_escape': True}}
    monkeypatch.setattr(portable_replay, 'capture_current', capture)
    monkeypatch.setattr(portable_replay, 'materialized_environment', capture)
    try:
        response = json.loads(server._dispatch_line(json.dumps(dict(op='capture_replay_environment',
            request_id=1, node_names=['A'], auth_token=cap, materialized_only=materialized)).encode()))
        assert response['rpc_error']['kind'] == 'auth_required'
        assert 'environment' not in response
    finally:
        server.shutdown()


def test_capture_liveness_forces_authentication_after_admission_wait(roots, monkeypatch):
    from contextlib import contextmanager
    from trellis.checker import admission
    main, task, _ = roots
    cap = token(main, task)
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    scheduler = admission.Admission(1)
    original = scheduler.acquire
    @contextmanager
    def acquire(*args, **kwargs):
        with original(*args, **kwargs) as elapsed:
            workspaces.revoke(main, cap)
            yield elapsed
    monkeypatch.setattr(scheduler, 'acquire', acquire)
    monkeypatch.setattr(admission, 'ACTIVE', scheduler)
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    @admission.admitted
    def compiler():
        pytest.fail('launched after revocation during admission')
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            live()
            with admission.authoritative_workspace(task, cancelled=live):
                with pytest.raises(ProtocolError):
                    compiler()
        assert not scheduler.active and not scheduler.waiters
    finally:
        router.shutdown()


def test_capture_liveness_shared_by_parallel_leaves_keeps_release_visible(roots, monkeypatch):
    from trellis.checker import admission
    main, task, _ = roots
    cap = token(main, task)
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    monkeypatch.setattr(admission, 'ACTIVE', admission.Admission(3, readonly_slots=3))
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    barrier = threading.Barrier(3)
    def leaf(value):
        admission.check_cancelled()
        barrier.wait(timeout=2)
        if value == 0:
            assert router.release(cap) == {'drained': False}
        barrier.wait(timeout=2)
        with pytest.raises(ProtocolError):
            admission.check_cancelled()
        return value
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            with admission.authoritative_workspace(task, cancelled=live):
                assert admission.map_prepared('semantic', leaf, range(3)) == [0, 1, 2]
        assert router.release(cap) == {'drained': True}
    finally:
        router.shutdown()


def test_capture_liveness_forces_authentication_immediately_before_spawn(roots, monkeypatch):
    from pathlib import Path
    from trellis.atomic_actions import observations
    from trellis.checker import admission
    main, task, _ = roots
    cap = token(main, task)
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    monkeypatch.setattr(admission, 'ACTIVE', admission.Admission(1))
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    def prepare(_):
        workspaces.revoke(main, cap)
        return {}
    monkeypatch.setattr(observations, 'authoritative_env_for_repo', prepare)
    monkeypatch.setattr(observations, '_Wait4Popen', lambda *a, **kw: pytest.fail('revoked compiler launched'))
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            with admission.authoritative_workspace(task, cancelled=live):
                with pytest.raises(ProtocolError):
                    observations._run_lake_command(task.parent / 'repo', [], timeout_secs=1,
                                                   lake_executable=Path('/bin/true'))
        assert not admission.ACTIVE.active
    finally:
        router.shutdown()


def test_capture_release_cancels_real_hash_loop_inside_interval(roots, monkeypatch):
    from trellis.checker import admission, portable_replay
    main, task, _ = roots
    path = task.parent / 'repo' / 'artifact'
    path.write_bytes(b'x' * (3 << 20))
    cap = token(main, task)
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    original = admission.check_cancelled
    polls = []
    def poll(**kwargs):
        original(**kwargs)
        polls.append(True)
        if len(polls) == 2:
            assert router.release(cap) == {'drained': False}
    monkeypatch.setattr(admission, 'check_cancelled', poll)
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            with admission.authoritative_workspace(task, cancelled=live):
                budget = portable_replay.HashBudget()
                with pytest.raises(ProtocolError):
                    budget.file(path)
                assert budget.bytes <= 1 << 20
                assert not budget.memo
        assert router.release(cap) == {'drained': True}
    finally:
        router.shutdown()


def test_nested_capture_polling_keeps_force_boundaries_and_restores_strict_checks(roots, monkeypatch):
    from trellis.checker import admission
    main, task, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    cap = token(main, task)
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            calls = []
            original = router._entry
            def entry(key):
                calls.append(key)
                return original(key)
            monkeypatch.setattr(router, '_entry', entry)
            with admission.authoritative_workspace(task, cancelled=live.force, capture_cancelled=live):
                admission.check_cancelled(); admission.check_cancelled()
                assert len(calls) == 2
                with admission.capture_cancellation():
                    assert len(calls) == 3  # Forced entry despite recent strict check.
                    for _ in range(10000): admission.check_cancelled()
                    assert len(calls) == 3
                    admission.check_cancelled(force=True)
                    assert len(calls) == 4  # Admission/spawn remain synchronous.
                    with admission.capture_cancellation():
                        admission.check_cancelled()
                    assert len(calls) == 6
                assert len(calls) == 7  # Forced normal return.
                admission.check_cancelled(); admission.check_cancelled()
                assert len(calls) == 9  # All non-capture work remains strict.
    finally:
        router.shutdown()


@pytest.mark.parametrize('change', ['registry', 'owner', 'metadata', 'release'])
def test_nested_capture_cannot_return_after_binding_changes(roots, monkeypatch, change):
    from trellis.checker import admission
    main, task, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    cap = token(main, task)
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            with admission.authoritative_workspace(task, cancelled=live.force, capture_cancelled=live):
                with pytest.raises(ProtocolError):
                    with admission.capture_cancellation():
                        if change == 'registry': workspaces.revoke(main, cap)
                        elif change == 'owner': monkeypatch.setattr(workspaces, '_process_identity', lambda _: {'pid': -1})
                        elif change == 'metadata':
                            path = task / 'runtime_metadata.json'
                            data = json.loads(path.read_text()); data['changed'] = True
                            path.write_text(json.dumps(data))
                        else:
                            assert router.release(cap) == {'drained': False}
                            # Explicit release sets a request event immediately.
                            with pytest.raises(ProtocolError): admission.check_cancelled()
                        # Forced exit must refuse even without elapsed poll time.
                assert admission._authoritative_cancel.get() == live.force
    finally:
        router.shutdown()


def test_nested_capture_external_revocation_detected_within_existing_poll_bound(roots, monkeypatch):
    from trellis.checker import admission
    main, task, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    cap = token(main, task)
    now = [10.0]
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: now[0])
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            with admission.authoritative_workspace(task, cancelled=live.force, capture_cancelled=live):
                with pytest.raises(ProtocolError):
                    with admission.capture_cancellation():
                        workspaces.revoke(main, cap)
                        now[0] += .099
                        admission.check_cancelled()
                        now[0] += .001
                        admission.check_cancelled()
                        pytest.fail('revocation not checked at bound')
    finally:
        router.shutdown()


def test_capture_scope_restores_callback_on_error_and_absent_optional_callback():
    from trellis.checker import admission
    calls = []
    def strict(): calls.append('strict')
    class Bounded:
        def __call__(self): calls.append('bounded')
        def force(self): calls.append('force')
    with admission.authoritative_workspace('owned', cancelled=strict, capture_cancelled=Bounded()):
        with pytest.raises(ValueError, match='original'):
            with admission.capture_cancellation():
                admission.check_cancelled()
                raise ValueError('original')
        admission.check_cancelled()
    assert calls == ['force', 'bounded', 'strict']
    calls.clear()
    with admission.authoritative_workspace('owned', cancelled=strict):
        with admission.capture_cancellation(): admission.check_cancelled()
    assert calls == ['strict']


def test_actual_portable_capture_forces_entry_and_return_even_optional_failure(roots, monkeypatch):
    from trellis.atomic_actions import observations
    from trellis.checker import admission, portable_replay
    main, task, _ = roots
    router = workspaces.WorkspaceRouter(main, CheckerServer)
    cap = token(main, task)
    monkeypatch.setattr(workspaces.time, 'monotonic', lambda: 10.0)
    monkeypatch.setattr(observations, '_run_lake_command', lambda *a, **k: {'returncode': 1, 'stdout': ''})
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            calls = []
            original = router._entry
            def entry(key):
                calls.append(key)
                return original(key)
            monkeypatch.setattr(router, '_entry', entry)
            with admission.authoritative_workspace(task, cancelled=live.force, capture_cancelled=live):
                value, stats = portable_replay.capture(task.parent / 'repo', ['A'], timeout_secs=10, bwrap_role='lake_compiler')
                assert value is None and stats['status'] == 'unavailable'
                assert len(calls) == 2
                admission.check_cancelled()
                assert len(calls) == 3
    finally:
        router.shutdown()


def test_capture_entry_failure_keeps_original_callback():
    from trellis.checker import admission
    calls = []
    def strict(): calls.append('strict')
    class Refused:
        def __call__(self): pytest.fail('refused callback installed')
        def force(self): raise ValueError('entry refused')
    with admission.authoritative_workspace('owned', cancelled=strict, capture_cancelled=Refused()):
        with pytest.raises(ValueError, match='entry refused'):
            with admission.capture_cancellation():
                pytest.fail('entered refused capture')
        assert admission._authoritative_cancel.get() is strict
        admission.check_cancelled()
    assert calls == ['strict']


def test_nested_workspace_capture_restores_distinct_outer_callbacks():
    from trellis.checker import admission
    calls = []
    class Callback:
        def __init__(self, name): self.name = name
        def __call__(self): calls.append(self.name)
        def force(self): calls.append(self.name + ':force')
    outer, inner = Callback('outer'), Callback('inner')
    with admission.authoritative_workspace('outer', cancelled=outer.force, capture_cancelled=outer):
        with admission.capture_cancellation():
            admission.check_cancelled()
            with admission.authoritative_workspace('inner', cancelled=inner.force, capture_cancelled=inner):
                with admission.capture_cancellation(): admission.check_cancelled()
                assert admission._authoritative_cancel.get() == inner.force
            assert admission._authoritative_cancel.get() is outer
            assert admission._capture_cancel.get() is outer
            admission.check_cancelled()
        assert admission._authoritative_cancel.get() == outer.force
        admission.check_cancelled()
    assert calls == ['outer:force', 'outer', 'inner:force', 'inner',
                     'inner:force', 'outer', 'outer:force', 'outer:force']

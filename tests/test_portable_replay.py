import json
import os
from pathlib import Path
import shutil
import subprocess
from types import SimpleNamespace

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import portable_replay as portable
from trellis.dispatch import replay_donation as donation
from trellis.dispatch import candidate
from trellis.dispatch.publication import atomic


@pytest.fixture(autouse=True)
def cold_package_cache(monkeypatch):
    # This suite injects read-time races and checks exact cold I/O accounting.
    # Keep each observation cold; warm/coalesced capture behavior has its own
    # mutation and exact-environment regression suite.
    monkeypatch.setattr(portable, '_PROTECTED_DIGESTS', portable._ProtectedDigestCache(capacity=0))


def put(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data if isinstance(data, bytes) else candidate.canonical_json(data))


@pytest.fixture
def imported(tmp_path, monkeypatch):
    sysroot = tmp_path / 'toolchain'
    for tool in ('lean', 'lake', 'leanchecker'):
        put(sysroot / 'bin' / tool, tool.encode())
        (sysroot / 'bin' / tool).chmod(0o755)
    put(sysroot / 'lib/lean/Init.olean', b'pinned standard library')
    def setup(repo):
        for name, source in {'Preamble': 'import External\n', 'A': 'import Tablet.Preamble\n'}.items():
            put(repo / f'Tablet/{name}.lean', source.encode())
            put(repo / f'.lake/build/lib/lean/Tablet/{name}.olean', ('artifact ' + name).encode())
        put(repo / '.lake/packages/dependency/.lake/build/lib/lean/External.olean', b'external artifact')
        put(repo / 'lean-toolchain', b'leanprover/lean4:v4.33.0\n')
        put(repo / '.trellis/scripts/check.py', b'# pinned source closure checker\n')
        put(repo / 'lake-manifest.json', {'packagesDir': '.lake/packages', 'packages': []})
        return repo
    def inventory(repo, args, **kwargs):
        assert args[:2] == ['env', 'lean'] and '--replay-environment' in args
        roots = [repo / '.lake/build/lib/lean', repo / '.lake/packages/dependency/.lake/build/lib/lean', sysroot / 'lib/lean']
        paths = {'Tablet.A': roots[0] / 'Tablet/A.olean', 'Tablet.Preamble': roots[0] / 'Tablet/Preamble.olean',
                 'External': roots[1] / 'External.olean', 'Init': roots[2] / 'Init.olean'}
        dependencies = {'Tablet.A': ['Tablet.Preamble'], 'Tablet.Preamble': ['External'], 'External': ['Init']}
        rows = [dict(environment_header=True, sysroot=str(sysroot), path=str(sysroot / 'bin'), search_path=list(map(str, roots)))]
        for name, path in paths.items():
            if name == 'Init':
                rows.append(dict(module=name, sysroot_import=str(path)))
            else:
                parts = [dict(level='exported', path=str(path))]
                for level in ('server', 'private'):
                    if Path(str(path) + '.' + level).exists():
                        parts.append(dict(level=level, path=str(path) + '.' + level))
                rows.append(dict(module=name, parts=parts,
                    imports=[dict(module=dep, olean=str(paths[dep]), import_all=False,
                                  is_exported=True, is_meta=False) for dep in dependencies[name]]))
        return dict(returncode=0, stdout='\n'.join(json.dumps(row) for row in rows), stderr='')
    monkeypatch.setattr(obs, '_run_lake_command', inventory)
    return setup, sysroot


def capture(repo, **kwargs):
    environment, stats = portable.capture(repo, ['A', 'Preamble'], timeout_secs=60, bwrap_role='lake_compiler', **kwargs)
    assert environment is not None, stats
    return environment, stats


def test_import_binding_is_portable_but_detects_transitive_package_and_split_changes(tmp_path, imported):
    setup, _ = imported
    old, new = setup(tmp_path / 'old'), setup(tmp_path / 'new')
    before, _ = capture(old)
    after, _ = capture(new)
    assert before == after  # Different absolute private workspace locations.
    imported_path = new / '.lake/packages/dependency/.lake/build/lib/lean/External.olean'
    times = imported_path.stat()
    imported_path.write_bytes(b'changed external!')
    os.utime(imported_path, ns=(times.st_atime_ns, times.st_mtime_ns))
    changed, _ = capture(new)
    assert changed != before
    shutil.copy2(old / imported_path.relative_to(new), imported_path)
    Path(str(imported_path) + '.server').write_bytes(b'new private visibility')
    changed, _ = capture(new)
    assert changed != before


def test_resolution_absences_are_bound_and_shadowing_is_cold(tmp_path, imported):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    before, _ = capture(repo)
    assert before['files']['repo/.lake/build/lib/lean/External.olean'] is None
    put(repo / '.lake/build/lib/lean/External.olean', b'shadow the package')
    value, stats = portable.capture(repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler')
    assert value is None and 'resolver differs' in stats['reason']


def test_bounded_workspace_liveness_preserves_capture_bytes_and_absences(tmp_path, imported):
    from trellis.checker import admission, workspaces
    setup, _ = imported
    main = tmp_path / 'runtime'
    task = main / 'dispatch/1/runtime'
    repo = setup(task.parent / 'repo')
    put(task / 'runtime_metadata.json', {'repo_path': str(repo)})
    cap = workspaces.register(main, task)['TRELLIS_CHECKER_TOKEN']
    router = workspaces.WorkspaceRouter(main, lambda _: SimpleNamespace(shutdown=lambda **kw: None))
    try:
        with router.acquire(cap, with_liveness=True) as (_, _, live):
            with admission.authoritative_workspace(task, cancelled=live.force):
                before, _ = capture(repo)
            with admission.authoritative_workspace(task, cancelled=live):
                after, _ = capture(repo)
                live.force()
                assert after == before
                assert portable.digest(after) == portable.digest(before)
                put(repo / '.lake/build/lib/lean/External.olean', b'shadow the package')
                value, stats = portable.capture(repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler')
                assert value is None and 'resolver differs' in stats['reason']
    finally:
        router.shutdown()


def test_import_lookup_alias_cannot_escape_later_native_file_recheck(tmp_path, imported):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    package = repo / '.lake/packages/dependency/.lake/build/lib/lean/External.olean'
    alias = repo / '.lake/build/lib/lean/External.olean'
    alias.symlink_to(package)
    value, stats = portable.capture(repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler')
    assert value is None and 'alias is not portable' in stats['reason']


def test_hash_budget_streams_and_memo_lives_only_in_one_operation(tmp_path):
    path = tmp_path / 'large'
    data = b'x' * ((2 << 20) + 17)
    path.write_bytes(data)
    budget = portable.HashBudget(max_bytes=len(data))
    first = budget.file(path)
    assert budget.file(path) == first and budget.bytes == len(data) and budget.files == 1
    with pytest.raises(portable.Unavailable):
        portable.HashBudget(max_bytes=len(data) - 1).file(path)
    stamp = path.stat()
    path.write_bytes(b'y' * len(data)); os.utime(path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    assert portable.HashBudget().file(path) != first
    with pytest.raises(portable.Unavailable):
        budget.file(path)  # Changed bytes cannot borrow the old within-pass memo.


@pytest.mark.parametrize('supplied_budget', [False, True])
@pytest.mark.parametrize('slow_hash', [False, True])
def test_capture_admission_wait_does_not_spend_hash_budget(
    tmp_path, imported, monkeypatch, supplied_budget, slow_hash,
):
    from contextlib import contextmanager
    from trellis.checker import admission
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    clock = [0.0]
    leased = [False]
    monkeypatch.setattr(portable.time, 'monotonic', lambda: clock[0])
    @contextmanager
    def delayed_lease(*, exclusive=False):
        assert not exclusive
        clock[0] += 600.0  # Legitimate queue exceeds the whole hash allowance.
        leased[0] = True
        try:
            yield 600_000.0
        finally:
            leased[0] = False
    monkeypatch.setattr(admission, 'authoritative_lease', delayed_lease)
    inventory = obs._run_lake_command
    @admission.admitted
    def admitted_inventory(*args, **kwargs):
        assert leased[0] and kwargs['timeout_secs'] == 60
        clock[0] += 1.0
        return inventory(*args, **kwargs)
    monkeypatch.setattr(obs, '_run_lake_command', admitted_inventory)
    original_file = portable.HashBudget.file
    def observed_file(self, path, **kwargs):
        assert not leased[0]  # Hashing never occupies a compiler lease.
        if slow_hash:
            clock[0] += 61.0
        return original_file(self, path, **kwargs)
    monkeypatch.setattr(portable.HashBudget, 'file', observed_file)
    budget = portable.HashBudget(seconds=60) if supplied_budget else None
    environment, stats = portable.capture(repo, ['A'], timeout_secs=60,
        bwrap_role='lake_compiler', budget=budget)
    assert stats['inventory_wall_secs_including_queue'] == 601.0
    if slow_hash:
        assert environment is None and 'deadline exhausted' in stats['reason']
        assert stats['hash_wall_secs'] == 61.0
    else:
        assert environment is not None and stats['bytes_hashed'] > 0
        assert stats['hash_wall_secs'] == 0.0 and stats['wall_secs'] == 601.0


@pytest.mark.parametrize('jobs_enabled', [False, True])
def test_capture_client_waits_for_queue_and_hashing_once(monkeypatch, jobs_enabled):
    from trellis.atomic_actions import checker_client as client
    monkeypatch.delenv('TRELLIS_CHECK_JOBS_SOCKET', raising=False)
    monkeypatch.setenv('TRELLIS_CHECK_JOBS', '1' if jobs_enabled else '0')
    monkeypatch.delenv('TRELLIS_CLOSURE_IDENTITY_ONLY', raising=False)
    monkeypatch.setattr(client, '_connect', lambda *_a, **_k: SimpleNamespace(close=lambda: None))
    deadlines = []
    monkeypatch.setattr(client, '_send_request', lambda _s, _r, **kw: deadlines.append(kw['deadline_secs']))
    def read(_s, **kw):
        deadlines.append(kw['deadline_secs'])
        return b'{"request_id":1,"environment":null}\n'
    monkeypatch.setattr(client, '_read_one_line', read)
    response = client._round_trip(Path('/unused'), dict(op='capture_replay_environment',
        request_id=1, node_names=['A'], timeout_secs=180), timeout_secs=180)
    assert response['environment'] is None
    assert deadlines == [3965.0, 3965.0]


@pytest.fixture
def donor(tmp_path, imported):
    setup, _ = imported
    attempts = tmp_path / 'dispatch/1/acceptance'
    old, new = attempts / '1', attempts / '2'
    source = setup(old / 'repo/.trellis/supervisor/repo')
    destination = setup(new / 'repo/.trellis/supervisor/repo')
    for node in ('A', 'Preamble'):
        obs.write_olean_srcclosure(source, node, obs.tablet_source_closure_hash(source, node))
        assert obs._write_kernel_replay_attestation(source, node, olean_sha256='')
    environment, _ = capture(source)
    portable.attach(source, ['A', 'Preamble'], environment)
    completion = {'files': {}, 'key': {'task_id': 1}}
    put(old / 'completed.json', completion)
    current_key = dict(task_id=1, acceptance_sha256='new-context')
    put(new / 'canonical.json', dict(key=current_key, config_sha256='c' * 64))
    put(new / 'replay-cache-donor.json', dict(version=1, donor_round='1',
        donor_completion_sha256=candidate.digest(candidate.canonical_json(completion)),
        current_key_sha256=candidate.digest(candidate.canonical_json(current_key))))
    invalidated = []
    workspace = SimpleNamespace(runtime_root=new / 'runtime', supervisor_repo=destination,
        _invalidate_compile_cache_for_sync=lambda names: invalidated.extend(names))
    return source, destination, workspace, invalidated


def test_completed_host_artifacts_cross_rounds_without_replay_and_keep_exact_bindings(donor):
    source, destination, workspace, invalidated = donor
    result = donation.install(workspace)
    assert result['status'] == 'installed', result
    assert result['covered_nodes'] == ['A', 'Preamble']
    assert invalidated == ['Preamble.lean', 'A.lean'] or sorted(invalidated) == ['A.lean', 'Preamble.lean']
    assert all(obs.olean_has_current_kernel_replay(destination, node) for node in result['copied_nodes'])
    assert all(obs.olean_is_content_current(destination, node) for node in result['copied_nodes'])
    for node in result['copied_nodes']:
        assert obs._tablet_olean_path(source, node).stat().st_ino != obs._tablet_olean_path(destination, node).stat().st_ino
    assert result['artifact_files'][str(destination / '.lake/build/lib/lean/External.olean')] is None
    # Labels are rebound to the fresh authority; bytes remain the authority.
    provenance = obs._read_olean_provenance(destination, 'A')['kernel_replay']
    assert all(str(source) not in row['olean'] for row in provenance['trusted_direct_imports'])


@pytest.fixture
def overlapping_replay_dependencies(donor):
    source, _, _, _ = donor
    previous = 'A'
    roots = ['A', 'Preamble']
    for index in range(6):
        node = f'Chain{index}'
        put(source / f'Tablet/{node}.lean', f'import Tablet.{previous}\n'.encode())
        put(obs._tablet_olean_path(source, node), f'artifact {node}'.encode())
        obs.write_olean_srcclosure(source, node, obs.tablet_source_closure_hash(source, node))
        assert obs._write_kernel_replay_attestation(source, node, olean_sha256='')
        roots.append(node)
        previous = node
    return source, obs.SourceClosureSnapshot(source), roots


def test_shared_dependency_validation_matches_scalar_and_reduces_reads(overlapping_replay_dependencies, monkeypatch):
    source, snapshot, roots = overlapping_replay_dependencies
    scalar = {node for node in roots if all(
        obs.olean_is_content_current(source, dep, source_snapshot=snapshot)
        and obs.olean_has_current_kernel_replay(source, dep)
        for dep in snapshot.materialization_order([node]))}
    calls = []
    original = obs.olean_has_current_kernel_replay
    def observed(repo, node, **kwargs):
        calls.append(node)
        return original(repo, node, **kwargs)
    monkeypatch.setattr(obs, 'olean_has_current_kernel_replay', observed)
    covered, stats = donation._dependency_coverage(source, snapshot, roots)
    assert covered == scalar == set(roots)
    assert stats == dict(mode='shared_dependencies', references=36, unique=8, validations=16)
    assert len(calls) == 16 and all(calls.count(node) == 2 for node in roots)
    # A later operation reads bytes again; success never persists across calls.
    obs._tablet_olean_path(source, 'Preamble').write_bytes(b'changed')
    assert donation._dependency_coverage(source, snapshot, roots)[0] == set()


@pytest.mark.parametrize('damage', ['bytes', 'optional_part', 'replay_policy'])
def test_shared_dependency_second_pass_detects_drift(overlapping_replay_dependencies, monkeypatch, damage):
    source, snapshot, roots = overlapping_replay_dependencies
    original = obs.olean_has_current_kernel_replay
    calls = 0
    def observed(repo, node, **kwargs):
        nonlocal calls
        calls += 1
        if calls == len(roots) + 1:
            artifact = obs._tablet_olean_path(source, 'Preamble')
            if damage == 'bytes':
                artifact.write_bytes(b'changed')
            elif damage == 'optional_part':
                Path(str(artifact) + '.server').write_bytes(b'new optional part')
            else:
                record = obs._read_olean_provenance(source, 'Preamble')
                record['kernel_replay']['trusted_import_policy_version'] = -1
                assert obs._write_olean_provenance(source, 'Preamble', record)
        return original(repo, node, **kwargs)
    monkeypatch.setattr(obs, 'olean_has_current_kernel_replay', observed)
    assert donation._dependency_coverage(source, snapshot, roots)[0] == set()


def test_shared_dependency_oversize_uses_existing_scalar_checks(overlapping_replay_dependencies, monkeypatch):
    source, snapshot, roots = overlapping_replay_dependencies
    monkeypatch.setattr(donation, 'MAX_OWNERS', 2)
    covered, stats = donation._dependency_coverage(source, snapshot, roots)
    assert covered == set(roots)
    assert stats == dict(mode='scalar', validations=36)


def test_shared_dependency_common_failure_preserves_early_exit(overlapping_replay_dependencies):
    source, snapshot, roots = overlapping_replay_dependencies
    obs._tablet_olean_path(source, 'Preamble').unlink()
    covered, stats = donation._dependency_coverage(source, snapshot, roots)
    assert covered == set()
    assert stats == dict(mode='shared_dependencies', references=36, unique=8, validations=1)


def test_donation_source_drift_during_dependency_validation_is_cold_and_purged(donor, monkeypatch):
    source, destination, workspace, _ = donor
    validate = donation._dependency_coverage
    def changing(authority, snapshot, roots):
        result = validate(authority, snapshot, roots)
        put(source / 'Tablet/Preamble.lean', b'import External\n-- changed during coverage\n')
        return result
    monkeypatch.setattr(donation, '_dependency_coverage', changing)
    result = donation.install(workspace)
    assert result['status'] == 'cold'
    assert result['copied_nodes'] == result['covered_nodes'] == []
    assert result['reason'] == 'source changed during dependency validation'
    for node in ('A', 'Preamble'):
        assert not obs._tablet_olean_path(destination, node).exists()
        assert not obs.olean_has_current_kernel_replay(destination, node)


@pytest.mark.parametrize('change', ['none', 'missing', 'wrong_digest', 'symlink', 'imported_bytes'])
def test_seeded_dependency_recovers_exact_manifest_from_canonical_authority(donor, tmp_path, change):
    source, destination, workspace, _ = donor
    identity = obs._read_olean_provenance(source, 'Preamble')['kernel_replay']['portable_environment_sha256']
    original = source / portable.DIRECTORY / (identity + '.json')
    data = original.read_bytes()
    canonical = tmp_path / 'canonical/.trellis/supervisor/repo'
    recovered = canonical / portable.DIRECTORY / original.name
    put(recovered, data)
    original.unlink()  # Reproduce older build seeding: provenance, no manifest.
    if change == 'missing':
        recovered.unlink()
    elif change == 'wrong_digest':
        put(recovered, dict(version=portable.VERSION, different=True))
    elif change == 'symlink':
        other = tmp_path / 'other.json'
        recovered.rename(other)
        recovered.symlink_to(other)
    elif change == 'imported_bytes':
        (destination / '.lake/packages/dependency/.lake/build/lib/lean/External.olean').write_bytes(b'changed import')
    result = donation.install(workspace, environment_source=canonical)
    assert not original.exists()  # Never repair or rewrite the old donor.
    if change == 'none':
        assert result['status'] == 'installed', result
        assert result['covered_nodes'] == ['A', 'Preamble']
        assert result['canonical_environment_manifests'] == [identity]
        assert recovered.read_bytes() == data
        assert obs.olean_has_current_kernel_replay(destination, 'A')
    else:
        assert result['status'] == 'cold', result
        assert result['covered_nodes'] == []
        assert not obs.olean_has_current_kernel_replay(destination, 'A')


def test_staged_artifact_roots_copy_only_their_import_closure(donor, monkeypatch):
    source, destination, workspace, invalidated = donor
    path = workspace.runtime_root.parent / 'replay-cache-donor.json'
    marker = json.loads(path.read_text())
    marker['owner_nodes'] = ['Preamble']
    put(path, marker)
    original = obs._read_olean_provenance
    def read(repo, node):
        if Path(repo) == source and node == 'A':
            pytest.fail('unrelated owned artifact must not be examined or transferred')
        return original(repo, node)
    monkeypatch.setattr(obs, '_read_olean_provenance', read)
    result = donation.install(workspace)
    assert result['status'] == 'installed', result
    assert result['covered_nodes'] == ['Preamble']
    assert invalidated == ['Preamble.lean']
    assert obs.olean_has_current_kernel_replay(destination, 'Preamble')


@pytest.mark.parametrize('damage', ['legacy', 'source', 'artifact', 'package', 'completion', 'hardlink', 'symlink', 'copy_budget'])
def test_damaged_or_unsupported_donation_cannot_install_a_replay_capability(donor, monkeypatch, damage):
    source, destination, workspace, _ = donor
    if damage == 'legacy':
        for node in ('A', 'Preamble'):
            record = obs._read_olean_provenance(source, node)
            record['kernel_replay'].pop('portable_environment_sha256')
            obs._write_olean_provenance(source, node, record)
    elif damage == 'source':
        (destination / 'Tablet/Preamble.lean').write_text('changed source')
    elif damage == 'artifact':
        obs._tablet_olean_path(source, 'A').write_bytes(b'forged artifact')
    elif damage == 'package':
        (destination / '.lake/packages/dependency/.lake/build/lib/lean/External.olean').write_bytes(b'changed package')
    elif damage == 'completion':
        (workspace.runtime_root.parent.parent / '1/completed.json').write_text('{}')
    elif damage in ('symlink', 'hardlink'):
        path = obs._tablet_olean_path(source, 'Preamble')
        backup = path.with_suffix('.original'); path.rename(backup)
        if damage == 'symlink': path.symlink_to(backup)
        else: path.hardlink_to(backup)
    else:
        monkeypatch.setattr(donation, 'MAX_COPY_BYTES', 1)
    result = donation.install(workspace)
    assert 'A' not in result['covered_nodes'], result
    if damage != 'artifact':
        assert not result['covered_nodes'], result
    assert not obs.olean_has_current_kernel_replay(destination, 'A')


def test_non_host_workspace_and_worker_marker_cannot_choose_a_donor(tmp_path):
    put(tmp_path / 'worker/runtime/replay-cache-donor.json', dict(donor_round='1'))
    with pytest.raises(portable.Unavailable):
        donation._marker(tmp_path / 'worker/runtime')


@pytest.mark.parametrize('relocated_adapter', [False, True])
def test_portable_publication_preserves_replay_and_falls_back_on_environment_change(donor, tmp_path, monkeypatch, relocated_adapter):
    from trellis.dispatch import publication_artifacts as publication
    source, destination, _workspace, _ = donor
    if relocated_adapter:
        identity = obs._read_olean_provenance(source, 'A')['kernel_replay']['portable_environment_sha256']
        original = portable.environment_for(source, identity)
        historical = historical_adapter_environment(original,
            'external//old-deployment/scripts/lean_module_manifest.lean')
        portable.attach(source, ['A', 'Preamble'], historical)
    response = dict(local_closure_revalidation=dict(refreshed=[
        [node, dict(node_certificate=dict(artifact_bundle=obs._artifact_bundle(source, node)))]
        for node in ('A', 'Preamble')]))
    staged, canonical = source.parents[2], destination.parents[2]
    journal = tmp_path / 'publication'
    handoff = publication.stage(staged, canonical, journal, response)
    assert handoff['portable_environments']
    publication.validate(journal, handoff)
    monkeypatch.setattr('trellis.supervisor_workspace.sync_supervisor_workspace',
        lambda _: dict(authoritative_repo_path=str(destination)))
    cold = []
    def replay(repo, nodes, **kwargs):
        cold.append([node for node in nodes if not obs.olean_has_current_kernel_replay(repo, node)])
        return dict(returncode=0)
    monkeypatch.setattr(obs, '_kernel_replay_tablet_modules', replay)
    publication.install(canonical, journal, handoff, lambda _: None)
    assert cold == [[]]
    assert all(obs.olean_has_current_kernel_replay(destination, node) for node in ('A', 'Preamble'))
    (destination / '.lake/packages/dependency/.lake/build/lib/lean/External.olean').write_bytes(b'changed package')
    publication.install(canonical, journal, handoff, lambda _: None)
    assert set(cold[-1]) == {'A', 'Preamble'}


def test_replay_seed_wire_accepts_no_paths_or_worker_selected_sources():
    from trellis.checker.protocol import validate_request, ProtocolError
    for field in ('repo_path', 'donor', 'source', 'command', 'nodes'):
        with pytest.raises(ProtocolError):
            validate_request(dict(op='seed_acceptance_replay', request_id=1, **{field: '/untrusted'}))


def test_config_context_uses_exact_prepared_carrier_not_worker_payload(tmp_path):
    runtime = tmp_path / 'acceptance/2/runtime'
    put(runtime.parent / 'canonical.json', dict(config_sha256='canonical'))
    put(runtime.parent / 'prepared.json', dict(prepared_config_sha256='prepared'))
    assert donation.validation_fact_context(runtime) == dict(canonical_config_sha256='canonical', prepared_config_sha256='prepared')


def test_supervisor_marker_context_has_fixed_host_paths_and_exact_bytes(tmp_path):
    runtime = tmp_path / 'acceptance/2/runtime'
    put(runtime.parent / 'canonical.json', dict(config_sha256='canonical', source={'input_hashes': {}}))
    put(runtime.parent / 'prepared.json', dict(prepared_config_sha256='prepared'))
    worker = runtime.parent / 'repo'
    supervisor = worker / '.trellis/supervisor'
    marker = supervisor / 'repo/.trellis-supervisor.json'
    expected = dict(worker_repo_path=str(worker), supervisor_repo_path=str(supervisor / 'repo'),
                    supervisor_home=str(supervisor / 'home'), supervisor_cache=str(supervisor / 'cache'))
    put(marker, expected)
    binding = donation.validation_fact_context(runtime)['supervisor_marker']
    assert binding == dict(canonical_sha256=None, prepared_sha256=candidate.digest(marker.read_bytes()), expected=expected)
    put(marker, dict(expected, supervisor_home='/some/other/worker'))
    assert donation.validation_fact_context(runtime) is None
    put(marker, dict(expected, extra='unrecognized role'))
    assert donation.validation_fact_context(runtime) is None


def test_fact_donation_uses_authenticated_native_roots_not_all_materialized_or_worker_claims(donor):
    _source, destination, workspace, _ = donor
    previous, marker, completed = donation._marker(workspace.runtime_root)
    response_path = previous / 'execution/response.json'
    def authorize(owners):
        response = dict(validation_fact_receipt=dict(schema_version=3, artifact_owners=owners), validation_step_results=[dict(ok=True)])
        put(response_path, response)
        completed['files']['execution/response.json'] = candidate.digest(response_path.read_bytes())
        put(previous / 'completed.json', completed)
        marker['donor_completion_sha256'] = candidate.digest((previous / 'completed.json').read_bytes())
        put(workspace.runtime_root.parent / 'replay-cache-donor.json', marker)
        return dict(version=1, status='installed', covered_nodes=['A'], artifact_files={},
                    donor_round='1', donor_completion_sha256=marker['donor_completion_sha256'])
    # Unrelated materialized legacy owner does not expand the actual plan reads.
    put(destination / '.lake/build/lib/lean/Tablet/Unrelated.olean', b'old canonical owner')
    witness = authorize(['A'])
    assert donation.validation_fact_donor(workspace.runtime_root, witness) is not None
    # Approved correspondence target is an independent mandatory observation.
    witness = authorize(['A', 'Downstream'])
    assert donation.validation_fact_donor(workspace.runtime_root, witness) is None
    witness = authorize(['A'])
    response_path.write_text('{"validation_fact_receipt":{"artifact_owners":[]}}')
    assert donation.validation_fact_donor(workspace.runtime_root, witness) is None


def test_read_only_environment_capture_never_syncs_builds_or_mints_provenance(donor, monkeypatch):
    from contextlib import contextmanager
    _source, destination, workspace, _ = donor
    held = []
    @contextmanager
    def lock(name):
        held.append(name)
        try: yield
        finally: assert held.pop() == name
    workspace._workspace_lock = lock('workspace')
    workspace._sync_lock = lock('sync')
    original = portable.capture
    def capture_locked(*args, **kwargs):
        assert held == ['workspace', 'sync']
        return original(*args, **kwargs)
    monkeypatch.setattr(portable, 'capture', capture_locked)
    monkeypatch.setattr(obs, '_write_olean_provenance', lambda *_: pytest.fail('read-only capture cannot attest'))
    monkeypatch.setattr(obs, '_kernel_replay_tablet_modules', lambda *_a, **_k: pytest.fail('read-only capture cannot replay'))
    monkeypatch.setattr('trellis.supervisor_workspace.sync_supervisor_workspace',
        lambda *_: pytest.fail('read-only capture cannot sync retained sources'))
    before = {str(path.relative_to(destination)): path.read_bytes() for path in destination.rglob('*') if path.is_file()}
    result = portable.capture_current(workspace, ['A'], timeout_secs=180)
    assert set(result['environment']['environment_sha256s']) == {'A'}
    assert not (destination / portable.DIRECTORY).exists()
    assert before == {str(path.relative_to(destination)): path.read_bytes() for path in destination.rglob('*') if path.is_file()}


def test_environment_capture_wire_is_bounded_and_accepts_no_path_selection():
    from trellis.checker.protocol import validate_request, ProtocolError
    assert validate_request(dict(op='capture_replay_environment', request_id=1, node_names=['A'])).nodes == ('A',)
    for extra in ({'repo_path': '/untrusted'}, {'nodes': ['A']}, {'command': 'touch'}, {'source': '/elsewhere'}):
        with pytest.raises(ProtocolError):
            validate_request(dict(op='capture_replay_environment', request_id=1, node_names=['A'], **extra))
    with pytest.raises(ProtocolError):
        validate_request(dict(op='capture_replay_environment', request_id=1, node_names=['../A']))


def test_environment_capture_cli_uses_scoped_socket_only(tmp_path, monkeypatch, capsys):
    from trellis.atomic_actions import cli, checker_client
    calls = []
    monkeypatch.setattr(obs, '_resolve_socket_path', lambda: '/host/checker.sock')
    def round_trip(socket, request, **kwargs):
        calls.append((socket, request, kwargs))
        return dict(request_id=request['request_id'], environment={'version': 1})
    monkeypatch.setattr(checker_client, '_round_trip', round_trip)
    monkeypatch.setattr(obs, '_run_lake_command', lambda *_a, **_k: pytest.fail('no direct compiler fallback'))
    assert cli.main(['capture-replay-environment', str(tmp_path), '["A"]']) == 0
    assert json.loads(capsys.readouterr().out) == {'environment': {'version': 1}}
    assert len(calls) == 1 and calls[0][0] == '/host/checker.sock'
    assert calls[0][1]['node_names'] == ['A']
    assert 'repo_path' not in calls[0][1] and calls[0][2] == {'timeout_secs': 180.0}
    for invalid in ('not json', '"A"', '["../A"]'):
        assert cli.main(['capture-replay-environment', str(tmp_path), invalid]) == 0
        assert json.loads(capsys.readouterr().out)['environment'] is None
    assert len(calls) == 1
    monkeypatch.setattr(obs, '_resolve_socket_path', lambda: None)
    assert obs.capture_replay_environment(tmp_path, ['A'])['environment'] is None
    assert len(calls) == 1
    monkeypatch.setattr(obs, '_resolve_socket_path', lambda: '/host/checker.sock')
    def failed(*_a, **_k):
        raise checker_client.CheckerRpcError('auth_required', 'expired capability')
    monkeypatch.setattr(checker_client, '_round_trip', failed)
    assert obs.capture_replay_environment(tmp_path, ['A']) == {
        'environment': None, 'cold_reason': 'auth_required: expired capability'}


def test_environment_capture_reopens_reaped_workspace_with_new_host_capability(tmp_path, monkeypatch):
    from trellis.checker.server import CheckerServer
    from trellis.checker.workspaces import register, revoke
    from trellis.runtime.bridge import _register_burst_token
    root = tmp_path / 'runtime'
    repo = tmp_path / 'repo'
    (repo / '.trellis').mkdir(parents=True)
    put(root / 'runtime_metadata.json', dict(repo_path=str(repo)))
    task = root / 'dispatch/1/acceptance/1'
    (task / 'repo/.trellis').mkdir(parents=True)
    put(task / 'runtime/runtime_metadata.json', dict(repo_path=str(task / 'repo')))
    server = CheckerServer(root)
    logs = []
    monkeypatch.setattr(server._request_log, 'emit', lambda **row: logs.append(row))
    _register_burst_token(root, token='host', burst_id='capture', kind='supervisor', request_id=0, cycle=0)
    _register_burst_token(root, token='worker', burst_id='worker', kind='worker', request_id=0, cycle=0)
    server._reload_burst_tokens_if_changed()
    observed = []
    def current(workspace, nodes, **kwargs):
        observed.append(workspace.runtime_root)
        return dict(environment=None, cold_reason='fixture has no artifacts')
    monkeypatch.setattr(portable, 'capture_current', current)
    def request(token):
        return json.loads(server._dispatch_line(candidate.canonical_json(dict(op='capture_replay_environment',
            request_id=1, node_names=['A'], auth_token=token))))
    old = register(root, task / 'runtime', lifetime=None)['TRELLIS_CHECKER_TOKEN']
    revoke(root, old)
    assert request(old)['rpc_error']['kind'] == 'auth_required'
    new = register(root, task / 'runtime', lifetime=None)['TRELLIS_CHECKER_TOKEN']
    try:
        assert request(new)['environment'] is None
        assert observed == [task / 'runtime']
        assert request('host')['environment'] is None
        assert observed[-1] == root
        assert request('worker')['rpc_error']['kind'] == 'auth_required'
        assert len(logs) == 2
        assert all(row['op'] == 'capture_replay_environment' and not row['environment_available'] for row in logs)
        assert all(row['finished_ns'] >= row['started_ns'] for row in logs)
        assert all('environment' not in row and 'artifact_files' not in row for row in logs)
    finally:
        server._task_workspaces.release(new)


def test_real_module_graph_and_replay_cross_round_directories(tmp_path, monkeypatch):
    from trellis.host_runtime import worker_elan_home
    toolchain = worker_elan_home() / 'toolchains/leanprover--lean4---v4.33.0/bin'
    if not (toolchain / 'lean').is_file():
        pytest.skip('requires installed Lean 4.33 toolchain')
    attempts = tmp_path / 'dispatch/1/acceptance'
    old, new = attempts / '1', attempts / '2'
    source, destination = [p / 'repo/.trellis/supervisor/repo' for p in (old, new)]
    put(source / 'lakefile.toml', b'name = "portable_test"\nversion = "0.1.0"\n[[lean_lib]]\nname = "Tablet"\n')
    put(source / 'lean-toolchain', b'leanprover/lean4:v4.33.0\n')
    put(source / 'Tablet/Preamble.lean', b'import Init\ndef portableZero : Nat := 0\n')
    put(source / 'Tablet/A.lean', b'import Tablet.Preamble\ntheorem A : portableZero = 0 := by sorry\n')
    put(source / '.trellis/scripts/check.py', b'# fixture checker source binding\n')
    env = dict(os.environ, PATH=str(toolchain) + os.pathsep + os.environ['PATH'], LEAN_NUM_THREADS='1')
    env.pop('LEAN_PATH', None)
    built = subprocess.run([str(toolchain / 'lake'), 'build', 'Tablet.A'], cwd=source,
        env=env, text=True, capture_output=True, timeout=90)
    assert built.returncode == 0, built.stdout + built.stderr
    calls = []
    def run(repo, args, **kwargs):
        calls.append(list(args))
        proc = subprocess.run([str(toolchain / 'lake'), *args], cwd=repo, env=env,
            text=True, capture_output=True, timeout=kwargs['timeout_secs'])
        return dict(returncode=proc.returncode, stdout=proc.stdout, stderr=proc.stderr,
                    timed_out=False, spawn_error='')
    monkeypatch.setattr(obs, '_run_lake_command', run)
    for node in ('A', 'Preamble'):
        obs.write_olean_srcclosure(source, node, obs.tablet_source_closure_hash(source, node))
    replay = obs._kernel_replay_tablet_modules(source, ['Preamble', 'A'], timeout_secs=90, bwrap_role=None)
    assert replay['returncode'] == 0, replay
    assert replay['portable_replay'].get('status') == 'attested', replay['portable_replay']
    baseline_environment = obs._read_olean_provenance(source, 'Preamble')['kernel_replay']['portable_environment_sha256']
    put(source / 'Tablet/A.lean', b'import Tablet.Preamble\ntheorem A : portableZero = 0 := rfl\n')
    built = subprocess.run([str(toolchain / 'lake'), 'build', 'Tablet.A'], cwd=source,
        env=env, text=True, capture_output=True, timeout=90)
    assert built.returncode == 0, built.stdout + built.stderr
    obs.write_olean_srcclosure(source, 'A', obs.tablet_source_closure_hash(source, 'A'))
    after_overlay = obs._kernel_replay_tablet_modules(source, ['Preamble', 'A'], timeout_secs=90, bwrap_role=None)
    assert after_overlay['returncode'] == 0 and after_overlay['checked_nodes'] == ['A'], after_overlay
    assert obs._read_olean_provenance(source, 'Preamble')['kernel_replay']['portable_environment_sha256'] == baseline_environment
    assert obs._read_olean_provenance(source, 'A')['kernel_replay']['portable_environment_sha256'] != baseline_environment
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns('build', 'kernel-replay-environments'))
    put(old / 'completed.json', dict(files={}, key={}))
    put(new / 'canonical.json', dict(key={}))
    put(new / 'replay-cache-donor.json', dict(version=1, donor_round='1',
        donor_completion_sha256=candidate.digest((old / 'completed.json').read_bytes()),
        current_key_sha256=candidate.digest(candidate.canonical_json({}))))
    workspace = SimpleNamespace(runtime_root=new / 'runtime', supervisor_repo=destination,
        _invalidate_compile_cache_for_sync=lambda _: None)
    calls.clear()
    result = donation.install(workspace)
    assert result['status'] == 'installed' and result['covered_nodes'] == ['A', 'Preamble'], result
    assert not any('leanchecker' in call for call in calls), calls
    assert all(obs.olean_has_current_kernel_replay(destination, node) for node in ('A', 'Preamble'))
    assert result['captures'][0]['bytes_hashed'] > 0
    put(tmp_path / 'measurement.json', dict(baseline=replay['portable_replay'],
        candidate=after_overlay['portable_replay'],
        donation={key: value for key, value in result.items() if key != 'artifact_files'},
        artifact_file_bindings=len(result['artifact_files']), replay_commands_on_import=0))
    # The same genuine fresh replay evidence must also survive canonical
    # publication, without a second independent kernel replay.
    from trellis.dispatch import publication_artifacts as publication
    response = dict(local_closure_revalidation=dict(refreshed=[
        [node, dict(node_certificate=dict(artifact_bundle=obs._artifact_bundle(source, node)))]
        for node in ('A', 'Preamble')]))
    journal = tmp_path / 'publication'
    handoff = publication.stage(source.parents[2], destination.parents[2], journal, response)
    assert set(handoff['nodes']) == {'A', 'Preamble'}
    assert handoff['portable_environments']
    monkeypatch.setattr('trellis.supervisor_workspace.sync_supervisor_workspace',
        lambda _: dict(authoritative_repo_path=str(destination)))
    calls.clear()
    publication.install(destination.parents[2], journal, handoff, lambda _: None)
    assert not any('leanchecker' in call for call in calls), calls
    assert all(obs.olean_has_current_kernel_replay(destination, node) for node in ('A', 'Preamble'))
    put(tmp_path / 'publication-measurement.json', dict(owners=sorted(handoff['nodes']),
        portable_environments=handoff['portable_environments'], replay_commands_on_publication=0))
    # Identical module artifacts cannot retain portability after imported bytes change.
    obs._tablet_olean_path(destination, 'Preamble').write_bytes(b'changed imported artifact')
    assert not obs.olean_has_current_kernel_replay(destination, 'Preamble')


def historical_adapter_environment(environment, label):
    value = json.loads(json.dumps(environment))
    previous = next(key for key in value['common_files'] if key.endswith('/scripts/lean_module_manifest.lean'))
    value['files'][label] = value['files'].pop(previous)
    value['common_files'] = sorted(label if key == previous else key for key in value['common_files'])
    return value


def test_historical_host_adapter_relocation_preserves_commitments_and_current_witness(donor):
    from threading import RLock
    source, destination, workspace, _ = donor
    record = obs._read_olean_provenance(source, 'A')
    identity = record['kernel_replay']['portable_environment_sha256']
    original = portable.environment_for(source, identity)
    # Actual historical deployment shape: only the host adapter's installation
    # path changes; its bytes and every imported module remain exact.
    old_label = 'external//path/to/trellis/pv_runs/trellis-native-implementation-v210/scripts/lean_module_manifest.lean'
    old = historical_adapter_environment(original, old_label)
    portable.attach(source, ['A', 'Preamble'], old)
    old_file = source / portable.DIRECTORY / (portable.digest(old) + '.json')
    old_bytes = old_file.read_bytes()
    expected = {node: portable.EnvironmentView(old).owner_digest(node) for node in ('A', 'Preamble')}
    result = donation.install(workspace)
    assert result['status'] == 'installed', result
    assert result['environment_sha256s'] == expected
    assert set(result['adapter_relocation_nodes']) == {'A', 'Preamble'}
    assert old_label[9:] not in result['artifact_files']
    adapter = next(key for key in original['common_files'] if key.endswith('/scripts/lean_module_manifest.lean'))
    assert adapter[9:] in result['artifact_files']
    assert result['adapter_relocations'] == [dict(previous=old_label, current=adapter)]
    workspace._workspace_lock, workspace._sync_lock = RLock(), RLock()
    current = portable.capture_current(workspace, ['A', 'Preamble'])
    assert current['environment']['environment_sha256s'] == expected
    assert old_label[9:] not in current['environment']['artifact_files']
    assert adapter[9:] in current['environment']['artifact_files']
    assert set(current['measurement']['adapter_relocation_nodes']) == {'A', 'Preamble'}
    assert current['measurement']['adapter_relocations'] == result['adapter_relocations']
    # Native Environment.agrees consumes these exact saved owner commitments;
    # Environment.current rehashes the fresh file witness rather than old paths.
    assert current['environment']['environment_sha256s'] == expected
    for path, value in current['environment']['artifact_files'].items():
        assert (candidate.digest(Path(path).read_bytes()) if Path(path).exists() else None) == value
    assert old_file.read_bytes() == old_bytes  # No historical commitment rewrite.


@pytest.mark.parametrize('change', ['adapter_bytes', 'external_library', 'search_path', 'ambiguous', 'module_binding', 'repo_adapter', 'bad_adapter_binding'])
def test_adapter_relocation_cannot_hide_changed_or_ambiguous_inputs(donor, change):
    source, destination, workspace, _ = donor
    identity = obs._read_olean_provenance(source, 'A')['kernel_replay']['portable_environment_sha256']
    environment = portable.environment_for(source, identity)
    label = 'external//old/scripts/lean_module_manifest.lean'
    old = historical_adapter_environment(environment, label)
    if change == 'repo_adapter':
        old = historical_adapter_environment(old, 'repo/scripts/lean_module_manifest.lean')
    elif change == 'bad_adapter_binding':
        old['adapter_sha256'] = 'a' * 64
    elif change == 'adapter_bytes':
        old['files'][label]['sha256'] = old['adapter_sha256'] = 'a' * 64
    elif change == 'external_library':
        old['files']['repo/.lake/packages/dependency/.lake/build/lib/lean/External.olean']['sha256'] = 'a' * 64
    elif change == 'search_path':
        old['search_path'].append('external//different/library')
    elif change == 'ambiguous':
        other = 'external//other/scripts/lean_module_manifest.lean'
        old['files'][other] = dict(old['files'][label])
        old['common_files'].append(other)
    elif change == 'module_binding':
        old['modules']['Tablet.A']['file_bindings'].append(label)
    portable.attach(source, ['A', 'Preamble'], old)
    result = donation.install(workspace)
    assert result['status'] == 'cold', result
    assert result['covered_nodes'] == []
    assert result['reason']
    if change in ('adapter_bytes', 'external_library', 'search_path'):
        assert result['skipped']['import environment changed'] == 2
        assert len(result['skipped_samples']) == 2
    assert not obs.olean_has_current_kernel_replay(destination, 'A')


def test_comparison_retains_external_library_locations_exactly(tmp_path, imported):
    setup, _ = imported
    environment, _ = capture(setup(tmp_path / 'repo'))
    # A non-adapter host binding cannot borrow the adapter's relocation role,
    # even when its contents match at the replacement location.
    environment['common_files'].append('external//old/library.olean')
    environment['files']['external//old/library.olean'] = {'sha256': 'a' * 64, 'size_bytes': 1}
    changed = historical_adapter_environment(environment, 'external//new/scripts/lean_module_manifest.lean')
    changed['files']['external//new/library.olean'] = changed['files'].pop('external//old/library.olean')
    changed['common_files'] = [key.replace('/old/library.olean', '/new/library.olean') for key in changed['common_files']]
    assert not portable.EnvironmentView(environment).same_owner(portable.EnvironmentView(changed), 'A')


@pytest.mark.parametrize('marker', ['missing', 'changed'])
def test_current_alias_requires_authenticated_fixed_donor(donor, marker):
    source, destination, workspace, _ = donor
    current, _ = capture(destination)
    path = workspace.runtime_root.parent / 'replay-cache-donor.json'
    if marker == 'missing':
        path.unlink()
        assert donation.current_owner_aliases(workspace, portable.EnvironmentView(current), ['A']) == ({}, [])
    else:
        value = json.loads(path.read_text())
        value['donor_completion_sha256'] = 'a' * 64
        put(path, value)
        with pytest.raises(portable.Unavailable, match='completion changed'):
            donation.current_owner_aliases(workspace, portable.EnvironmentView(current), ['A'])


@pytest.fixture
def unstamped_owned(donor):
    """Successful owned environment + ordinary replay, with no portable cache."""
    source, destination, workspace, _ = donor
    owned = source.parents[3]
    identity = obs._read_olean_provenance(source, 'A')['kernel_replay']['portable_environment_sha256']
    observed_inventory = historical_adapter_environment(portable.environment_for(source, identity),
        'external//host/source-b7/scripts/lean_module_manifest.lean')
    view = portable.EnvironmentView(observed_inventory)
    observed = dict(version=1, environment_sha256s={'A': view.owner_digest('A')},
        artifact_files={str(portable.bound_path(label, source, observed_inventory['sysroot'])):
            binding['sha256'] if binding is not None else None
            for label, binding in view.files_for(['A']).items()})
    output = dict(stage='owned', status='facts', facts=dict(schema_version=1, owner_ok=True,
        owner_nodes=['A'], observation_environment=observed))
    raw = candidate.canonical_json(output)
    put(owned / 'execution/response.json', raw)
    completed = dict(key={'task_id': 1}, files={'execution/response.json': candidate.digest(raw)})
    put(owned / 'staged-completed.json', completed)
    capture = dict(key={'task_id': 1}, config_sha256='c' * 64)
    put(owned / 'canonical.json', capture)
    final = owned / 'finalization/1'
    target = final / 'repo/.trellis/supervisor/repo'
    target.parent.mkdir(parents=True)
    destination.rename(target)
    workspace.runtime_root = final / 'runtime'
    workspace.supervisor_repo = target
    put(final / 'canonical.json', capture)
    put(final / 'staged-donor.json', dict(version=1, donor_round=owned.name,
        donor_completion_sha256=candidate.digest(candidate.canonical_json(completed)),
        current_key_sha256=candidate.digest(candidate.canonical_json(capture['key'])), owner_nodes=['A']))
    for node in ('A', 'Preamble'):
        record = obs._read_olean_provenance(source, node)
        record['kernel_replay'].pop('portable_environment_sha256')
        assert obs._write_olean_provenance(source, node, record)
    shutil.rmtree(source / portable.DIRECTORY)
    return source, target, workspace, observed


def test_owned_observation_recovers_unstamped_replay_without_rechecking_owner(unstamped_owned, monkeypatch):
    from threading import RLock
    source, destination, workspace, observed = unstamped_owned
    original = {path: path.read_bytes() for path in source.rglob('*') if path.is_file()}
    owner_response = (source.parents[3] / 'execution/response.json').read_bytes()
    monkeypatch.setattr(obs, '_kernel_replay_tablet_modules', lambda *_a, **_kw: pytest.fail('no owner replay'))
    result = donation.install(workspace)
    assert result['status'] == 'installed', result
    assert result['owned_environment_recovered_nodes'] == ['A', 'Preamble']
    assert result['environment_sha256s']['A'] == observed['environment_sha256s']['A']
    for node in ('A', 'Preamble'):
        assert obs.olean_has_current_kernel_replay(destination, node)
        assert obs._read_olean_provenance(destination, node)['kernel_replay']['portable_environment_sha256']
    workspace._workspace_lock, workspace._sync_lock = RLock(), RLock()
    current = portable.capture_current(workspace, ['A'])
    assert current['environment']['environment_sha256s'] == observed['environment_sha256s']
    assert '/host/source-b7/scripts/lean_module_manifest.lean' not in current['environment']['artifact_files']
    assert all(path.read_bytes() == data for path, data in original.items())
    assert not (source / portable.DIRECTORY).exists()
    assert (source.parents[3] / 'execution/response.json').read_bytes() == owner_response


@pytest.mark.parametrize('fault', ['missing_replay', 'stale_replay', 'source', 'import_bytes',
    'toolchain', 'adapter', 'forged', 'not_owned', 'coverage', 'missing_owner', 'partial_owner_match'])
def test_saved_owned_environment_cannot_supply_missing_or_changed_authority(unstamped_owned, monkeypatch, fault):
    source, destination, workspace, observed = unstamped_owned
    owned = source.parents[3]
    if fault in ('missing_replay', 'stale_replay'):
        record = obs._read_olean_provenance(source, 'A')
        if fault == 'missing_replay':
            record.pop('kernel_replay')
        else:
            record['kernel_replay']['artifact_bundle'][0]['sha256'] = 'a' * 64
        obs._write_olean_provenance(source, 'A', record)
    elif fault == 'source':
        (destination / 'Tablet/A.lean').write_text('different source')
    elif fault == 'import_bytes':
        (destination / '.lake/packages/dependency/.lake/build/lib/lean/External.olean').write_bytes(b'changed import')
    elif fault == 'toolchain':
        original_capture = portable.capture
        def changed_capture(*args, **kwargs):
            value, stats = original_capture(*args, **kwargs)
            value['toolchain_sha256'] = 'a' * 64
            return value, stats
        monkeypatch.setattr(portable, 'capture', changed_capture)
    elif fault == 'adapter':
        original_capture = portable.capture
        def changed_capture(*args, **kwargs):
            value, stats = original_capture(*args, **kwargs)
            key = next(key for key in value['common_files'] if key.endswith('/scripts/lean_module_manifest.lean'))
            value['files'][key]['sha256'] = value['adapter_sha256'] = 'a' * 64
            return value, stats
        monkeypatch.setattr(portable, 'capture', changed_capture)
    elif fault == 'forged':
        (owned / 'execution/response.json').write_text('{}')
    else:
        output = json.loads((owned / 'execution/response.json').read_text())
        if fault == 'not_owned':
            output['stage'] = 'finalize'
        elif fault == 'coverage':
            output['facts']['observation_environment']['environment_sha256s'] = {}
        elif fault == 'partial_owner_match':
            output['facts']['owner_nodes'].append('Preamble')
            output['facts']['observation_environment']['environment_sha256s']['Preamble'] = 'a' * 64
        else:
            output['facts']['owner_nodes'].append('Missing')
        raw = candidate.canonical_json(output)
        put(owned / 'execution/response.json', raw)
        complete = json.loads((owned / 'staged-completed.json').read_text())
        complete['files']['execution/response.json'] = candidate.digest(raw)
        put(owned / 'staged-completed.json', complete)
        marker_path = workspace.runtime_root.parent / 'staged-donor.json'
        marker = json.loads(marker_path.read_text())
        marker['donor_completion_sha256'] = candidate.digest(candidate.canonical_json(complete))
        marker['owner_nodes'] = output['facts']['owner_nodes']
        put(marker_path, marker)
    result = donation.install(workspace)
    assert result['status'] == 'cold', result
    assert result['covered_nodes'] == []
    assert result['reason']
    if fault != 'forged':
        assert result['failure_class'] == 'saved_owner_binding'
    assert not obs.olean_has_current_kernel_replay(destination, 'A')


@pytest.fixture
def materialized_donor(donor):
    source, _, _, _ = donor
    for node in ('A', 'Preamble'):
        record = obs._read_olean_provenance(source, node)
        environment = portable.environment_for(source, record['kernel_replay']['portable_environment_sha256'])
        record['kernel_replay']['trusted_direct_imports'] = [dict(module=entry['module'],
            olean=str(portable.bound_path(entry['path'], source, environment['sysroot'])))
            for entry in environment['modules']['Tablet.' + node]['imports']]
        obs._write_olean_provenance(source, node, record)
    return donor


def materialized_workspace(repo):
    import threading
    from trellis.checker.server import CheckerServer, _WorkspaceReaderWriterLock
    workspace = SimpleNamespace(supervisor_repo=repo, _workspace_lock=_WorkspaceReaderWriterLock(),
                                _sync_lock=threading.Lock())
    workspace._closures_have_current_kernel_replay = lambda nodes: CheckerServer._closures_have_current_kernel_replay(workspace, nodes)
    return workspace


def test_materialized_witness_reuses_one_manifest_without_collector_or_donation(materialized_donor, monkeypatch):
    source, _, _, _ = materialized_donor
    calls = []
    original = portable.environment_for
    monkeypatch.setattr(portable, 'environment_for', lambda repo, identity: (calls.append(identity), original(repo, identity))[1])
    monkeypatch.setattr(obs, '_run_lake_command', lambda *a, **kw: pytest.fail('materialized lookup cannot compile'))
    monkeypatch.setattr(donation, 'current_owner_aliases', lambda *a, **kw: pytest.fail('no legacy donor authentication needed'))
    result = portable.materialized_environment(materialized_workspace(source), ['A', 'Preamble'])
    assert result['environment'] is not None, result
    assert len(calls) == 1
    assert set(result['environment']['environment_sha256s']) == {'A', 'Preamble'}
    files = result['environment']['artifact_files']
    assert files[str(source / '.lake/build/lib/lean/External.olean')] is None
    package = source / '.lake/packages/dependency/.lake/build/lib/lean/External.olean'
    assert files[str(package)] == candidate.digest(package.read_bytes())
    assert result['measurement']['status'] == 'materialized'


@pytest.mark.parametrize('change', ['no_stamp', 'missing', 'corrupt', 'symlink', 'owner', 'source', 'toolchain', 'coverage', 'wrong_imports'])
def test_materialized_witness_missing_or_stale_is_optional_without_compiler_fallback(materialized_donor, monkeypatch, change):
    source, _, _, _ = materialized_donor
    record = obs._read_olean_provenance(source, 'A')
    identity = record['kernel_replay']['portable_environment_sha256']
    manifest = source / portable.DIRECTORY / (identity + '.json')
    nodes = ['A', 'Preamble']
    if change == 'no_stamp':
        del record['kernel_replay']['portable_environment_sha256']
        obs._write_olean_provenance(source, 'A', record)
    elif change == 'missing': manifest.unlink()
    elif change == 'corrupt': manifest.write_text('{}')
    elif change == 'symlink':
        carrier = manifest.with_suffix('.actual'); manifest.rename(carrier); manifest.symlink_to(carrier)
    elif change == 'owner': (source / '.lake/build/lib/lean/Tablet/A.olean').write_bytes(b'new artifact')
    elif change == 'source': (source / 'Tablet/Preamble.lean').write_text('import Different\n')
    elif change == 'toolchain': (source / 'lean-toolchain').write_text('different\n')
    elif change == 'coverage': nodes.append('Missing')
    elif change == 'wrong_imports':
        record['kernel_replay']['trusted_direct_imports'] = []
        obs._write_olean_provenance(source, 'A', record)
    monkeypatch.setattr(obs, '_run_lake_command', lambda *a, **kw: pytest.fail('no compiler fallback'))
    result = portable.materialized_environment(materialized_workspace(source), nodes)
    assert result['environment'] is None, (change, result)
    assert result['cold_reason']


def test_materialized_witness_keeps_historical_absences_for_native_end_guard(materialized_donor):
    source, _, _, _ = materialized_donor
    absent = source / '.lake/build/lib/lean/External.olean'
    absent.write_bytes(b'shadowing import created after capture')
    result = portable.materialized_environment(materialized_workspace(source), ['A'])
    # This endpoint returns the committed witness, not a current verdict. The
    # required native end guard rejects this newly occupied resolution slot.
    assert result['environment']['artifact_files'][str(absent)] is None


def test_materialized_only_capture_flag_is_strict_boolean():
    from trellis.checker.protocol import validate_request, ProtocolError
    for value in (True, False):
        validate_request(dict(op='capture_replay_environment', request_id=1, node_names=['A'], materialized_only=value))
    for value in (None, 1, 'true'):
        with pytest.raises(ProtocolError):
            validate_request(dict(op='capture_replay_environment', request_id=1, node_names=['A'], materialized_only=value))


def test_capture_path_memo_preserves_full_environment(tmp_path, imported, monkeypatch):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    actual, _ = capture(repo)

    original_state = portable._CapturePaths.state
    class UnmemoizedPaths:
        def __init__(self, repo): self.repo = repo; self.bindings = {}; self.initial_states = {}
        def resolve(self, path):
            resolved = portable._resolved(path, self.repo)
            self.initial_states.setdefault(resolved, original_state(resolved))
            return resolved
        def identity(self, path, sysroot): return portable._identity_path(path, self.repo, sysroot)
        def validate(self, budget): pass

    monkeypatch.setattr(portable, '_CapturePaths', UnmemoizedPaths)
    expected, _ = capture(repo)
    assert actual == expected  # Same content, import graph, tools and absences.


@pytest.mark.parametrize('mutation', ['appearance', 'replacement', 'same_inode', 'alias', 'parent_swap'])
def test_capture_path_memo_rechecks_absences_files_and_ancestors(tmp_path, mutation):
    repo = tmp_path / 'repo'
    parent = repo / 'build'
    parent.mkdir(parents=True)
    path = parent / 'A.olean'
    if mutation != 'appearance':
        path.write_bytes(b'before')
    memo = portable._CapturePaths(repo)
    assert memo.resolve(path) == path
    assert memo.resolve(path) == path
    if mutation == 'appearance':
        path.write_bytes(b'new shadow')
    elif mutation == 'replacement':
        other = parent / 'replacement'
        other.write_bytes(b'before')
        other.replace(path)
    elif mutation == 'same_inode':
        stamp = path.stat()
        path.write_bytes(b'change')
        os.utime(path, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        assert path.stat().st_ino == stamp.st_ino
        assert path.stat().st_size == stamp.st_size
    elif mutation == 'alias':
        other = repo / 'other'
        other.write_bytes(b'before')
        path.unlink()
        path.symlink_to(other)
    else:
        parent.rename(repo / 'old')
        parent.mkdir()
        path.write_bytes(b'before')
    with pytest.raises(portable.Unavailable, match='alias|resolution changed'):
        memo.validate(portable.HashBudget())


def test_capture_path_memo_rejects_alias_before_parent_traversal(tmp_path):
    repo = tmp_path / 'repo'
    repo.mkdir()
    (repo / 'real').mkdir()
    (repo / 'alias').symlink_to(repo / 'real', target_is_directory=True)
    with pytest.raises(portable.Unavailable, match='alias'):
        portable._CapturePaths(repo).resolve(repo / 'alias/../A.olean')


def test_capture_path_memo_is_bounded_and_does_not_cross_captures(tmp_path, monkeypatch):
    repo = tmp_path / 'repo'
    repo.mkdir()
    path = repo / 'A.olean'
    first = portable._CapturePaths(repo)
    first.resolve(path)
    path.write_bytes(b'new')
    second = portable._CapturePaths(repo)
    second.resolve(path)
    second.validate(portable.HashBudget())
    with pytest.raises(portable.Unavailable, match='resolution changed'):
        first.validate(portable.HashBudget())
    monkeypatch.setattr(portable, 'MAX_FILE_BINDINGS', 1)
    with pytest.raises(portable.Unavailable, match='path validation limit'):
        portable._CapturePaths(repo).resolve(repo / 'B.olean')


def test_capture_telemetry_records_cold_reason_without_commitments(tmp_path, imported):
    from trellis.checker import telemetry
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    state = telemetry.Measurement()
    token = telemetry._current.set(state)
    try:
        capture(repo)
        portable.capture(repo, [], timeout_secs=60, bwrap_role='lake_compiler')
    finally:
        telemetry._current.reset(token)
    rows = state.snapshot()['portable_captures']
    assert rows[0]['status'] == 'captured'
    assert rows[1]['reason'] == 'empty/oversized replay roots'
    assert all('environment_sha256' not in row for row in rows)
    assert str(repo) not in json.dumps(rows)


def test_capture_path_validation_ignores_unrelated_sibling_creation(tmp_path):
    path = tmp_path / 'owner.olean'
    path.write_bytes(b'exact')
    memo = portable._CapturePaths(tmp_path)
    memo.resolve(path)
    (tmp_path / 'unrelated').write_bytes(b'new')
    memo.validate(portable.HashBudget())


@pytest.mark.parametrize('failure,retryable', [
    (portable.CaptureUnavailable('import inventory unavailable or oversized'), True),
    (portable.Unavailable('import artifact changed during content capture'), False),
    (portable.Unavailable('import resolution changed during content capture'), False),
    (portable.Unavailable('import alias is not portable'), False),
])
def test_current_capture_distinguishes_resource_unavailability_from_binding_changes(tmp_path, imported, monkeypatch, failure, retryable):
    import threading
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    def fail(*args, **kwargs): raise failure
    monkeypatch.setattr(portable.HashBudget, 'file', fail)
    workspace = SimpleNamespace(supervisor_repo=repo, _workspace_lock=threading.RLock(), _sync_lock=threading.RLock())
    result = portable.capture_current(workspace, ['A'])
    assert result['environment'] is None
    assert result['cold_reason'] == str(failure)
    assert result['failure_class'] == ('environment_capture' if retryable else 'environment_binding')


def _during_first_digest(monkeypatch, action, *, every=False):
    original = portable.hashlib.sha256
    count = [0]
    class Digest:
        def __init__(self, *args, **kwargs): self.inner = original(*args, **kwargs)
        def update(self, value):
            self.inner.update(value)
            count[0] += 1
            if every or count[0] == 1: action(count[0])
        def hexdigest(self): return self.inner.hexdigest()
    monkeypatch.setattr(portable.hashlib, 'sha256', Digest)
    return count


@pytest.mark.parametrize('operation', ['link', 'unlink', 'link_unlink'])
def test_first_unbound_hash_cannot_assume_ctime_churn_preserved_bytes(tmp_path, monkeypatch, operation):
    path = tmp_path / 'owner.olean'
    data = b'unchanged package artifact' * 4096
    path.write_bytes(data)
    other = tmp_path / 'new-workspace.olean'
    if operation == 'unlink': os.link(path, other)
    def change(_):
        if operation != 'unlink': os.link(path, other)
        if operation != 'link': other.unlink()
    count = _during_first_digest(monkeypatch, change)
    budget = portable.HashBudget(max_bytes=3*len(data))
    with pytest.raises(portable.Unavailable, match='artifact changed'):
        budget.file(path)
    assert path.read_bytes() == data
    assert budget.files == 1 and count[0] == 1
    assert path not in budget.memo  # No old binding exists, despite a plausible host link operation.


@pytest.mark.parametrize('mutation', ['bytes', 'inode', 'mode', 'mtime', 'alias'])
def test_hash_revalidation_never_hides_real_changes_behind_link_churn(tmp_path, monkeypatch, mutation):
    path = tmp_path / 'owner.olean'
    data = b'original'
    path.write_bytes(data)
    before = path.stat()
    def change(_):
        os.link(path, tmp_path / 'link')
        if mutation == 'bytes':
            path.write_bytes(b'forgery!')
            os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
        elif mutation == 'inode':
            replacement = tmp_path / 'replacement'
            replacement.write_bytes(data)
            os.utime(replacement, ns=(before.st_atime_ns, before.st_mtime_ns))
            replacement.replace(path)
        elif mutation == 'mode': path.chmod(before.st_mode ^ 0o100)
        elif mutation == 'mtime': os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns + 1))
        else:
            path.unlink()
            path.symlink_to(tmp_path / 'link')
    _during_first_digest(monkeypatch, change)
    with pytest.raises(portable.Unavailable, match='artifact .*changed'):
        portable.HashBudget().file(path)


def test_hash_revalidation_is_bounded_and_charges_existing_budget(tmp_path, monkeypatch):
    path = tmp_path / 'owner.olean'
    data = b'exact'
    path.write_bytes(data)
    budget = portable.HashBudget(max_bytes=3*len(data))
    expected = budget.file(path)
    original_memo = budget.memo[path]
    def change(index):
        other = tmp_path / f'link-{index}'
        os.link(path, other)
        other.unlink()
    count = _during_first_digest(monkeypatch, change, every=True)
    with pytest.raises(portable.CaptureUnavailable, match='metadata did not settle'):
        budget.file(path, expected=expected)
    assert count[0] == portable.MAX_CONTENT_READS - 1 == 2
    assert budget.files == 3 and budget.bytes == 3*len(data) and path.read_bytes() == data
    assert budget.memo[path] == original_memo  # Never publish an unstable replacement binding.


@pytest.mark.parametrize('limit', ['bytes', 'time'])
def test_hash_revalidation_cannot_reset_byte_or_time_limits(tmp_path, monkeypatch, limit):
    path = tmp_path / 'owner.olean'
    path.write_bytes(b'exact')
    budget = portable.HashBudget(max_bytes=5 if limit == 'bytes' else 100)
    expected = budget.file(path)
    os.link(path, tmp_path / 'link')
    if limit == 'time': budget.deadline = 0
    with pytest.raises(portable.CaptureUnavailable, match='file/byte limit|deadline exhausted'):
        budget.file(path, expected=expected)
    assert budget.bytes == 5


@pytest.mark.parametrize('overwrite_memo', [False, True])
def test_path_end_guard_revalidates_original_binding_not_newest_memo(tmp_path, overwrite_memo):
    path = tmp_path / 'A.olean'
    path.write_bytes(b'original')
    before = path.stat()
    paths = portable._CapturePaths(tmp_path)
    paths.resolve(path)
    budget = portable.HashBudget()
    binding = budget.file(path)
    paths.bindings[path] = binding
    os.link(path, tmp_path / 'other-workspace.olean')
    if overwrite_memo:
        path.write_bytes(b'forgery!')
        os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
        assert budget.file(path) != binding  # A later call cannot rewrite the first commitment.
        with pytest.raises(portable.Unavailable, match='bytes changed'):
            paths.validate(budget)
    else:
        paths.validate(budget)
        assert budget.bytes == 2*len(b'original')
        assert budget.file(path) == binding


def test_unhashed_path_end_guard_does_not_assume_link_churn_is_safe(tmp_path):
    path = tmp_path / 'unhashed.olean'
    path.write_bytes(b'exact')
    paths = portable._CapturePaths(tmp_path)
    paths.resolve(path)
    os.link(path, tmp_path / 'link')
    with pytest.raises(portable.Unavailable, match='resolution changed'):
        paths.validate(portable.HashBudget())


def test_capture_revalidates_real_link_churn_without_repeating_inventory(tmp_path, imported, monkeypatch):
    from trellis.checker import telemetry
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    expected, before_stats = capture(repo)
    original = portable.HashBudget.file
    linked = []
    def file(budget, path, **kwargs):
        result = original(budget, path, **kwargs)
        if Path(path).name == 'A.olean' and not linked:
            linked.append(True)
            os.link(path, tmp_path / 'another-workspace.olean')
        return result
    monkeypatch.setattr(portable.HashBudget, 'file', file)
    measurement = telemetry.Measurement()
    token = telemetry._current.set(measurement)
    try:
        actual, stats = capture(repo)
    finally:
        telemetry._current.reset(token)
    assert actual == expected
    assert stats['files_hashed'] == before_stats['files_hashed'] + 1
    assert stats['content_revalidation_reads'] == 1
    assert before_stats['content_revalidation_reads'] == 0
    assert linked == [True]
    assert measurement.snapshot()['portable_captures'][0]['content_revalidation_reads'] == 1
    cost = measurement.portable_captures[0]
    assert cost['timed_revalidation_reads'] == 1
    assert cost['read_final_validation_wall_ms'] > 0
    assert cost['timed_read_attempts'] == stats['files_hashed']
    assert cost['timed_read_bytes'] == stats['bytes_hashed']


def test_first_reference_binding_keeps_owner_labels_and_hashes_each_file_once(tmp_path, imported):
    setup, _ = imported
    environment, stats = capture(setup(tmp_path / 'repo'))
    for node in ('A', 'Preamble'):
        exported = f'repo/.lake/build/lib/lean/Tablet/{node}.olean'
        assert set(environment['modules'][f'Tablet.{node}']['file_bindings']) == {
            exported, exported + '.server', exported + '.private'}
    commitments = [value for value in environment['files'].values() if value is not None]
    assert stats['files_hashed'] == len(commitments)
    assert stats['bytes_hashed'] == sum(value['size_bytes'] for value in commitments)
    assert stats['content_revalidation_reads'] == 0


@pytest.mark.parametrize('mutation', ['mirror', 'unlink', 'bytes', 'inode', 'mode', 'alias'])
def test_first_import_reference_binds_bytes_before_later_module_row(tmp_path, imported, monkeypatch, mutation):
    from threading import Thread
    from trellis.supervisor_workspace import _ensure_supervisor_lake_packages
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    artifact = repo / '.lake/packages/dependency/.lake/build/lib/lean/External.olean'
    baseline, baseline_stats = capture(repo)
    original_bytes = artifact.read_bytes()
    before = artifact.stat()
    alias = tmp_path / 'existing-alias'
    if mutation == 'unlink': os.link(artifact, alias)
    changed = []

    def change():
        if mutation == 'unlink':
            alias.unlink()
        else:
            # The actual cold advisory mirror creates shared package links.
            result = _ensure_supervisor_lake_packages(repo, tmp_path / 'cold-job')
            assert result['files_linked'] > 0
            if mutation == 'bytes':
                artifact.write_bytes(b'X' * before.st_size)
                os.utime(artifact, ns=(before.st_atime_ns, before.st_mtime_ns))
            elif mutation == 'inode':
                alias.write_bytes(original_bytes)
                os.utime(alias, ns=(before.st_atime_ns, before.st_mtime_ns))
                alias.replace(artifact)
            elif mutation == 'mode':
                artifact.chmod(before.st_mode ^ 0o100)
            elif mutation == 'alias':
                alias.write_bytes(original_bytes)
                artifact.unlink()
                artifact.symlink_to(alias)
        changed.append(True)

    loads = json.loads
    class LaterModule(dict):
        def __getitem__(self, key):
            if key == 'module' and not changed:
                # Root imports have already named External, but its inventory
                # row has not yet been consumed. Use a real concurrent writer.
                thread = Thread(target=change)
                thread.start()
                thread.join(timeout=5)
                assert not thread.is_alive() and changed == [True]
            return super().__getitem__(key)
    def load(raw, *args, **kwargs):
        value = loads(raw, *args, **kwargs)
        return LaterModule(value) if isinstance(value, dict) and value.get('module') == 'External' else value
    monkeypatch.setattr(portable.json, 'loads', load)
    environment, stats = portable.capture(repo, ['A', 'Preamble'], timeout_secs=60, bwrap_role='lake_compiler')
    assert changed == [True]
    if mutation in ('mirror', 'unlink'):
        assert environment == baseline, stats
        assert stats['content_revalidation_reads'] == 1
        assert stats['files_hashed'] == baseline_stats['files_hashed'] + 1
        assert stats['bytes_hashed'] == baseline_stats['bytes_hashed'] + len(original_bytes)
    else:
        assert environment is None, 'real changes must not hide behind package link churn'
        assert stats['failure_class'] == 'environment_binding'


@pytest.mark.parametrize('point', ['resolver_to_hash', 'before_first_read'])
def test_capture_rejects_pre_first_hash_restored_mtime_forgery(tmp_path, imported, monkeypatch, point):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    artifact = repo / '.lake/build/lib/lean/Tablet/A.olean'
    before = artifact.stat()
    original_open = Path.open
    original_file = portable.HashBudget.file
    changed = []
    def mutate():
        if changed: return
        changed.append(True)
        with original_open(artifact, 'wb') as handle: handle.write(b'X' * before.st_size)
        os.utime(artifact, ns=(before.st_atime_ns, before.st_mtime_ns))
    def file(budget, path, **kwargs):
        if path == artifact and point == 'resolver_to_hash': mutate()
        return original_file(budget, path, **kwargs)
    def opening(path, mode='r', *args, **kwargs):
        if path == artifact and mode == 'rb' and point == 'before_first_read': mutate()
        return original_open(path, mode, *args, **kwargs)
    monkeypatch.setattr(portable.HashBudget, 'file', file)
    monkeypatch.setattr(Path, 'open', opening)
    environment, stats = portable.capture(repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler')
    assert environment is None and changed == [True]
    assert stats['failure_class'] == ('environment_capture' if point == 'resolver_to_hash' else 'environment_binding')
    assert stats['content_revalidation_reads'] == 0
    assert 'changed' in stats['reason']
    assert stats['failure_detail']['path'] == str(artifact)[-320:]
    assert 'st_ctime_ns' in stats['failure_detail']['changed_fields']


def test_end_guard_revalidation_is_shared_across_lexical_spellings(tmp_path):
    path = tmp_path / 'A.olean'
    path.write_bytes(b'exact')
    paths = portable._CapturePaths(tmp_path)
    alternate = tmp_path / '../' / tmp_path.name / 'A.olean'
    paths.resolve(alternate)
    paths.resolve(path)
    budget = portable.HashBudget()
    binding = budget.file(path, initial_state=paths.initial_states[path])
    paths.bindings[path] = binding
    os.link(path, tmp_path / 'link')
    paths.validate(budget)
    assert budget.files == 2 and budget.content_revalidations == 1


@pytest.fixture
def mixed_owned_witness(donor, imported, monkeypatch):
    """A real artifact donation followed by the historical mixed owned witness."""
    from threading import RLock
    from trellis.dispatch import acceptance_receipts as receipts
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'fixture-implementation')
    setup, _ = imported
    source, owned_repo, workspace, _ = donor
    old, owned = source.parents[3], owned_repo.parents[3]
    key = dict(task_id=1, candidate_sha256='c'*64, task_binding_sha256='b'*64,
               implementation_identity='fixture-implementation')
    old_response = candidate.canonical_json(dict(status='ordinary checked'))
    old_completed = dict(key=key, files={'execution/response.json': candidate.digest(old_response)})
    put(old / 'execution/response.json', old_response)
    put(old / 'completed.json', old_completed)
    captured = dict(key=key, config_sha256='d'*64, staged_plan={'owner_nodes':['A']})
    put(owned / 'canonical.json', captured)
    put(owned / 'replay-cache-donor.json', dict(version=1, donor_round=old.name,
        donor_completion_sha256=candidate.digest(candidate.canonical_json(old_completed)),
        current_key_sha256=candidate.digest(candidate.canonical_json(key))))
    identity = obs._read_olean_provenance(source, 'A')['kernel_replay']['portable_environment_sha256']
    historical = historical_adapter_environment(portable.environment_for(source, identity),
        'external//host/old-source/scripts/lean_module_manifest.lean')
    portable.attach(source, ['A', 'Preamble'], historical)
    installation = donation.install(workspace)
    assert installation['status'] == 'installed', installation
    fresh, _ = capture(owned_repo)
    observed = dict(version=1, environment_sha256s={'A':portable.EnvironmentView(historical).owner_digest('A')},
        artifact_files=donation._artifact_witness(portable.EnvironmentView(fresh), owned_repo, ['A']))
    legacy = dict(version=1, donor_round=old.name,
        donor_completion_sha256=candidate.digest(candidate.canonical_json(old_completed)),
        current_key_sha256=candidate.digest(candidate.canonical_json(key)),
        producer_identity=key['implementation_identity'], consumer_identity=key['implementation_identity'], owner_nodes=['A'])
    (owned / 'replay-cache-donor.json').unlink()
    put(owned / 'legacy-owner-donor.json', legacy)
    output = dict(stage='owned', status='facts', replay_donation={'captures':installation['captures']},
        facts=dict(schema_version=1, owner_ok=True, owner_nodes=['A'],
                   acceptance_logic_identity=key['implementation_identity'], observation_environment=observed))
    raw = candidate.canonical_json(output)
    completed = dict(key=key, files={'execution/response.json':candidate.digest(raw),
        'legacy-owner-donor.json':candidate.digest(candidate.canonical_json(legacy))})
    put(owned / 'execution/response.json', raw)
    put(owned / 'staged-completed.json', completed)
    request = dict(phase='ProofFormalization', kind='worker', acceptance_logic_identity=key['implementation_identity'])
    put(owned / 'runtime/protocol_state.json', {'in_flight_request':request})
    final = owned / 'finalization/1'
    target = setup(final / 'repo/.trellis/supervisor/repo')
    put(final / 'canonical.json', captured)
    put(final / 'runtime/protocol_state.json', {'in_flight_request':request})
    put(final / 'staged-donor.json', dict(version=1, donor_round=owned.name,
        donor_completion_sha256=candidate.digest(candidate.canonical_json(completed)),
        current_key_sha256=candidate.digest(candidate.canonical_json(key)), owner_nodes=['A']))
    for node in ('A', 'Preamble'):
        record = obs._read_olean_provenance(owned_repo, node)
        record['kernel_replay'].pop('portable_environment_sha256')
        assert obs._write_olean_provenance(owned_repo, node, record)
    workspace.runtime_root, workspace.supervisor_repo = final / 'runtime', target
    workspace._workspace_lock, workspace._sync_lock = RLock(), RLock()
    return SimpleNamespace(source=source, owned_repo=owned_repo, owned=owned, final=final,
        workspace=workspace, observed=observed, historical=historical, fresh=fresh,
        installation=installation, completed=completed, output=output)


@pytest.mark.parametrize('marker', ['legacy', 'retry'])
def test_new_owned_observations_never_inherit_artifact_donor_aliases(mixed_owned_witness, monkeypatch, marker):
    f = mixed_owned_witness
    workspace = SimpleNamespace(runtime_root=f.owned / 'runtime', supervisor_repo=f.owned_repo,
        _workspace_lock=f.workspace._workspace_lock, _sync_lock=f.workspace._sync_lock)
    if marker == 'retry':
        (f.owned / 'legacy-owner-donor.json').unlink()
        put(f.owned / 'replay-cache-donor.json', {'donor_kind':'owned_capture_retry'})
    monkeypatch.setattr(donation, '_marker', lambda *_a, **_kw: pytest.fail('fresh facts need no alias authority'))
    actual = portable.capture_current(workspace, ['A'])
    assert actual['environment']['environment_sha256s'] == {'A':portable.EnvironmentView(f.fresh).owner_digest('A')}
    assert actual['environment']['environment_sha256s'] != f.observed['environment_sha256s']
    assert actual['environment']['artifact_files'] == f.observed['artifact_files']


@pytest.mark.parametrize('stamped', [False, True])
def test_mixed_owned_witness_recovers_exact_retained_chain_without_rechecking(mixed_owned_witness, monkeypatch, stamped):
    f = mixed_owned_witness
    if stamped:
        portable.attach(f.owned_repo, ['A','Preamble'], f.fresh)
    before = {path:path.read_bytes() for path in (f.owned/'execution/response.json',f.owned/'staged-completed.json')}
    monkeypatch.setattr(obs, '_kernel_replay_tablet_modules', lambda *_a, **_kw: pytest.fail('no owner replay'))
    result = donation.install(f.workspace)
    assert result['status'] == 'installed', result
    actual = portable.capture_current(f.workspace, ['A'])
    assert actual['environment']['environment_sha256s'] == f.observed['environment_sha256s'], actual
    current, _ = capture(f.workspace.supervisor_repo)
    assert actual['environment']['artifact_files'] == donation._artifact_witness(
        portable.EnvironmentView(current), f.workspace.supervisor_repo, ['A'])
    assert all(path.read_bytes() == raw for path,raw in before.items())
    assert donation.recover_owned_alias(f.final/'runtime',portable.EnvironmentView(current))[0] == f.observed['environment_sha256s']


def _replace_owned_output(f):
    raw = candidate.canonical_json(f.output)
    f.completed['files']['execution/response.json'] = candidate.digest(raw)
    put(f.owned/'execution/response.json',raw)
    put(f.owned/'staged-completed.json',f.completed)
    marker = json.loads((f.final/'staged-donor.json').read_text())
    marker['donor_completion_sha256'] = candidate.digest(candidate.canonical_json(f.completed))
    put(f.final/'staged-donor.json',marker)


@pytest.mark.parametrize('fault', ['immediate_missing','witness_absence','witness_bytes','unreferenced',
    'marker_changed','candidate','forward','hop_limit','manifest_limit','import_bytes','toolchain','adapter','partial'])
def test_mixed_witness_recovery_rejects_missing_or_changed_evidence(mixed_owned_witness, monkeypatch, fault):
    f = mixed_owned_witness
    current = json.loads(json.dumps(f.fresh))
    if fault == 'immediate_missing':
        (f.owned_repo/portable.DIRECTORY/(portable.digest(f.fresh)+'.json')).unlink()
    elif fault in ('witness_absence','witness_bytes'):
        files = f.output['facts']['observation_environment']['artifact_files']
        if fault == 'witness_absence': files.pop(next(path for path,value in files.items() if value is None))
        else: files[next(path for path,value in files.items() if value is not None)] = 'e'*64
        _replace_owned_output(f)
    elif fault == 'unreferenced':
        record = obs._read_olean_provenance(f.source,'A')
        record['kernel_replay'].pop('portable_environment_sha256')
        obs._write_olean_provenance(f.source,'A',record)
        # Matching historical manifest remains on disk but is no longer referenced.
    elif fault in ('marker_changed','candidate','forward'):
        marker = json.loads((f.owned/'legacy-owner-donor.json').read_text())
        if fault == 'marker_changed': marker['donor_completion_sha256'] = 'f'*64
        elif fault == 'forward': marker['donor_round'] = f.owned.name
        else:
            completed = json.loads((f.source.parents[3]/'completed.json').read_text())
            completed['key']['candidate_sha256'] = 'f'*64
            put(f.source.parents[3]/'completed.json',completed)
            marker['donor_completion_sha256'] = candidate.digest(candidate.canonical_json(completed))
        put(f.owned/'legacy-owner-donor.json',marker)
        if fault != 'marker_changed':
            f.completed['files']['legacy-owner-donor.json'] = candidate.digest(candidate.canonical_json(marker))
            _replace_owned_output(f)
    elif fault == 'hop_limit': monkeypatch.setattr(donation,'MAX_DONOR_HOPS',0)
    elif fault == 'manifest_limit': monkeypatch.setattr(donation,'MAX_ENVIRONMENTS',1)
    elif fault == 'import_bytes': current['files']['repo/.lake/packages/dependency/.lake/build/lib/lean/External.olean']['sha256']='f'*64
    elif fault == 'toolchain': current['toolchain_sha256']='f'*64
    elif fault == 'adapter':
        label=next(label for label in current['common_files'] if label.endswith('/scripts/lean_module_manifest.lean'))
        current['files'][label]['sha256']=current['adapter_sha256']='f'*64
    else:
        f.output['facts']['owner_nodes'].append('Preamble')
        f.output['facts']['observation_environment']['environment_sha256s']['Preamble']='f'*64
        _replace_owned_output(f)
        marker=json.loads((f.final/'staged-donor.json').read_text());marker['owner_nodes']=['A','Preamble']
        put(f.final/'staged-donor.json',marker)
    with pytest.raises((portable.Unavailable,candidate.CandidateError,OSError)):
        donation.recover_owned_alias(f.final/'runtime',portable.EnvironmentView(current))


def test_old_cross_producer_cold_preflight_keeps_historical_consumer_explicit(mixed_owned_witness, monkeypatch):
    from trellis.dispatch import acceptance_receipts as receipts, staged_acceptance as stages
    f=mixed_owned_witness
    captured=json.loads((f.final/'canonical.json').read_text())
    captured['key']['implementation_identity']='historical-finalizer'
    put(f.final/'canonical.json',captured)
    put(f.final/'runtime/protocol_state.json',{'in_flight_request':dict(phase='ProofFormalization',kind='worker',
        acceptance_logic_identity='historical-finalizer')})
    # Cross-producer authentication also binds host authority and math scope.
    f.completed['key']['authority_input_hashes']={'trusted':'a'*64}
    f.completed['key']['implementation_identity']='fixture-implementation'
    captured['key']['authority_input_hashes']={'trusted':'a'*64}
    put(f.final/'canonical.json',captured)
    _replace_owned_output(f)
    marker=json.loads((f.final/'staged-donor.json').read_text())
    marker['current_key_sha256']=candidate.digest(candidate.canonical_json(captured['key']))
    marker['producer_compatibility']=dict(version=1,kind='reviewed_staged_owner_producer',
        producer_identity='fixture-implementation',consumer_identity='historical-finalizer')
    put(f.final/'staged-donor.json',marker)
    monkeypatch.setattr(receipts,'implementation_identity',lambda:'new-implementation')
    monkeypatch.setattr(stages,'reviewed_owned_producers',lambda:frozenset({'fixture-implementation','historical-finalizer'}))
    with pytest.raises(candidate.CandidateError,match='not compatible'):
        donation.recover_owned_alias(f.final/'runtime',portable.EnvironmentView(f.fresh))
    assert donation.recover_owned_alias(f.final/'runtime',portable.EnvironmentView(f.fresh),
        historical_consumer='historical-finalizer')[0] == f.observed['environment_sha256s']


@pytest.mark.parametrize('fault', ['cycle','depth'])
def test_recovery_bounds_retry_chain_before_recursive_authentication(mixed_owned_witness, monkeypatch, fault):
    from trellis.dispatch import staged_driver
    f=mixed_owned_witness
    put(f.owned/'replay-cache-donor.json',dict(donor_kind='owned_capture_retry',
        donor_round=f.owned.name if fault=='cycle' else f.source.parents[3].name))
    if fault=='depth': monkeypatch.setattr(donation,'MAX_DONOR_HOPS',0)
    monkeypatch.setattr(staged_driver,'owned_retry_donor',lambda *_a,**_kw:pytest.fail('unbounded recursive authentication'))
    with pytest.raises(portable.Unavailable,match='donor chain'):
        donation.recover_owned_alias(f.final/'runtime',portable.EnvironmentView(f.fresh))


def test_donation_consumes_independent_manifests_without_retaining_inventories(donor, monkeypatch):
    source, destination, workspace, _ = donor
    inventory = obs._run_lake_command
    def with_c(repo, args, **kwargs):
        result = inventory(repo, args, **kwargs)
        result['stdout'] += '\n' + json.dumps(dict(module='Tablet.C',
            parts=[dict(level='exported', path=str(repo / '.lake/build/lib/lean/Tablet/C.olean'))],
            imports=[dict(module='Tablet.A', olean=str(repo / '.lake/build/lib/lean/Tablet/A.olean'),
                          import_all=False, is_exported=True, is_meta=False)]))
        return result
    monkeypatch.setattr(obs, '_run_lake_command', with_c)
    for repo in (source, destination):
        put(repo / 'Tablet/C.lean', b'import Tablet.A\n')
        put(repo / '.lake/build/lib/lean/Tablet/C.olean', b'C artifact')
    obs.write_olean_srcclosure(source, 'C', obs.tablet_source_closure_hash(source, 'C'))
    assert obs._write_kernel_replay_attestation(source, 'C', olean_sha256='')
    env, _ = capture(source)
    for node in ('A', 'Preamble', 'C'):
        # Independent replay batches have different root lists but equal
        # per-owner imported environments, as in the production donor.
        portable.attach(source, [node], dict(env, roots=['Tablet.' + node]))
    load = donation._environment
    live = set()
    class TrackedManifest(dict):
        def __del__(self): live.discard(id(self))
    def tracked(*args, **kwargs):
        assert not live, 'previous full manifest still retained'
        document, recovered = load(*args, **kwargs)
        document = TrackedManifest(document)
        live.add(id(document))
        return document, recovered
    monkeypatch.setattr(donation, '_environment', tracked)
    result = donation.install(workspace)
    assert result['status'] == 'installed', result
    assert result['covered_nodes'] == ['A', 'C', 'Preamble']
    assert not live
    current, _ = capture(destination)
    assert donation.current_owner_aliases(workspace, portable.EnvironmentView(current), ['A', 'C', 'Preamble']) == ({}, [])
    assert not live


@pytest.mark.parametrize('failure', ['exit', 'timeout', 'spawn', 'size'])
def test_capture_preserves_bounded_inventory_failure_diagnostic(imported, tmp_path, monkeypatch, failure):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    result = dict(returncode=0, stdout='', stderr='x' * 4096)
    if failure == 'exit': result['returncode'] = 17
    elif failure == 'timeout': result['timed_out'] = True
    elif failure == 'spawn': result['spawn_error'] = 'missing compiler'
    else:
        monkeypatch.setattr(portable, 'MAX_MANIFEST_BYTES', 10)
        result['stdout'] = 'x' * 11
    monkeypatch.setattr(obs, '_run_lake_command', lambda *_a, **_kw: result)
    environment, stats = portable.capture(repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler')
    assert environment is None
    assert stats['reason'] == 'import inventory unavailable or oversized'
    detail = stats['failure_detail']
    assert detail['returncode'] == result['returncode']
    assert detail['timed_out'] == bool(result.get('timed_out'))
    assert detail['spawn_error'] == result.get('spawn_error', '')
    assert detail['stdout_bytes'] == len(result['stdout'])
    assert len(detail['stderr_tail']) == 2048


def test_initial_link_drift_stays_unbound_retryable_and_keeps_diagnostics(tmp_path, imported, monkeypatch):
    from trellis.checker import telemetry
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    artifact = repo / '.lake/build/lib/lean/Tablet/A.olean'
    original = portable.HashBudget.file
    linked = []
    def file(budget, path, **kwargs):
        if Path(path) == artifact and not linked:
            linked.append(True)
            os.link(path, tmp_path / 'other-workspace.olean')
        return original(budget, path, **kwargs)
    monkeypatch.setattr(portable.HashBudget, 'file', file)
    measurement = telemetry.Measurement()
    token = telemetry._current.set(measurement)
    try:
        environment, stats = portable.capture(repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler')
    finally:
        telemetry._current.reset(token)
    assert environment is None  # Metadata similarity never supplies byte identity.
    assert stats['failure_class'] == 'environment_capture'
    assert stats['reason'] == 'import resolution changed before initial content binding'
    detail = stats['failure_detail']
    assert detail['path'] == str(artifact)[-320:]
    assert 'st_nlink' in detail['changed_fields']
    assert measurement.snapshot()['portable_captures'][0]['failure_detail'] == detail
    fresh, _ = capture(repo)  # A separate complete capture can now bind actual bytes.
    assert fresh['files']['repo/.lake/build/lib/lean/Tablet/A.olean']['sha256'] == candidate.digest(artifact.read_bytes())


def test_capture_failure_telemetry_bounds_path_and_excludes_arbitrary_details():
    from trellis.checker import telemetry
    measurement = telemetry.Measurement()
    token = telemetry._current.set(measurement)
    try:
        telemetry.portable_capture(dict(status='unavailable', failure_detail=dict(
            path='x'*1000, changed_fields=['st_ctime_ns', 'not-a-stat-field'],
            stderr_tail='private output', artifact_contents='not telemetry')))
    finally:
        telemetry._current.reset(token)
    detail = measurement.snapshot()['portable_captures'][0]['failure_detail']
    assert detail == dict(path='x'*320, changed_fields=['st_ctime_ns'])

@pytest.mark.parametrize('point', ['before_hash', 'during_hash'])
@pytest.mark.parametrize('role', ['lake_compiler', None])
def test_protected_package_initial_link_churn_requires_matching_confirmation(
        tmp_path, imported, monkeypatch, point, role):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    baseline, baseline_stats = capture(repo)
    package = repo / '.lake/packages/dependency/.lake/build/lib/lean/External.olean'
    data = package.read_bytes()
    changed = []
    def link():
        if not changed:
            os.link(package, tmp_path / 'mirror.olean')
            changed.append(True)
    original_file = portable.HashBudget.file
    def file(budget, path, **kwargs):
        if path == package and point == 'before_hash': link()
        return original_file(budget, path, **kwargs)
    monkeypatch.setattr(portable.HashBudget, 'file', file)
    original_hash = portable.hashlib.sha256
    class Digest:
        def __init__(self, *args, **kwargs): self.inner = original_hash(*args, **kwargs)
        def update(self, value):
            self.inner.update(value)
            if value == data and point == 'during_hash': link()
        def hexdigest(self): return self.inner.hexdigest()
    monkeypatch.setattr(portable.hashlib, 'sha256', Digest)
    environment, stats = portable.capture(repo, ['A', 'Preamble'], timeout_secs=60, bwrap_role=role)
    assert changed == [True]
    if role == 'lake_compiler':
        assert environment == baseline, stats
        assert stats['files_hashed'] == baseline_stats['files_hashed'] + 1
        assert stats['bytes_hashed'] == baseline_stats['bytes_hashed'] + len(data)
        assert stats['content_revalidation_reads'] == 1
    else:
        assert environment is None and 'changed' in stats['reason']


@pytest.mark.parametrize('mutation', ['bytes', 'inode', 'mode', 'mtime', 'absent', 'alias'])
def test_protected_package_confirmation_rejects_real_change(tmp_path, monkeypatch, mutation):
    path = tmp_path / 'package.olean'
    path.write_bytes(b'original')
    before = path.stat()
    def change(_):
        os.link(path, tmp_path / 'link')
        if mutation == 'bytes':
            path.write_bytes(b'forgery!')
            os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
        elif mutation == 'inode':
            other = tmp_path / 'replacement'
            other.write_bytes(b'original')
            os.utime(other, ns=(before.st_atime_ns, before.st_mtime_ns))
            other.replace(path)
        elif mutation == 'mode': path.chmod(before.st_mode ^ 0o100)
        elif mutation == 'mtime': os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns + 1))
        else:
            path.unlink()
            if mutation == 'alias': path.symlink_to(tmp_path / 'link')
    _during_first_digest(monkeypatch, change)
    budget = portable.HashBudget()
    with pytest.raises((portable.Unavailable, OSError)):
        budget.file(path, initial_state=portable._file_state(before), protected_package=True)
    assert path not in budget.memo


@pytest.mark.parametrize('limit', ['reads', 'bytes', 'time'])
def test_protected_package_stabilization_has_one_shared_budget(tmp_path, monkeypatch, limit):
    path = tmp_path / 'package.olean'
    path.write_bytes(b'exact')
    budget = portable.HashBudget(max_bytes=5 if limit == 'bytes' else 100)
    def change(index):
        os.link(path, tmp_path / f'link-{index}')
        if limit == 'time': budget.deadline = 0
    count = _during_first_digest(monkeypatch, change, every=limit == 'reads')
    with pytest.raises(portable.CaptureUnavailable):
        budget.file(path, protected_package=True)
    assert count[0] == (3 if limit == 'reads' else 1)
    assert budget.bytes == count[0] * 5
    assert path not in budget.memo


def test_external_import_does_not_gain_package_stabilization(tmp_path, imported, monkeypatch):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    original_root = repo / '.lake/packages/dependency'
    external = tmp_path / 'external'
    shutil.move(original_root, external)
    package = external / '.lake/build/lib/lean/External.olean'
    original_inventory = obs._run_lake_command
    def inventory(*args, **kwargs):
        result = original_inventory(*args, **kwargs)
        result['stdout'] = result['stdout'].replace(str(original_root), str(external))
        return result
    monkeypatch.setattr(obs, '_run_lake_command', inventory)
    monkeypatch.setattr(obs, '_lake_managed_import_roots', lambda _: [repo, external])
    original_file = portable.HashBudget.file
    changed = []
    def file(budget, path, **kwargs):
        if path == package and not changed:
            assert kwargs['protected_package'] is False
            os.link(path, tmp_path / 'mirror.olean')
            changed.append(True)
        return original_file(budget, path, **kwargs)
    monkeypatch.setattr(portable.HashBudget, 'file', file)
    environment, stats = portable.capture(repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler')
    assert changed == [True] and environment is None
    assert stats['failure_class'] == 'environment_capture'


def _capture_cost_run(work):
    from trellis.checker import telemetry as tm
    state = tm.Measurement()
    token = tm._current.set(state)
    try:
        value = work()
        assert state.active_capture is None
        return value, state
    finally:
        tm._current.reset(token)


def _capture_cost_stable_stats(stats):
    return {key: value for key, value in stats.items()
            if key not in {'wall_secs', 'hash_wall_secs', 'inventory_wall_secs_including_queue'}}


def test_capture_cost_equivalent_io_hash_order_and_payload(tmp_path, imported, monkeypatch):
    from trellis.checker import telemetry as tm
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    events = []
    original_open, original_lstat, original_sha = Path.open, Path.lstat, portable.hashlib.sha256
    class Handle:
        def __init__(self, path, handle):
            self.path, self.handle = path, handle
        def __enter__(self):
            self.handle.__enter__()
            return self
        def __exit__(self, *args):
            events.append(('close', str(self.path)))
            return self.handle.__exit__(*args)
        def read(self, *args, **kwargs):
            events.append(('read', str(self.path), args, kwargs))
            return self.handle.read(*args, **kwargs)
    def opened(path, *args, **kwargs):
        events.append(('open', str(path), args, kwargs))
        return Handle(path, original_open(path, *args, **kwargs))
    def stated(path, *args, **kwargs):
        events.append(('lstat', str(path), args, kwargs))
        return original_lstat(path, *args, **kwargs)
    class Hash:
        def __init__(self, *args, **kwargs):
            events.append(('sha256', args, kwargs))
            self.inner = original_sha(*args, **kwargs)
        def update(self, data):
            events.append(('update', data))
            return self.inner.update(data)
        def hexdigest(self):
            events.append(('hexdigest',))
            return self.inner.hexdigest()
    monkeypatch.setattr(Path, 'open', opened)
    monkeypatch.setattr(Path, 'lstat', stated)
    monkeypatch.setattr(portable.hashlib, 'sha256', Hash)
    token = tm._current.set(None)
    try:
        cold = capture(repo)
        disabled_events = list(events)
        events.clear()
        warm, state = _capture_cost_run(lambda: capture(repo, telemetry_origin='replay_before'))
        enabled_events = list(events)
    finally:
        tm._current.reset(token)
    assert enabled_events == disabled_events
    assert warm[0] == cold[0]
    assert _capture_cost_stable_stats(warm[1]) == _capture_cost_stable_stats(cold[1])
    row = state.portable_captures[0]
    assert row['origin'] == 'replay_before'
    assert row['cost_partition_version'] == 1
    assert row['timed_read_attempts'] == row['files_hashed']
    assert row['timed_read_bytes'] == row['bytes_hashed']
    assert row['read_timing_complete'] and not row['timing_budget_exhausted']
    assert set(row['completed_phases']) == {'inventory', 'hash_phase', 'final_validation'}
    assert row['hash_phase_wall_ms'] >= row['final_validation_wall_ms']
    assert row['hash_phase_wall_ms'] >= row['read_binding_wall_ms']
    for forbidden in ('origin', 'cost_partition_version', 'timed_read_attempts'):
        assert forbidden not in warm[1] and forbidden not in warm[0]
    assert str(repo) not in json.dumps(row)
    assert 'environment_sha256' not in row


def test_capture_cost_partition_fake_clocks_and_revalidation(monkeypatch):
    from trellis.checker import telemetry as tm
    state = tm.Measurement()
    state.started_ns = 0
    clocks = [0, 0]
    monkeypatch.setattr(tm, 'time', SimpleNamespace(
        monotonic_ns=lambda: clocks[0] * 1000000,
        thread_time_ns=lambda: clocks[1] * 1000000))
    token = tm._current.set(state)
    try:
        capture_ticket = tm._capture_begin('donation_install')
        inventory = tm.capture_phase_begin('inventory')
        clocks[:] = [10, 2]
        tm.capture_phase_end(inventory)
        hashing = tm.capture_phase_begin('hash_phase')
        budget = SimpleNamespace(bytes=0)
        read = tm.capture_read_begin(budget, revalidation=False)
        clocks[:] = [15, 5]
        budget.bytes = 7
        tm.capture_read_end(read, budget, completed=True)
        validation = tm.capture_phase_begin('final_validation')
        read = tm.capture_read_begin(budget, revalidation=True)
        clocks[:] = [18, 6]
        budget.bytes += 11
        tm.capture_read_end(read, budget, completed=True)
        clocks[:] = [20, 7]
        tm.capture_phase_end(validation)
        clocks[:] = [24, 9]
        tm.capture_phase_end(hashing)
        tm.portable_capture(dict(status='captured'))
        tm._capture_restore(capture_ticket)
    finally:
        tm._current.reset(token)
    row = state.portable_captures[0]
    for prefix, wall, cpu in [('inventory', 10, 2), ('hash_phase', 14, 7),
                             ('final_validation', 5, 2), ('read_binding', 5, 3),
                             ('read_final_validation', 3, 1)]:
        assert row[prefix + '_wall_ms'] == wall
        assert row[prefix + '_thread_cpu_ms'] == cpu
    assert row['timed_read_bytes'] == 18
    assert row['timed_initial_reads'] == row['timed_revalidation_reads'] == 1
    assert row['started_ms'] == 0 and row['finished_ms'] == 24
    assert state.active_capture is None


def test_capture_cost_caps_reused_budget_and_memo(tmp_path, monkeypatch):
    from trellis.checker import telemetry as tm
    path = tmp_path / 'artifact'
    path.write_bytes(b'exact')
    budget = portable.HashBudget()
    monkeypatch.setattr(tm, '_CAPTURE_READ_LIMIT', 2)
    def work():
        @tm.capture_cost
        def measured(*, telemetry_origin='other'):
            expected = budget.file(path)
            budget.file(path)  # Memo hit must not consume a timing ticket.
            budget.file(path, expected=expected)
            tm.portable_capture(dict(status='captured'))
        measured(telemetry_origin='readonly_current')
        # A reusable budget must not carry diagnostic totals into this capture.
        measured(telemetry_origin='untrusted' * 1000)
        for _ in range(7):
            @tm.capture_cost
            def empty():
                tm.portable_capture(dict(status='unavailable'))
            empty()
    _, state = _capture_cost_run(work)
    first, second = state.portable_captures[:2]
    assert first['timed_read_attempts'] == 2
    assert first['timed_read_bytes'] == 10
    assert first['read_timing_complete']
    assert second['timed_read_attempts'] == second['timed_read_bytes'] == 0
    assert second['timing_budget_exhausted'] and not second['read_timing_complete']
    assert second['origin'] == 'other'
    assert len(state.portable_captures) == state.capture_detail_count == 8
    assert state.portable_capture_count == 9 and state.capture_read_count == 2
    assert len(json.dumps(state.portable_captures)) < 32768


@pytest.mark.parametrize('clock_name,clock_failure_at', [
    ('monotonic_ns', at) for at in (1, 2, 3, 4, 8, 16, 24)] + [
    ('thread_time_ns', at) for at in (1, 3, 8, 22)])
def test_capture_cost_clock_fault_keeps_result_and_context(tmp_path, imported, monkeypatch,
                                                         clock_name, clock_failure_at):
    from trellis.checker import telemetry as tm
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    expected = capture(repo)
    state = tm.Measurement()
    original_time = tm.time
    calls = []
    def clock():
        calls.append(1)
        if len(calls) == clock_failure_at:
            raise KeyboardInterrupt('diagnostic clock fault')
        return getattr(original_time, clock_name)()
    clock_proxy = SimpleNamespace(monotonic_ns=original_time.monotonic_ns,
                                 thread_time_ns=original_time.thread_time_ns)
    setattr(clock_proxy, clock_name, clock)
    monkeypatch.setattr(tm, 'time', clock_proxy)
    token = tm._current.set(state)
    try:
        actual = capture(repo)
        assert state.active_capture is None
    finally:
        tm._current.reset(token)
    assert len(calls) >= clock_failure_at
    assert state.failed
    assert actual[0] == expected[0]
    assert _capture_cost_stable_stats(actual[1]) == _capture_cost_stable_stats(expected[1])


def test_capture_cost_original_exception_nested_restore_and_read_failure(tmp_path, monkeypatch):
    from trellis.checker import telemetry as tm
    class Stop(BaseException):
        pass
    failure = Stop('original body failure')
    path = tmp_path / 'artifact'
    path.write_bytes(b'bytes')
    budget = portable.HashBudget()
    original_open = Path.open
    def fail_open(p, *args, **kwargs):
        if p == path:
            raise failure
        return original_open(p, *args, **kwargs)
    monkeypatch.setattr(Path, 'open', fail_open)
    def work():
        @tm.capture_cost
        def outer():
            parent = tm._current.get().active_capture
            @tm.capture_cost
            def inner():
                try:
                    budget.file(path)
                finally:
                    tm.portable_capture(dict(status='unavailable'))
            with pytest.raises(Stop) as caught:
                inner()
            assert caught.value is failure
            assert tm._current.get().active_capture is parent
            tm.portable_capture(dict(status='unavailable'))
        outer()
    _, state = _capture_cost_run(work)
    assert state.portable_captures[0]['timed_read_attempts'] == 1
    assert not state.portable_captures[0]['read_timing_complete']
    assert state.portable_captures[0]['timed_read_bytes'] == 0
    assert state.portable_captures[1]['timed_read_attempts'] == 0


def test_capture_cost_disabled_and_parallel_omit_detail(tmp_path, monkeypatch):
    from trellis.checker import telemetry as tm
    path = tmp_path / 'artifact'
    path.write_bytes(b'bytes')
    @tm.capture_cost
    def work():
        value = portable.HashBudget().file(path)
        tm.portable_capture(dict(status='captured'))
        return value
    disabled_clock_calls = []
    def forbidden_clock():
        disabled_clock_calls.append(True)
        raise AssertionError('disabled telemetry used a diagnostic clock')
    # With no owning measurement no clocks are consulted at any hook.
    with monkeypatch.context() as patch:
        patch.setattr(tm, 'time', SimpleNamespace(monotonic_ns=forbidden_clock,
                                                  thread_time_ns=forbidden_clock))
        expected = work()
    assert disabled_clock_calls == []
    parent = tm.Measurement()
    token = tm._current.set(parent)
    try:
        value, child = tm.parallel_call(lambda _: work(), None)
        tm.merge_parallel(child)
    finally:
        tm._current.reset(token)
    assert value == expected
    assert child.capture_read_count == child.capture_detail_count == 0
    assert child.omitted_parallel_capture_detail
    assert 'cost_partition_version' not in child.portable_captures[0]
    assert parent.snapshot()['portable_capture_detail_omitted_parallel']
    assert parent.snapshot()['portable_capture_detail_scope'] == 'request_thread_only'


def test_capture_cost_read_clock_count_is_per_file_not_chunk(tmp_path, monkeypatch):
    from trellis.checker import telemetry as tm
    path = tmp_path / 'artifact'
    path.write_bytes(b'x' * (3 * (1 << 20) + 17))
    budget = portable.HashBudget()
    state = tm.Measurement()
    counts = dict(wall=0, cpu=0)
    def clock(name):
        counts[name] += 1
        return counts[name]
    monkeypatch.setattr(tm, 'time', SimpleNamespace(monotonic_ns=lambda: clock('wall'),
                                                  thread_time_ns=lambda: clock('cpu')))
    token = tm._current.set(state)
    try:
        ticket = tm._capture_begin('other')
        counts.update(wall=0, cpu=0)
        budget.file(path)
        assert counts == dict(wall=2, cpu=2)
        budget.file(path)
        assert counts == dict(wall=2, cpu=2)
        tm._capture_restore(ticket)
    finally:
        tm._current.reset(token)


@pytest.mark.parametrize('stage', ['inventory', 'final_validation'])
def test_capture_cost_failure_stages_keep_original_exception_and_restore(tmp_path, imported, monkeypatch, stage):
    from trellis.checker import telemetry as tm
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    class Stop(BaseException):
        pass
    failure = Stop('body exception remains exact')
    def fail(*args, **kwargs):
        raise failure
    if stage == 'inventory':
        monkeypatch.setattr(obs, '_run_lake_command', fail)
    else:
        monkeypatch.setattr(portable._CapturePaths, 'validate', fail)
    state = tm.Measurement()
    token = tm._current.set(state)
    try:
        with pytest.raises(Stop) as caught:
            capture(repo)
        assert caught.value is failure
        assert state.active_capture is None
    finally:
        tm._current.reset(token)
    row = state.portable_captures[0]
    assert stage + '_wall_ms' in row
    assert stage not in row['completed_phases']
    assert 'hash_phase' not in row['completed_phases']


def test_capture_cost_caught_binding_failure_equivalent(tmp_path, imported, monkeypatch):
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    failure = portable.Unavailable('import resolution changed during content capture')
    def fail(*args):
        raise failure
    monkeypatch.setattr(portable._CapturePaths, 'validate', fail)
    expected = portable.capture(repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler')
    actual, state = _capture_cost_run(lambda: portable.capture(
        repo, ['A'], timeout_secs=60, bwrap_role='lake_compiler'))
    assert actual[0] is expected[0] is None
    assert _capture_cost_stable_stats(actual[1]) == _capture_cost_stable_stats(expected[1])
    row = state.portable_captures[0]
    assert row['reason'] == str(failure)
    assert 'final_validation' not in row['completed_phases']


def test_capture_cost_donation_and_readonly_origins_are_diagnostic_only(donor):
    import threading
    _source, _destination, workspace, _invalidated = donor
    installed, install_state = _capture_cost_run(lambda: donation.install(workspace))
    assert installed['status'] == 'installed'
    assert [row['origin'] for row in install_state.portable_captures] == ['donation_install']
    assert all('cost_partition_version' not in row for row in installed['captures'])
    workspace._workspace_lock = threading.RLock()
    workspace._sync_lock = threading.RLock()
    observed, current_state = _capture_cost_run(lambda: portable.capture_current(workspace, ['A']))
    assert observed['environment'] is not None
    assert [row['origin'] for row in current_state.portable_captures] == ['readonly_current']
    assert 'cost_partition_version' not in observed['measurement']


def test_capture_cost_partial_read_exception_and_export_failure(tmp_path, imported, monkeypatch):
    from trellis.checker import telemetry as tm
    path = tmp_path / 'partial'
    path.write_bytes(b'abcdef')
    failure = OSError('read failed after one chunk')
    original_open = Path.open
    class Broken:
        def __enter__(self): return self
        def __exit__(self, *args): return False
        def read(self, *args):
            if not hasattr(self, 'read_once'):
                self.read_once = True
                return b'abc'
            raise failure
    def opened(p, *args, **kwargs):
        return Broken() if p == path else original_open(p, *args, **kwargs)
    monkeypatch.setattr(Path, 'open', opened)
    budget = portable.HashBudget()
    def work():
        @tm.capture_cost
        def read():
            try:
                with pytest.raises(OSError) as caught:
                    budget.file(path)
                assert caught.value is failure
            finally:
                tm.portable_capture(dict(status='unavailable'))
        read()
    _, state = _capture_cost_run(work)
    row = state.portable_captures[0]
    assert row['timed_read_bytes'] == budget.bytes == 3
    assert row['timed_read_attempts'] == 1 and not row['read_timing_complete']
    setup, _ = imported
    repo = setup(tmp_path / 'repo')
    expected = capture(repo)
    broken_export = tm.Measurement()
    broken_export.portable_captures = None  # Fail inside the contained emitter.
    token = tm._current.set(broken_export)
    try:
        actual = capture(repo)
        assert broken_export.failed and broken_export.active_capture is None
    finally:
        tm._current.reset(token)
    assert actual[0] == expected[0]
    assert _capture_cost_stable_stats(actual[1]) == _capture_cost_stable_stats(expected[1])


def test_fact_donation_reads_exact_archived_host_response(donor, monkeypatch):
    from trellis.dispatch import retained_files as retained
    monkeypatch.setattr(retained, 'MIN_BYTES', 1)
    _, _, workspace, _ = donor
    previous, marker, completed = donation._marker(workspace.runtime_root)
    path = previous / 'execution/response.json'
    response = dict(validation_fact_receipt=dict(schema_version=3, artifact_owners=['A']),
                    validation_step_results=[dict(ok=True)], diagnostic='repeat ' * 1000)
    put(path, response)
    completed['files']['execution/response.json'] = candidate.digest(path.read_bytes())
    put(previous / 'completed.json', completed)
    marker['donor_completion_sha256'] = candidate.digest((previous / 'completed.json').read_bytes())
    put(workspace.runtime_root.parent / 'replay-cache-donor.json', marker)
    witness = dict(version=1, status='installed', covered_nodes=['A'], artifact_files={},
                   donor_round='1', donor_completion_sha256=marker['donor_completion_sha256'])
    before = donation.validation_fact_donor(workspace.runtime_root, witness)
    assert before is not None
    assert retained.archive(path)
    assert donation.validation_fact_donor(workspace.runtime_root, witness) == before
    archive = retained._paths(path)[0]
    archive.write_bytes(archive.read_bytes()[:-8])
    assert donation.validation_fact_donor(workspace.runtime_root, witness) is None

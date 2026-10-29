import json
from pathlib import Path
import shutil
import subprocess

import pytest

from trellis.dispatch.candidate import CandidateError, digest, source_manifest
from trellis.dispatch.workspace import prepare


def git(repo, *args):
    subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)


@pytest.fixture
def project(tmp_path):
    repo, runtime = tmp_path / 'repo', tmp_path / 'runtime'
    (repo / 'Tablet').mkdir(parents=True)
    runtime.mkdir()
    (repo / 'Tablet/A.lean').write_text('old')
    (repo / 'Tablet/Removed.lean').write_text('removed later')
    (repo / 'GOAL.md').write_text('goal')
    config = repo / 'trellis.config.json'
    config.write_text(json.dumps(dict(repo_path=str(repo), goal_file=str(repo / 'GOAL.md'),
        workflow={'input_request_path': str(repo / 'INPUT_REQUEST.md')})))
    git(repo, 'init')
    git(repo, 'config', 'user.name', 'Test')
    git(repo, 'config', 'user.email', 'test@example.invalid')
    git(repo, 'add', '.')
    git(repo, 'commit', '-m', 'base')
    (repo / 'Tablet/A.lean').write_text('current accepted source')
    (repo / 'Tablet/Removed.lean').unlink()
    (repo / 'Tablet/Helper.lean').write_text('untracked accepted helper')
    (repo / '.lake/build').mkdir(parents=True)
    (repo / '.lake/build/project.olean').write_bytes(b'build cache')
    (runtime / 'runtime_metadata.json').write_text(json.dumps(dict(repo_path=str(repo), config_path=str(config))))
    return repo, runtime, config


def test_workspace_captures_current_sources_and_never_resets_adopted_attempt(project):
    repo, runtime, config = project
    handoff = repo / '.trellis/scratch/reviewer-plan.md'
    handoff.parent.mkdir(parents=True)
    handoff.write_text('Existing reviewer handoff referenced by the worker contract.')
    state = {'in_flight_request': {'id': 7, 'reviewer_comments': 'Read `.trellis/scratch/reviewer-plan.md`.'}}
    before = source_manifest(repo)
    target = runtime / 'dispatch/1'
    prepare(runtime, repo, config, target, state, expected_manifest=before, seed_packages=False)
    private = target / 'repo'
    copied_handoff = private / '.trellis/scratch/reviewer-plan.md'
    assert copied_handoff.read_text() == handoff.read_text()
    assert copied_handoff.stat().st_ino != handoff.stat().st_ino
    copied_handoff.write_text('Private worker notes.')
    assert source_manifest(private) == before
    assert not (private / 'Tablet/Removed.lean').exists()
    copied = json.loads((private / 'trellis.config.json').read_text())
    assert copied['goal_file'] == str(private / 'GOAL.md')
    assert copied['workflow']['input_request_path'] == str(private / 'INPUT_REQUEST.md')
    metadata = json.loads((target / 'runtime/runtime_metadata.json').read_text())
    assert metadata['repo_path'] == str(private)
    assert metadata['config_path'] == str(private / 'trellis.config.json')
    cache = private / '.lake/build/project.olean'
    assert cache.stat().st_ino != (repo / '.lake/build/project.olean').stat().st_ino
    cache.write_bytes(b'private rebuild')
    (private / 'Tablet/A.lean').write_text('worker edits')
    prepare(runtime, repo, config, target, state, expected_manifest=before, seed_packages=False)
    assert (private / 'Tablet/A.lean').read_text() == 'worker edits'
    assert copied_handoff.read_text() == 'Private worker notes.'
    assert handoff.read_text() == 'Existing reviewer handoff referenced by the worker contract.'
    assert (repo / '.lake/build/project.olean').read_bytes() == b'build cache'
    with pytest.raises(RuntimeError, match='different prepared inputs'):
        prepare(runtime, repo, config, target, {'in_flight_request': {'id': 8}}, expected_manifest=before, seed_packages=False)


def test_workspace_refuses_stale_sources_and_unreceipted_destination(project):
    repo, runtime, config = project
    state = {'in_flight_request': {'id': 7}}
    before = source_manifest(repo)
    (repo / 'Tablet/A.lean').write_text('concurrent mutation')
    target = runtime / 'dispatch/1'
    with pytest.raises(CandidateError, match='changed before'):
        prepare(runtime, repo, config, target, state, expected_manifest=before, seed_packages=False)
    target.mkdir(parents=True)
    with pytest.raises(RuntimeError, match='without a preparation receipt'):
        prepare(runtime, repo, config, target, state, expected_manifest=source_manifest(repo), seed_packages=False)


@pytest.mark.parametrize('kind', ['Review', 'Sound', 'Worker'])
@pytest.mark.parametrize('canonical_marker_present', [False, True])
def test_tracked_deployment_stop_is_not_inherited_but_private_breaker_survives(
        project, kind, canonical_marker_present):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver

    repo, runtime, config = project
    sentinel = '.trellis-stop-after-checkpoint'
    canonical_stop = repo / sentinel
    canonical_stop.write_text('old deployment drain\n')
    git(repo, 'add', sentinel)
    git(repo, 'commit', '-m', 'checkpoint during deployment drain')
    if not canonical_marker_present:
        canonical_stop.unlink()  # Ordinary resume removes the worktree marker.
    state = {'in_flight_request': {'id': 7, 'kind': kind}}
    manifest = source_manifest(repo)
    target = runtime / 'dispatch/1'
    prepare(runtime, repo, config, target, state, expected_manifest=manifest, seed_packages=False)
    private_stop = target / 'repo' / sentinel
    assert not private_stop.exists()
    assert canonical_stop.exists() == canonical_marker_present
    assert source_manifest(target / 'repo') == manifest

    driver = ProductionDriver.__new__(ProductionDriver)
    driver.root = runtime / 'dispatch'
    driver.native = SimpleNamespace(locked=nullcontext)
    pauses = []
    driver._pause = lambda *args, **kwargs: pauses.append((args, kwargs))
    response = {'status': 'Ok'}
    driver._record_transport(target, response)
    assert pauses == []

    # A real breaker created by this attempt is retained and still propagated,
    # including after preparation reuse and an already recorded completion.
    private_stop.write_text('[bridge circuit-breaker] actual transport failure\n')
    receipt = (target / 'prepared.json').read_bytes()
    prepare(runtime, repo, config, target, state, expected_manifest=manifest, seed_packages=False)
    assert (target / 'prepared.json').read_bytes() == receipt
    driver._record_transport(target, response)
    assert pauses == [(('bridge_circuit_breaker', private_stop.read_text()), {})]


def test_retry_sees_preserved_failed_work_without_installing_it(project):
    repo, runtime, config = project
    before = source_manifest(repo)
    old = runtime / 'dispatch/1'
    (old / 'repo/Tablet').mkdir(parents=True)
    (old / 'repo/Tablet/A.lean').write_text('failed proof attempt')
    (old / 'execution').mkdir()
    report = dict(kind='worker', request_id=8, cycle=3, status='Ok', outcome='Invalid',
                  invalid_kind='checker_rejected', summary='Finished the assigned proof.',
                  comments='Final certification failed for another node.',
                  deterministic_rejection_reasons=['checker identity unavailable'],
                  snapshot={'large_state': 'must not enter the handoff'})
    response = json.dumps(report).encode()
    (old / 'execution/response.json').write_bytes(response)
    (old / 'execution/result.json').write_text(json.dumps(dict(returncode=0, response_sha256=digest(response))))
    target = runtime / 'dispatch/2'
    prepare(runtime, repo, config, target, {'in_flight_request': {'id': 9}},
            expected_manifest=before, previous=old, seed_packages=False)
    assert source_manifest(target / 'repo') == before
    assert (target / 'repo/last_invalid/Tablet/A.lean').read_text() == 'failed proof attempt'
    metadata = json.loads((target / 'repo/last_invalid/metadata.json').read_text())
    assert metadata['previous_task'] == '1'
    assert metadata['report_available'] is True
    for key in ('request_id', 'cycle', 'status', 'outcome', 'invalid_kind', 'summary', 'comments',
                'deterministic_rejection_reasons'):
        assert metadata[key] == report[key]
    assert metadata['response_sha256'] == digest(response)
    assert metadata['preserved_source_surfaces'] == {'Tablet': True, 'reference': False}
    assert 'snapshot' not in metadata
    assert source_manifest(repo) == before
    (target/'repo/Tablet/A.lean').write_text('new retry work')
    (old/'execution/response.json').write_text('changed after preparation')
    prepare(runtime, repo, config, target, {'in_flight_request': {'id': 9}},
            expected_manifest=before, previous=old, seed_packages=False)
    assert (target/'repo/Tablet/A.lean').read_text() == 'new retry work'
    assert json.loads((target/'repo/last_invalid/metadata.json').read_text()) == metadata


@pytest.mark.parametrize('failure', ['missing', 'mismatch', 'oversized', 'symlink', 'malformed'])
def test_retry_report_failure_preserves_sources_with_explicit_missing_report(tmp_path, monkeypatch, failure):
    from trellis.dispatch import workspace
    old, snapshot = tmp_path/'old', tmp_path/'last_invalid'
    (old/'execution').mkdir(parents=True)
    (snapshot/'Tablet').mkdir(parents=True)
    (snapshot/'Tablet/A.lean').write_text('useful prior proof')
    response = json.dumps(dict(kind='worker', outcome='Invalid', summary='prior work')).encode()
    if failure == 'malformed':
        response = b'not JSON'
    (old/'execution/response.json').write_bytes(response)
    (old/'execution/result.json').write_text(json.dumps(dict(
        response_sha256='wrong' if failure == 'mismatch' else digest(response))))
    if failure == 'missing':
        (old/'execution/result.json').unlink()
    elif failure == 'oversized':
        monkeypatch.setattr(workspace, '_RETRY_RESPONSE_LIMIT', 16)
    elif failure == 'symlink':
        (old/'execution/response.json').unlink()
        outside = tmp_path/'outside.json'; outside.write_bytes(response)
        (old/'execution/response.json').symlink_to(outside)
    workspace._retry_metadata(old, snapshot)
    metadata = json.loads((snapshot/'metadata.json').read_text())
    assert metadata['report_available'] is False
    assert metadata['report_unavailable_reason']
    assert 'summary' not in metadata
    assert (snapshot/'Tablet/A.lean').read_text() == 'useful prior proof'


def test_retry_report_bounds_context_and_marks_truncation(tmp_path):
    from trellis.dispatch.workspace import _retry_metadata
    old, snapshot = tmp_path/'old', tmp_path/'last_invalid'
    (old/'execution').mkdir(parents=True)
    response = json.dumps(dict(kind='worker', outcome='Valid', summary='s'*30000,
        comments='c'*30000, deterministic_rejection_reasons=['r'*8000]*20,
        snapshot={'irrelevant_state': 'x'*100000})).encode()
    (old/'execution/response.json').write_bytes(response)
    (old/'execution/result.json').write_text(json.dumps(dict(response_sha256=digest(response))))
    _retry_metadata(old, snapshot)
    data = (snapshot/'metadata.json').read_bytes()
    metadata = json.loads(data)
    assert len(data) < 65536
    assert metadata['outcome'] == 'Valid'  # A worker claim is not acceptance evidence.
    assert metadata['truncated_fields'] == ['comments', 'deterministic_rejection_reasons', 'summary']
    assert 'snapshot' not in metadata and 'accepted' not in metadata


def test_retry_report_uses_latest_completed_submission_as_unaccepted_context(tmp_path):
    from trellis.dispatch.workspace import _retry_metadata
    old, snapshot = tmp_path / 'old', tmp_path / 'last_invalid'
    for turn in (1, 2):
        execution = old / 'turns' / str(turn) / 'execution'
        execution.mkdir(parents=True)
        response = json.dumps(dict(kind='worker_submission', request_id=9, cycle=4, turn=turn,
            raw=dict(outcome='Valid', summary=f'proof turn {turn}'), provider_session_id='private')).encode()
        (execution / 'response.json').write_bytes(response)
        (execution / 'result.json').write_text(json.dumps(dict(response_sha256=digest(response))))
    (old / 'turns/3/execution').mkdir(parents=True)
    _retry_metadata(old, snapshot)
    metadata = json.loads((snapshot / 'metadata.json').read_text())
    assert metadata['report_available'] and metadata['unaccepted']
    assert metadata['submission_turn'] == 2 and metadata['summary'] == 'proof turn 2'
    assert metadata['request_id'] == 9 and metadata['cycle'] == 4
    assert 'private' not in json.dumps(metadata)


def test_workspace_refuses_external_source_symlink(project, tmp_path):
    repo, runtime, config = project
    secret = tmp_path / 'external'
    secret.write_text('not an input')
    (repo / 'EXTERNAL').symlink_to(secret)
    with pytest.raises(CandidateError, match='symlinks'):
        prepare(runtime, repo, config, runtime / 'dispatch/1', {'in_flight_request': {'id': 7}},
                expected_manifest=source_manifest(repo), seed_packages=False)
    assert not (runtime / 'dispatch/1').exists()


@pytest.mark.parametrize('parent_link', [False, True])
def test_workspace_rejects_handoff_directory_symlinks(project, tmp_path, parent_link):
    repo, runtime, config = project
    outside = tmp_path / 'outside'; outside.mkdir()
    (outside / 'secret.md').write_text('not a handoff')
    if parent_link:
        (repo / '.trellis').symlink_to(outside, target_is_directory=True)
    else:
        (repo / '.trellis').mkdir()
        (repo / '.trellis/scratch').symlink_to(outside, target_is_directory=True)
    with pytest.raises((CandidateError, ValueError), match='symlinks|Cannot read reviewer note'):
        prepare(runtime, repo, config, runtime / 'dispatch/1', {'in_flight_request': {
            'id': 7, 'reviewer_comments': 'Read .trellis/scratch/secret.md'}},
                expected_manifest=source_manifest(repo), seed_packages=False)
    assert not (runtime / 'dispatch/1').exists()


def test_workspace_prepares_with_advisory_handoff_directory_reference(project):
    repo, runtime, config = project
    review_repo = runtime / 'dispatch/review-308/repo'
    relative = '.trellis/scratch/worker-handoffs/task-12'
    notes = review_repo / relative
    notes.mkdir(parents=True)
    (notes / 'unrequested.json').write_text('{"advisory": true}')
    request = dict(id=309, reviewer_comments=f'Consult `{relative}` for earlier work.')
    target = runtime / 'dispatch/13'
    prepare(runtime, repo, config, target, {'in_flight_request': request},
            expected_manifest=source_manifest(repo), handoff_repo=review_repo, seed_packages=False)
    prepared = json.loads((target / 'prepared.json').read_text())
    assert prepared['missing_reviewer_handoffs'] == [
        relative + ' (directory reference; name an exact advisory file)']
    assert prepared['reviewer_handoffs'] == {}
    assert not (target / 'repo' / relative).exists()
    assert json.loads((target / 'runtime/protocol_state.json').read_text())['in_flight_request'] == request


@pytest.mark.parametrize('missing', [False, True])
def test_workspace_handoff_uses_frozen_review_then_survives_fresh_worker_reset(project, missing):
    from trellis.runtime.bridge import _prepare_worker_support_files
    repo, runtime, config = project
    relative = '.trellis/scratch/plan.md'
    canonical = repo / relative; canonical.parent.mkdir(parents=True)
    canonical.write_text('Wrong older reviewer instructions')
    review_repo = runtime / 'dispatch/review-91/repo'
    note = review_repo / relative; note.parent.mkdir(parents=True)
    if not missing:
        note.write_text('Instructions from the assigned reviewer')
    (note.parent / 'unreferenced.lean').write_text('temporary reviewer probe')
    request = dict(id=92, fresh_context=True, reviewer_comments=f'Read `{relative}`.')
    target = runtime / 'dispatch/2'
    prepare(runtime, repo, config, target, {'in_flight_request': request},
        expected_manifest=source_manifest(repo), handoff_repo=review_repo, seed_packages=False)
    private = target / 'repo'
    result = _prepare_worker_support_files(repo_path=private, request=request)
    assert not (private / '.trellis/scratch/unreferenced.lean').exists()
    if missing:
        assert not (private / relative).exists()
        assert 'Reviewer notes unavailable: ' + relative in result['reviewer_handoff_status_text']
    else:
        assert (private / relative).read_text() == note.read_text()
        assert (private / relative).stat().st_ino != note.stat().st_ino
        assert 'retained the files referenced' in result['scratch_workspace_status_text']


@pytest.fixture
def trusted_build(project):
    from trellis.atomic_actions import observations as obs
    repo, runtime, config = project
    (repo / 'lean-toolchain').write_text('leanprover/lean4:v4.30.0\n')
    (repo / '.trellis/scripts').mkdir(parents=True)
    (repo / '.trellis/scripts/check.py').write_text('# test checker\n')
    for name, source in {'Preamble': 'import Init\n',
                         'A': 'import Tablet.Preamble\n',
                         'B': 'import Tablet.A\n'}.items():
        (repo / f'Tablet/{name}.lean').write_text(source)
    authority = repo / '.trellis/supervisor/repo'
    shutil.copytree(repo / 'Tablet', authority / 'Tablet')
    shutil.copy2(repo / 'lean-toolchain', authority / 'lean-toolchain')
    shutil.copytree(repo / '.trellis/scripts', authority / '.trellis/scripts')
    build = authority / '.lake/build/lib/lean/Tablet'
    build.mkdir(parents=True)
    ir = authority / '.lake/build/ir/Tablet'
    ir.mkdir(parents=True)
    for name in ('Preamble', 'A', 'B'):
        (build / f'{name}.olean').write_bytes(f'trusted unit-test bundle {name}'.encode())
        (ir / f'{name}.c').write_text(f'/* generated cache for {name} */\n')
        (ir / f'{name}.c.hash').write_text('lake-cache-hash')
        (ir / f'{name}.o').write_bytes(b'native object must not be transferred')
        obs.write_olean_srcclosure(authority, name, obs.tablet_source_closure_hash(authority, name))
        assert obs._write_kernel_replay_attestation(authority, name, olean_sha256='')
    return repo, runtime, config, authority


def seed_attempt(trusted_build):
    repo, runtime, config, authority = trusted_build
    target = runtime / 'dispatch/1'
    prepare(runtime, repo, config, target, {'in_flight_request': {'id': 7}},
            expected_manifest=source_manifest(repo), seed_packages=False)
    return target, target / 'repo/.trellis/supervisor/repo'


@pytest.mark.parametrize('warm_build', [False, True])
def test_sound_verifier_prepares_host_packages_even_without_a_build_seed(trusted_build, warm_build):
    repo, runtime, config, authority = trusted_build
    if not warm_build:
        shutil.rmtree(authority)
    package = repo / '.lake/packages/mathlib'
    contents = {
        'lakefile.lean': b'package mathlib',
        'Mathlib.lean': b'import Init',
        '.lake/config/lakefile.olean': b'prewarmed package configuration',
        '.lake/build/lib/lean/Mathlib.olean': b'prewarmed package artifact',
    }
    for relative, data in contents.items():
        path = package / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    (package / 'doc.html').symlink_to('missing-generated-doc.html')
    state = {'in_flight_request': {'id': 7, 'kind': 'sound', 'nodes': ['A']}}
    manifest = source_manifest(repo)
    target = runtime / 'dispatch/verifier-3'
    prepare(runtime, repo, config, target, state, expected_manifest=manifest)
    private = target / 'repo'
    for packages in (private / '.lake/packages',
                     private / '.trellis/supervisor/repo/.lake/packages'):
        for relative, data in contents.items():
            path = packages / 'mathlib' / relative
            assert path.read_bytes() == data
            assert path.stat().st_ino == (package / relative).stat().st_ino
        assert (packages / 'mathlib/doc.html').is_symlink()
    receipt = json.loads((target / 'prepared.json').read_text())
    assert receipt['seeded_oleans'] == (3 if warm_build else 0)
    assert source_manifest(private) == manifest
    assert json.loads((target / 'runtime/protocol_state.json').read_text()) == state
    # Re-adopting a prepared verifier does not rewrite its workspace.
    note = private / 'verifier-notes.txt'
    note.write_text('in-progress notes')
    prepare(runtime, repo, config, target, state, expected_manifest=manifest)
    assert note.read_text() == 'in-progress notes'


def test_workspace_durability_prunes_only_package_mirrors(tmp_path, monkeypatch):
    from trellis.dispatch.workspace import sync_tree
    from trellis.dispatch import candidate
    root = tmp_path / 'workspace'
    source = tmp_path / 'cached.olean'
    source.write_bytes(b'existing cache bytes')
    shared = []
    for relative in ('repo/.lake/packages/mathlib/Mathlib.olean',
                     'repo/.trellis/supervisor/repo/.lake/packages/mathlib/Mathlib.olean'):
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.hardlink_to(source)
        shared.append(path)
    copied = shared[-1].with_name('Copied.olean')
    shutil.copy2(source, copied)
    private = []
    for relative in ('repo/Tablet/A.lean', 'repo/.lake/build/Tablet/A.olean',
                     'repo/.trellis/supervisor/repo/.lake/build/Tablet/A.olean.srcclosure',
                     'repo/.trellis/kernel-replay-environments/environment.json',
                     'runtime/protocol_state.json', 'base-manifest.json'):
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b'durable input or evidence')
        private.append(path)
    opened = []
    synced = []
    original_open = Path.open

    def track_open(path, *args, **kwargs):
        opened.append(path)
        return original_open(path, *args, **kwargs)

    monkeypatch.setattr(Path, 'open', track_open)
    monkeypatch.setattr(candidate, '_sync_dir', synced.append)
    sync_tree(root)
    assert all(path not in opened for path in [*shared, copied])
    assert all(path in opened for path in private)
    assert all(not path.is_relative_to(package.parent) for path in synced for package in shared)
    assert root in synced and root / 'repo/.lake' in synced
    assert all(path.parent in synced for path in private)


def test_new_grant_repairs_crash_lost_package_entries_without_resetting_sources(project, monkeypatch):
    from trellis.dispatch import resources
    repo, runtime, config = project
    (repo / 'lake-manifest.json').write_text('{"packages":[]}')
    (repo / 'lean-toolchain').write_text('leanprover/lean4:v4.24.0')
    package = repo / '.lake/packages/mathlib'
    for relative in ('Mathlib/Basic.lean', '.lake/build/lib/lean/Mathlib/Basic.olean', '.lake/config/lakefile.olean'):
        path = package / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b'host package bytes')
    target = runtime / 'dispatch/1/acceptance/1/finalization/1'
    prepare(runtime, repo, config, target, {'in_flight_request': {'id': 7}},
            expected_manifest=source_manifest(repo))
    mirrors = [target / 'repo/.lake/packages', target / 'repo/.trellis/supervisor/repo/.lake/packages']
    shutil.rmtree(mirrors[0] / 'mathlib/Mathlib')
    (mirrors[1] / 'mathlib/.lake/build/lib/lean/Mathlib/Basic.olean').unlink()
    original = {name: (target / name).read_bytes() for name in ('prepared.json', 'base-manifest.json', 'runtime/protocol_state.json')}
    (target / 'repo/Tablet/A.lean').write_text('retained proof edit')
    registered = []
    def register(*args, **kwargs):
        for mirror in mirrors:
            for relative in ('Mathlib/Basic.lean', '.lake/build/lib/lean/Mathlib/Basic.olean', '.lake/config/lakefile.olean'):
                restored = mirror / 'mathlib' / relative
                assert restored.read_bytes() == b'host package bytes'
                assert restored.stat().st_ino == (package / relative).stat().st_ino
        registered.append(True)
        return {}
    monkeypatch.setattr(resources.workspaces, 'register', register)
    root = target / 'execution'
    root.mkdir()
    spec = dict(runtime=str(runtime), task_runtime=str(target / 'runtime'), compiler='none', lifetime=None, scope='finalizer')
    resources.grant(root, spec)
    assert registered == [True]
    assert (target / 'repo/Tablet/A.lean').read_text() == 'retained proof edit'
    assert all((target / name).read_bytes() == value for name, value in original.items())
    # An already issued channel is adopted, never repaired/reissued in place.
    monkeypatch.setattr('trellis.dispatch.workspace.repair_packages', lambda *_: pytest.fail('repaired an issued workspace'))
    with pytest.raises(RuntimeError, match='already issued'):
        resources.grant(root, spec)


@pytest.mark.parametrize('change', ['source_pin', 'private_pin', 'source_root'])
def test_package_repair_refuses_changed_trusted_inputs_before_grant(project, monkeypatch, change):
    from trellis.dispatch import resources
    repo, runtime, config = project
    (repo / 'lake-manifest.json').write_text('{"packages":[]}')
    target = runtime / 'dispatch/1'
    prepare(runtime, repo, config, target, {'in_flight_request': {'id': 7}}, expected_manifest=source_manifest(repo))
    if change == 'source_root':
        (runtime / 'runtime_metadata.json').write_text(json.dumps(dict(repo_path=str(repo.parent / 'different'))))
    else:
        ((repo if change == 'source_pin' else target / 'repo') / 'lake-manifest.json').write_text('{"changed":true}')
    monkeypatch.setattr(resources.workspaces, 'register', lambda *a, **k: pytest.fail('granted changed package inputs'))
    with pytest.raises(CandidateError, match='source or pins changed'):
        resources.grant(target / 'execution', dict(runtime=str(runtime), task_runtime=str(target / 'runtime'),
            compiler='none', lifetime=None, scope='worker'))
    assert not (target / 'execution/compiler-channels.json').exists()


def test_workspace_package_seed_cannot_escape_private_supervisor(project, tmp_path, monkeypatch):
    from trellis import supervisor_workspace
    repo, runtime, config = project
    outside = tmp_path / 'outside'
    monkeypatch.setattr(supervisor_workspace, 'authoritative_repo_path', lambda _: outside)
    package = repo / '.lake/packages/mathlib'
    package.mkdir(parents=True)
    (package / 'Mathlib.lean').write_text('import Init')
    target = runtime / 'dispatch/verifier-3'
    with pytest.raises(CandidateError, match='must stay inside the attempt'):
        prepare(runtime, repo, config, target, {'in_flight_request': {'id': 7, 'kind': 'sound'}},
                expected_manifest=source_manifest(repo))
    assert not outside.exists()
    assert not target.exists()


def test_authoritative_seed_copies_only_trusted_bundles_and_their_replay_evidence(trusted_build):
    from trellis.atomic_actions import observations as obs
    repo, _, _, authority = trusted_build
    worker_build = repo / '.lake/build/lib/lean/Tablet'
    worker_build.mkdir(parents=True)
    (worker_build / 'A.olean').write_bytes(b'worker-controlled cache must never seed authority')
    target, private = seed_attempt(trusted_build)
    assert json.loads((target / 'prepared.json').read_text())['seeded_oleans'] == 3
    for name in ('Preamble', 'A', 'B'):
        src = obs._tablet_olean_path(authority, name)
        dst = obs._tablet_olean_path(private, name)
        assert dst.read_bytes() == src.read_bytes()
        assert dst.stat().st_ino != src.stat().st_ino
        assert obs.olean_is_content_current(private, name)
        assert obs.olean_has_current_kernel_replay(private, name)
        assert obs.olean_has_current_kernel_replay(authority, name)
        ir = private / '.lake/build/ir/Tablet'
        assert (ir / f'{name}.c').is_file()
        assert (ir / f'{name}.c.hash').is_file()
        assert not (ir / f'{name}.o').exists()
    obs._tablet_olean_path(private, 'A').write_bytes(b'private compilation')
    assert obs._tablet_olean_path(authority, 'A').read_bytes() != b'private compilation'


@pytest.mark.parametrize('manifest', ['current', 'missing', 'changed', 'symlink'])
def test_authoritative_seed_preserves_referenced_portable_environments(trusted_build, tmp_path, monkeypatch, manifest):
    from trellis.atomic_actions import observations as obs
    from trellis.checker import portable_replay as portable
    authority = trusted_build[3]
    environment = dict(version=portable.VERSION, fixture='inherited dependency replay')
    identity = portable.digest(environment)
    source = authority / portable.DIRECTORY / (identity + '.json')
    source.parent.mkdir(parents=True)
    if manifest != 'missing':
        source.write_bytes(portable._json(environment if manifest != 'changed' else
                                          dict(environment, fixture='different environment')))
    if manifest == 'symlink':
        outside = tmp_path / 'aliased-environment.json'
        source.rename(outside)
        source.symlink_to(outside)
    for name in ('Preamble', 'A'):
        record = obs._read_olean_provenance(authority, name)
        record['kernel_replay']['portable_environment_sha256'] = identity
        assert obs._write_olean_provenance(authority, name, record)
    # Unreferenced manifests are not copied wholesale.
    unused = dict(version=portable.VERSION, fixture='unused')
    (source.parent / (portable.digest(unused) + '.json')).write_bytes(portable._json(unused))
    lookups = []
    original_lookup = portable.environment_bytes_for
    def lookup(repo, identity, **kwargs):
        lookups.append(identity)
        return original_lookup(repo, identity, **kwargs)
    monkeypatch.setattr(portable, 'environment_bytes_for', lookup)
    target, private = seed_attempt(trusted_build)
    assert lookups == [identity]
    assert json.loads((target / 'prepared.json').read_text())['seeded_oleans'] == 3
    copied = private / portable.DIRECTORY / source.name
    if manifest == 'current':
        assert portable.environment_for(private, identity) == environment
        assert copied.stat().st_ino != source.stat().st_ino
        assert list(copied.parent.iterdir()) == [copied]
        copied.write_bytes(b'private edit')
        assert portable.environment_for(authority, identity) == environment
    else:
        assert not copied.exists()
    # Missing optional portability does not discard otherwise coherent seeds.
    assert obs.olean_has_current_kernel_replay(private, 'A')


@pytest.mark.parametrize('change', ['source', 'bundle', 'private_part', 'missing_attestation', 'toolchain'])
def test_authoritative_seed_rejects_changed_inputs_and_dependents(trusted_build, change):
    from trellis.atomic_actions import observations as obs
    repo, _, _, authority = trusted_build
    if change == 'source':
        (repo / 'Tablet/A.lean').write_text('import Tablet.Preamble\n-- changed\n')
    elif change == 'bundle':
        obs._tablet_olean_path(authority, 'A').write_bytes(b'changed after attestation')
    elif change == 'private_part':
        Path(str(obs._tablet_olean_path(authority, 'A')) + '.private').write_bytes(b'new unbound part')
    elif change == 'missing_attestation':
        obs._olean_srcclosure_sidecar_path(authority, 'A').unlink()
    elif change == 'toolchain':
        (repo / 'lean-toolchain').write_text('different toolchain\n')
    target, private = seed_attempt(trusted_build)
    assert not obs._tablet_olean_path(private, 'A').exists()
    assert not obs._tablet_olean_path(private, 'B').exists()
    expected = 0 if change == 'toolchain' else 1
    assert json.loads((target / 'prepared.json').read_text())['seeded_oleans'] == expected


def test_authoritative_seed_detects_bundle_replacement_during_copy(trusted_build, monkeypatch):
    from trellis.atomic_actions import observations as obs
    import trellis.dispatch.workspace as workspace
    original = workspace._copy_build_file
    def interrupted_copy(source, destination, **kwargs):
        original(source, destination, **kwargs)
        if str(destination).endswith('.trellis/supervisor/repo/.lake/build/lib/lean/Tablet/A.olean'):
            destination.write_bytes(b'copied different artifact generation')
    monkeypatch.setattr(workspace, '_copy_build_file', interrupted_copy)
    target, private = seed_attempt(trusted_build)
    assert json.loads((target / 'prepared.json').read_text())['seeded_oleans'] == 1
    assert not obs._tablet_olean_path(private, 'A').exists()
    assert not obs._tablet_olean_path(private, 'B').exists()


@pytest.mark.parametrize('kind', ['olean', 'generated_c'])
def test_authoritative_seed_does_not_follow_cached_artifact_symlinks(trusted_build, tmp_path, kind):
    from trellis.atomic_actions import observations as obs
    authority = trusted_build[3]
    artifact = (obs._tablet_olean_path(authority, 'A') if kind == 'olean'
                else authority / '.lake/build/ir/Tablet/A.c')
    outside = tmp_path / 'untrusted.olean'
    outside.write_bytes(artifact.read_bytes())
    artifact.unlink()
    artifact.symlink_to(outside)
    target, private = seed_attempt(trusted_build)
    assert json.loads((target / 'prepared.json').read_text())['seeded_oleans'] == 0
    assert not private.exists()


@pytest.mark.parametrize('mutation', ['none', 'private_policy', 'canonical_policy', 'source', 'missing_input', 'legacy'])
def test_prepared_acceptance_stuck_checks_exact_relocated_carriers(project, monkeypatch, mutation):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch import acceptance, candidate, driver
    import trellis.runtime.kernel_cli
    repo, runtime, config = project
    config.write_text(json.dumps(dict(repo_path=str(repo), state_dir='.trellis',
        chat={'root_dir': str(repo / '.trellis/chats')}, workflow={'trust_mode': 'strict'})))
    captured = dict(source_manifest(repo), **{'trellis.config.json': digest(config.read_bytes()),
        'Tablet/Missing.tex': 'absent'})
    scoped = {p: h for p, h in captured.items() if p != 'Tablet/Helper.lean'}
    task = dict(status={'status': 'checking'}, assignment={'task_kind': 'lean_closure'})
    private = dict(live={}, in_flight_request=dict(id=7, cycle=1, kind='worker',
        phase='ProofFormalization', active_node='A', task_execution={'task_kind': 'lean_closure'}))
    source = dict(input_hashes=captured, graph={})
    d = driver.ProductionDriver.__new__(driver.ProductionDriver)
    d.runtime, d.root, d.repo, d.config_path = runtime, runtime / 'dispatch', repo, config
    d.state = lambda: dict(dispatch={'tasks': {'1': task}})
    d.native = SimpleNamespace(locked=nullcontext, _require_recovered_publication=lambda: None,
        observe_sources=lambda *_: source, _call=lambda _: private)
    d.prepare_workspace = lambda *a, **kw: prepare(*a, **kw, seed_packages=False)
    sealed = runtime / 'sealed'
    candidate.seal(repo, sealed, task_id=1, base_manifest=source_manifest(repo), inputs=scoped,
        response=dict(outcome='stuck', summary='Missing lemma', comments=''))
    record, identity = candidate.read(sealed)
    directory, _, returned_inputs, _ = d._prepare_acceptance(1, 1, identity)
    receipt = json.loads((directory / 'canonical.json').read_text())
    prepared = json.loads((directory / 'prepared.json').read_text())['prepared_config_sha256']
    assert returned_inputs == captured == receipt['key']['input_hashes']
    assert prepared != captured['trellis.config.json']
    private_repo = directory / 'repo'
    relocated = json.loads((private_repo / 'trellis.config.json').read_text())
    assert relocated['repo_path'] == str(private_repo)
    assert relocated['chat']['root_dir'] == str(private_repo / '.trellis/chats')
    if mutation == 'private_policy':
        relocated['workflow']['trust_mode'] = 'changed'
        (private_repo / 'trellis.config.json').write_text(json.dumps(relocated))
    elif mutation == 'canonical_policy':
        captured['trellis.config.json'] = digest(b'changed canonical policy')
    elif mutation == 'source':
        (private_repo / 'Tablet/A.lean').write_text('changed after preparation')
    elif mutation == 'missing_input':
        captured.pop('Tablet/Missing.tex')
    elif mutation == 'legacy':
        prepared = None
    calls = []
    def native(payload):
        calls.append(payload)
        return dict(status='dispatch_ok', output=dict(response=dict(outcome='Stuck', acceptance_check_passed=False)))
    monkeypatch.setattr(trellis.runtime.kernel_cli, 'run_kernel_cli', native)
    result = acceptance.unchanged_closure_stuck(private_repo, private, record, captured, prepared)
    assert bool(result) == (mutation == 'none')
    assert len(calls) == (1 if mutation == 'none' else 0)


def completion_state(**changes):
    return dict(phase='Modeling', in_flight_request=dict(id=308, kind='Audit'), **changes)


def completion_reports(repo):
    scratch = repo / '.trellis/scratch'
    current = scratch / 'provenance-current'
    current.mkdir(parents=True)
    companion = current / 'source-tree.json'
    companion.write_bytes(b'{"exact": "source bytes"}\n')
    report = current / 'provenance.json'
    report.write_text(json.dumps(dict(ok=True, source_digest=dict(manifest=str(companion)))))
    old = scratch / 'provenance-old'
    old.mkdir()
    (old / 'provenance.json').write_text('old, not current evidence')
    completion = scratch / 'modeling-completion-check.json'
    completion.write_text(json.dumps(dict(ok=False, errors=['example current failure'],
        provenance_reports=[str(report.relative_to(repo))])))
    return completion, report, companion


def test_completion_audit_delivers_exact_current_reports_and_retains_adoption(project):
    repo, runtime, config = project
    reports = completion_reports(repo)
    target = runtime / 'dispatch/exclusive-308'
    manifest = source_manifest(repo)
    prepare(runtime, repo, config, target, completion_state(), expected_manifest=manifest, seed_packages=False)
    for report in reports:
        private = target / 'repo' / report.relative_to(repo)
        assert private.read_bytes() == report.read_bytes()
        assert private.stat().st_ino != report.stat().st_ino
    assert not (target / 'repo/.trellis/scratch/provenance-old').exists()
    assert source_manifest(target / 'repo') == manifest
    receipt = json.loads((target / 'prepared.json').read_text())
    assert receipt['completion_evidence'] == {str(p.relative_to(repo)): digest(p.read_bytes()) for p in reports}
    retained = (target / 'repo/.trellis/scratch/modeling-completion-check.json').read_bytes()
    reports[0].write_text('later canonical change')
    prepare(runtime, repo, config, target, completion_state(), expected_manifest=manifest, seed_packages=False)
    assert (target / 'repo/.trellis/scratch/modeling-completion-check.json').read_bytes() == retained


@pytest.mark.parametrize('route', ['modeling_request', 'modeling_route_review'])
def test_route_and_reopening_audits_need_no_completion_evidence(project, route):
    repo, runtime, config = project
    prepare(runtime, repo, config, runtime / 'dispatch/audit', completion_state(**{route: {}}),
            expected_manifest=source_manifest(repo), seed_packages=False)


@pytest.mark.parametrize('failure', ['missing', 'historical', 'escape', 'symlink', 'missing-companion'])
def test_completion_audit_refuses_missing_or_unconfined_evidence(project, failure):
    repo, runtime, config = project
    completion, report, companion = completion_reports(repo)
    if failure == 'missing':
        completion.unlink()
    elif failure == 'historical':
        completion.write_text('{"ok": true}')
    elif failure == 'escape':
        completion.write_text(json.dumps(dict(provenance_reports=['../provenance-outside/provenance.json'])))
    elif failure == 'symlink':
        content = report.read_bytes()
        report.unlink()
        (repo / 'outside.json').write_bytes(content)
        report.symlink_to(repo / 'outside.json')
    else:
        companion.unlink()
    target = runtime / 'dispatch/audit'
    with pytest.raises(CandidateError, match='completion|provenance'):
        prepare(runtime, repo, config, target, completion_state(),
                expected_manifest=source_manifest(repo), seed_packages=False)
    assert not target.exists()


def test_completion_delivery_follows_cache_seeding_and_overrides_old_handoff(project, monkeypatch):
    from trellis.dispatch import workspace
    repo, runtime, config = project
    reports = completion_reports(repo)
    relative = reports[0].relative_to(repo)
    handoff = runtime / 'old-review'
    (handoff / relative).parent.mkdir(parents=True)
    (handoff / relative).write_text('outdated review scratch')
    def seed(source, destination):
        assert not (destination / '.trellis/supervisor/repo').exists()
        (destination / '.trellis/supervisor/repo').mkdir(parents=True)
        return 42
    monkeypatch.setattr(workspace, '_seed_authoritative_build', seed)
    state = completion_state()
    state['in_flight_request']['reviewer_comments'] = f'Read `{relative}`.'
    target = runtime / 'dispatch/audit'
    prepare(runtime, repo, config, target, state, expected_manifest=source_manifest(repo),
            seed_packages=False, handoff_repo=handoff)
    assert (target / 'repo' / relative).read_bytes() == reports[0].read_bytes()
    assert json.loads((target / 'prepared.json').read_text())['seeded_oleans'] == 42


@pytest.mark.parametrize('unsupported', [False, True])
def test_build_clone_keeps_independent_writes_and_metadata(tmp_path, monkeypatch, unsupported):
    import errno
    import os
    import trellis.dispatch.workspace as workspace
    source, target = tmp_path / 'source', tmp_path / 'target'
    source.write_bytes(b'build bytes' * 1000)
    source.chmod(0o640)
    os.utime(source, ns=(123456000000, 123456000000))
    calls = []
    def clone(dst, command, src):
        calls.append(command)
        if unsupported:
            raise OSError(errno.EOPNOTSUPP, 'unsupported filesystem')
        # Exercise the successful ioctl branch independently of test mount type.
        with os.fdopen(os.dup(src), 'rb') as reader, os.fdopen(os.dup(dst), 'wb') as writer:
            shutil.copyfileobj(reader, writer)
    monkeypatch.setattr(workspace.fcntl, 'ioctl', clone)
    workspace._copy_build_file(source, target)
    assert calls == [0x40049409]
    assert target.read_bytes() == source.read_bytes()
    assert target.stat().st_ino != source.stat().st_ino
    assert target.stat().st_mode == source.stat().st_mode
    assert target.stat().st_mtime_ns == source.stat().st_mtime_ns
    target.write_bytes(b'private rebuild')
    assert source.read_bytes() == b'build bytes' * 1000
    source.write_bytes(b'new canonical build')
    assert target.read_bytes() == b'private rebuild'


@pytest.mark.parametrize('error_name', ['EXDEV', 'EINVAL', 'ENOTTY', 'ENOSYS', 'ENOSPC', 'EIO'])
def test_build_clone_fallback_only_for_unsupported_operation(tmp_path, monkeypatch, error_name):
    import errno
    import trellis.dispatch.workspace as workspace
    source, target = tmp_path / 'source', tmp_path / 'target'
    source.write_bytes(b'exact cache bytes')
    def refuse(*args):
        raise OSError(getattr(errno, error_name), error_name)
    monkeypatch.setattr(workspace.fcntl, 'ioctl', refuse)
    if error_name in ('ENOSPC', 'EIO'):
        with pytest.raises(OSError) as result:
            workspace._copy_build_file(source, target)
        assert result.value.errno == getattr(errno, error_name)
        assert target.read_bytes() == b'', 'real storage faults must not trigger another allocation'
    else:
        workspace._copy_build_file(source, target)
        assert target.read_bytes() == b'exact cache bytes'
    assert source.read_bytes() == b'exact cache bytes'


def test_build_clone_existing_destination_uses_normal_copy_without_ioctl(tmp_path, monkeypatch):
    import trellis.dispatch.workspace as workspace
    source, target = tmp_path / 'source', tmp_path / 'target'
    source.write_bytes(b'new cache')
    target.write_bytes(b'old private cache')
    monkeypatch.setattr(workspace.fcntl, 'ioctl', lambda *a: pytest.fail('existing files must not be clone probes'))
    workspace._copy_build_file(source, target)
    assert target.read_bytes() == b'new cache'
    with pytest.raises(shutil.SameFileError):
        workspace._copy_build_file(source, source)
    assert source.read_bytes() == b'new cache'


@pytest.mark.parametrize('kind', ['Review', 'Worker', 'StuckMathAudit'])
@pytest.mark.parametrize('missing', [False, True])
def test_reviewer_advisory_captures_exact_producer_notes_and_keeps_prepared_bytes(project, missing, kind):
    repo, runtime, config = project
    relative = '.trellis/stuck-math-audit/cycle-928-request-3282/probe.lean'
    canonical = repo / relative
    canonical.parent.mkdir(parents=True)
    canonical.write_text('older canonical note must not substitute')
    review = runtime / 'dispatch/review-3282/repo'
    producer = review / relative
    producer.parent.mkdir(parents=True)
    if not missing:
        producer.write_bytes(b'exact stopped producer probe\n')
    (producer.parent / 'unreferenced.md').write_text('do not copy')
    request = dict(id=3288, kind=kind, reviewer_comments=f'Read `{relative}:12`.')
    state = {'in_flight_request': request}
    target = runtime / 'dispatch/exclusive-3288'
    manifest = source_manifest(repo)
    prepare(runtime, repo, config, target, state, expected_manifest=manifest,
            handoff_repo=review, seed_packages=False)
    receipt = (target / 'prepared.json').read_bytes()
    metadata = json.loads(receipt)
    assert not (target / 'repo' / relative).exists() if missing else (
        (target / 'repo' / relative).read_bytes() == producer.read_bytes())
    assert metadata['missing_reviewer_handoffs'] == ([relative] if missing else [])
    assert not (target / 'repo' / producer.parent.relative_to(review) / 'unreferenced.md').exists()
    producer.write_text('later producer change must not affect frozen attempt')
    prepare(runtime, repo, config, target, state, expected_manifest=manifest,
            handoff_repo=review, seed_packages=False)
    assert (target / 'prepared.json').read_bytes() == receipt
    if not missing:
        assert (target / 'repo' / relative).read_bytes() == b'exact stopped producer probe\n'
    assert canonical.read_text() == 'older canonical note must not substitute'


@pytest.mark.parametrize('change', ['none', 'target', 'dependency', 'toolchain', 'corrupt'])
def test_advisory_first_mirror_reuses_only_captured_current_supervisor_cache(trusted_build, tmp_path, monkeypatch, change):
    from trellis.checker.job_workspace import capture, prepare_mirror
    from trellis.atomic_actions import observations as obs
    repo, _, _, authority = trusted_build
    monkeypatch.setattr('trellis.supervisor_workspace._ensure_supervisor_lake_packages', lambda *a: None)
    if change == 'target':
        (repo / 'Tablet/B.lean').write_text('import Tablet.A\n-- edited target\n')
    elif change == 'dependency':
        (repo / 'Tablet/A.lean').write_text('import Tablet.Preamble\n-- edited dependency\n')
    elif change == 'toolchain':
        (repo / 'lean-toolchain').write_text('leanprover/lean4:v9.0.0\n')
    elif change == 'corrupt':
        obs._tablet_olean_path(authority, 'A').write_bytes(b'corrupt')
    captured = capture(repo)[1]
    # Live source can move after submission; the mirror's captured bytes win.
    (repo / 'Tablet/B.lean').write_text('import Tablet.A\n-- later edit\n')
    mirror = tmp_path / 'advisory/repo'
    prepare_mirror(repo, mirror, captured)
    expected = {'none': {'Preamble', 'A', 'B'}, 'target': {'Preamble', 'A'},
                'dependency': {'Preamble'}, 'toolchain': set(), 'corrupt': {'Preamble'}}[change]
    cache = mirror / '.lake/build/lib/lean/Tablet'
    assert {p.stem for p in cache.glob('*.olean')} == expected
    assert not (mirror / '.trellis/supervisor/repo').exists()
    for name in expected:
        original = obs._tablet_olean_path(authority, name)
        copied = obs._tablet_olean_path(mirror, name)
        assert copied.read_bytes() == original.read_bytes()
        assert copied.stat().st_ino != original.stat().st_ino
        assert not (mirror / f'.lake/build/ir/Tablet/{name}.o').exists()
    if expected:
        own = cache / 'Preamble.olean'; own.write_bytes(b'own later advisory output')
        prepare_mirror(repo, mirror, captured)
        assert own.read_bytes() == b'own later advisory output'
        assert obs._tablet_olean_path(authority, 'Preamble').read_bytes() != own.read_bytes()


def test_compact_workspace_is_adopted_without_rewriting_snapshot(project, monkeypatch):
    from trellis import private_state
    from trellis.history_artifacts import encode_shared_state
    from trellis.dispatch.candidate import canonical_json
    repo, runtime, config = project
    state = dict(in_flight_request=dict(id=7), evidence=['long shared evidence '*20]*400)
    original_writer = private_state.write_private_state
    writes = []
    def native(request):
        writes.append(request['path'])
        raw = canonical_json(encode_shared_state({'state': request['state']}))
        Path(request['path']).write_bytes(raw)
        return dict(status='dispatch_ok', output=dict(status='written', bytes=len(raw), sha256=digest(raw)))
    monkeypatch.setattr(private_state, 'write_private_state',
        lambda path, state: original_writer(path, state, kernel=native))
    target = runtime / 'dispatch/1'
    manifest = source_manifest(repo)
    prepare(runtime, repo, config, target, state, expected_manifest=manifest, seed_packages=False)
    path = target / 'runtime/protocol_state.json'
    before = path.read_bytes()
    assert json.loads(before)['$format'] == 'trellis-shared-state/1'
    assert private_state.read_private_state(path) == state
    prepare(runtime, repo, config, target, state, expected_manifest=manifest, seed_packages=False)
    assert len(writes) == 1 and path.read_bytes() == before


def test_workspace_phase_measurements_are_optional_and_reuse_does_no_copy(project):
    from trellis.dispatch.telemetry import NativeMeasurements
    repo, runtime, config = project
    m = NativeMeasurements()
    state = {'in_flight_request': {'id': 7}}
    target = runtime / 'dispatch/1'
    manifest = source_manifest(repo)
    prepare(runtime, repo, config, target, state, expected_manifest=manifest,
            seed_packages=False, measurements=m)
    marker = (target / 'prepared.json').read_bytes()
    before = m.snapshot()['rows']
    assert all(before['phase:workspace.' + name]['calls'] == 1 for name in
               ['identity', 'source_validation', 'copy', 'private_state_write',
                'final_source_validation', 'audit_materialization', 'sync', 'publish'])
    assert all(row['failures'] == 0 for row in before.values())
    prepare(runtime, repo, config, target, state, expected_manifest=manifest,
            seed_packages=False, measurements=m)
    after = m.snapshot()['rows']
    assert after['phase:workspace.identity']['calls'] == 2
    assert after['phase:workspace.copy']['calls'] == 1
    assert (target / 'prepared.json').read_bytes() == marker


def test_workspace_sync_failure_is_measured_and_does_not_publish(project, monkeypatch):
    from trellis.dispatch import workspace
    from trellis.dispatch.telemetry import NativeMeasurements
    repo, runtime, config = project
    m = NativeMeasurements()
    error = OSError('fsync failed')
    def fail(_):
        raise error
    monkeypatch.setattr(workspace, 'sync_tree', fail)
    target = runtime / 'dispatch/1'
    with pytest.raises(OSError) as caught:
        prepare(runtime, repo, config, target, {'in_flight_request': {'id': 7}},
                expected_manifest=source_manifest(repo), seed_packages=False, measurements=m)
    assert caught.value is error
    assert not target.exists()
    rows = m.snapshot()['rows']
    assert rows['phase:workspace.sync']['failures'] == 1
    assert 'phase:workspace.publish' not in rows


def test_host_preparation_omits_only_outer_build_and_cannot_become_provider(project):
    from trellis.dispatch import batch_cache
    from trellis.dispatch.driver import ProductionDriver
    from types import SimpleNamespace
    repo, runtime, config = project
    config_cache = repo / '.lake/config/lakefile.olean'
    config_cache.parent.mkdir(parents=True)
    config_cache.write_bytes(b'Lake configuration cache')
    target = runtime / 'dispatch/1'
    state = {'in_flight_request': {'id': 7}}
    inputs = dict(expected_manifest=source_manifest(repo), seed_packages=False)
    prepare(runtime, repo, config, target, state, host_only=True, **inputs)
    assert not (target / 'repo/.lake/build').exists()
    assert (target / 'repo/.lake/config/lakefile.olean').read_bytes() == config_cache.read_bytes()
    assert source_manifest(target / 'repo') == inputs['expected_manifest']
    marker = (target / 'prepared.json').read_bytes()
    assert json.loads(marker)['host_only'] is True
    prepare(runtime, repo, config, target, state, host_only=True, **inputs)
    assert (target / 'prepared.json').read_bytes() == marker
    with pytest.raises(CandidateError, match='host-only'):
        prepare(runtime, repo, config, target, state, **inputs)
    with pytest.raises(CandidateError, match='host-only'):
        batch_cache.hydrate(target, runtime / 'dispatch', None)
    with pytest.raises(CandidateError, match='host-only'):
        ProductionDriver._hydrate_worker_cache(SimpleNamespace(root=runtime / 'dispatch'), {'id': 1})
    with pytest.raises(CandidateError, match='host-only'):
        prepare(runtime, repo, config, runtime / 'dispatch/2', state, host_only=True,
                deferred_build_cache={'unused': True}, **inputs)
    assert not (runtime / 'dispatch/2').exists()


def test_old_full_host_preparation_is_adopted_without_deleting_its_build(project):
    repo, runtime, config = project
    audit = repo / '.trellis/stuck-math-audit/cycle-1/retained.md'
    audit.parent.mkdir(parents=True); audit.write_text('old captured audit history')
    target = runtime / 'dispatch/1/acceptance/1'
    state = {'in_flight_request': {'id': 7}}
    inputs = dict(expected_manifest=source_manifest(repo), seed_packages=False)
    prepare(runtime, repo, config, target, state, **inputs)
    marker = (target / 'prepared.json').read_bytes()
    build = target / 'repo/.lake/build/project.olean'
    before = build.read_bytes()
    prepare(runtime, repo, config, target, state, host_only=True, **inputs)
    assert build.read_bytes() == before
    assert (target / 'repo' / audit.relative_to(repo)).read_bytes() == audit.read_bytes()
    assert (target / 'prepared.json').read_bytes() == marker


def test_staged_host_preparation_and_finalizer_donor_restart_use_real_workspace(project, monkeypatch):
    from contextlib import nullcontext
    from types import SimpleNamespace
    from trellis.dispatch import acceptance_receipts as receipts, staged_driver, staged_acceptance as staged, workspace
    from trellis.dispatch import candidate
    from trellis.dispatch.driver import ProductionDriver
    from trellis.dispatch.publication import atomic
    repo, runtime, config = project
    audit = repo / '.trellis/stuck-math-audit/cycle-1/old.md'
    audit.parent.mkdir(parents=True); audit.write_text('unreferenced old audit')
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'test-implementation')
    monkeypatch.setattr(receipts, 'input_hashes', candidate.source_manifest)
    manifest = source_manifest(repo)
    task = dict(id=1, assignment={'scope': {'nodes': ['A']}, 'compiler': 'managed'})
    source = dict(manifest=manifest, input_hashes=manifest, graph={'edges': {'A': []}})
    private = dict(phase='ProofFormalization', request_seq=7,
        in_flight_request=dict(id=7, cycle=1, kind='Worker', phase='ProofFormalization',
                              acceptance_logic_identity='test-implementation'))
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.runtime, driver.root, driver.repo, driver.config_path = runtime, runtime / 'dispatch', repo, config
    driver.native = SimpleNamespace(locked=nullcontext)
    driver.prepare_workspace = workspace.prepare
    identity = candidate.seal(repo, driver.root / '1/candidate', task_id=1,
        base_manifest=manifest, inputs=manifest, response={'outcome': 'valid'})
    monkeypatch.setattr(staged_driver, '_current', lambda *a: (task, source, private))
    monkeypatch.setattr(staged_driver, '_plan', lambda *a: dict(status='planned', read_nodes=['A'], write_nodes=['A']))
    monkeypatch.setattr(staged_driver, '_select_legacy', lambda *a: None)
    atomic(driver.root / '1/acceptance-admission.json', candidate.canonical_json(
        {'footprint': {'reads': ['A'], 'writes': ['A'], 'exclusive': False}}))
    owned = driver.root / '1/acceptance/1'
    assert staged_driver._prepare(driver, 1, identity, owned)['status'] == 'prepared'
    assert json.loads((owned / 'prepared.json').read_text())['host_only'] is True
    assert not (owned / 'repo/.lake/build').exists()
    assert not (owned / 'repo/.trellis/stuck-math-audit').exists()
    def put(path, value):
        atomic(path, candidate.canonical_json(value))
    process = dict(pid=99999999, start_ticks=1, boot_id='expired-test-process')
    for name in ('process', 'permit', 'cleanup'):
        put(owned / f'execution/{name}.json', process)
    payload = dict(stage='owned', config_path=str(owned / 'repo/trellis.config.json'),
        runtime_root=str(owned / 'runtime'), candidate=str(driver.root / '1/candidate'),
        captured_inputs=manifest, source_graph=source['graph'])
    put(owned / 'execution/recipe.json', dict(command=['python', *staged._COMMAND], payload=payload,
        checker=dict(runtime=str(runtime), task_runtime=str(owned / 'runtime'), compiler='managed',
                     scope='1/acceptance/1')))
    context = {'request': private['in_flight_request'], 'host_baseline': True}
    put(owned / 'runtime/staged-context.json', context)
    output = dict(stage='owned', status='facts',
        facts=dict(schema_version=1, owner_nodes=['A'], owner_ok=True,
            acceptance_logic_identity='test-implementation',
            observation_environment={'environment_sha256s': {'A': 'complete-import-bundle'}}),
        candidate_sha256=identity, context_sha256=digest(candidate.canonical_json(context)),
        checked_sources=receipts.checked_sources(owned / 'repo'))
    put(owned / 'execution/response.json', output)
    put(owned / 'execution/result.json', dict(returncode=0,
        response_sha256=digest((owned / 'execution/response.json').read_bytes())))
    staged.complete(owned, output)
    finalizer = owned / 'finalization/1'
    assert staged_driver._prepare(driver, 1, identity, finalizer, owned=owned, checked=output)['status'] == 'prepared'
    assert not (finalizer / 'repo/.lake/build').exists()
    assert not (finalizer / 'repo/.trellis/stuck-math-audit').exists()
    assert json.loads((finalizer / 'prepared.json').read_text())['host_only'] is True
    assert staged.donor(finalizer / 'runtime')[0] == owned
    preserved = {name: (finalizer / name).read_bytes() for name in ('prepared.json', 'canonical.json', 'staged-donor.json')}
    assert staged_driver._prepare(driver, 1, identity, finalizer, owned=owned, checked=output)['status'] == 'prepared'
    assert all((finalizer / name).read_bytes() == data for name, data in preserved.items())
    assert staged.donor(finalizer / 'runtime')[0] == owned


def test_host_preparation_preserves_custom_adapter_call_contract(project):
    from types import SimpleNamespace
    from trellis.dispatch.driver import ProductionDriver
    repo, runtime, config = project
    driver = ProductionDriver.__new__(ProductionDriver)
    driver.runtime, driver.root, driver.repo, driver.config_path = runtime, runtime / 'dispatch', repo, config
    driver.native = SimpleNamespace()
    calls = []
    def adapter(runtime, repo, config, target, private, *, expected_manifest, previous, handoff_repo):
        calls.append(target)
        return prepare(runtime, repo, config, target, private, expected_manifest=expected_manifest,
                       previous=previous, handoff_repo=handoff_repo, seed_packages=False)
    driver.prepare_workspace = adapter
    target = driver.root / '1/acceptance/1'
    driver._prepare(target, {'in_flight_request': {'id': 7}}, host_only=True)
    assert calls == [target]
    assert (target / 'repo/.lake/build/project.olean').is_file()
    assert 'host_only' not in json.loads((target / 'prepared.json').read_text())


@pytest.mark.parametrize('producer', ['canonical', 'review', 'missing_review'])
def test_host_never_copies_bulk_audit_history_but_preserves_scoped_handoffs(project, monkeypatch, producer):
    from trellis.dispatch import workspace
    repo, runtime, config = project
    relative = '.trellis/stuck-math-audit/cycle-7/probe.lean'
    requested = repo / relative
    requested.parent.mkdir(parents=True)
    requested.write_text('canonical evidence')
    (requested.parent / 'unreferenced.md').write_text('historical audit context')
    review = runtime / 'dispatch/review-6/repo'
    (review / relative).parent.mkdir(parents=True)
    if producer == 'review':
        (review / relative).write_text('exact private reviewer evidence')
    scoped = repo / '.trellis/artifacts/scoped.json'
    scoped.parent.mkdir(parents=True); scoped.write_text('scoped artifact')
    state = {'in_flight_request': dict(id=7, kind='Worker', reviewer_comments=f'Read `{relative}`.')}
    copied = []
    original = workspace._copy_tree
    def copy(source, destination, **kwargs):
        copied.append(Path(source))
        return original(source, destination, **kwargs)
    monkeypatch.setattr(workspace, '_copy_tree', copy)
    target = runtime / 'dispatch/1/acceptance/1'
    arguments = dict(expected_manifest=source_manifest(repo), seed_packages=False, host_only=True,
        handoff_repo=None if producer == 'canonical' else review)
    prepare(runtime, repo, config, target, state, **arguments)
    assert repo / '.trellis/stuck-math-audit' not in copied
    assert not (target / 'repo' / requested.parent.relative_to(repo) / 'unreferenced.md').exists()
    assert (target / 'repo' / scoped.relative_to(repo)).read_text() == 'scoped artifact'
    marker = (target / 'prepared.json').read_bytes(); metadata = json.loads(marker)
    selected = target / 'repo' / relative
    if producer == 'missing_review':
        assert not selected.exists() and metadata['missing_reviewer_handoffs'] == [relative]
    else:
        assert selected.read_bytes() == (requested if producer == 'canonical' else review / relative).read_bytes()
    # Restart adopts the captured request/evidence; newly changed producer
    # history cannot add or replace anything in the already prepared host.
    requested.write_text('new canonical evidence')
    (review / relative).write_text('new private reviewer evidence')
    copied.clear()
    prepare(runtime, repo, config, target, state, **arguments)
    assert not copied and (target / 'prepared.json').read_bytes() == marker
    if producer == 'missing_review':
        assert not selected.exists()
    else:
        assert selected.read_text() == ('canonical evidence' if producer == 'canonical' else
                                       'exact private reviewer evidence')


@pytest.mark.parametrize('kind', ['Worker', 'Review', 'StuckMathAudit'])
def test_provider_preparation_keeps_unreferenced_audit_history(project, kind):
    repo, runtime, config = project
    relative = '.trellis/stuck-math-audit/cycle-1/old-evidence.lean'
    path = repo / relative; path.parent.mkdir(parents=True); path.write_text('citable earlier audit')
    target = runtime / 'dispatch/1'
    prepare(runtime, repo, config, target, {'in_flight_request': {'id': 7, 'kind': kind}},
            expected_manifest=source_manifest(repo), seed_packages=False)
    assert (target / 'repo' / relative).read_bytes() == path.read_bytes()

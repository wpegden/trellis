import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

import pytest

from test_dispatch_terminal_retention import setup, put, git
from trellis.dispatch import terminal_retention as retention
from trellis.dispatch import retained_files as retained
from trellis.dispatch.candidate import canonical_json


@pytest.fixture(params=['review', 'verifier'])
def owner_setup(setup, request):
    collector, directory, _, paths, state = setup
    kind = request.param
    id = 2 if kind == 'review' else 3
    target = directory.with_name(f'{kind}-{id}')
    directory.rename(target)
    paths = tuple(target / p.relative_to(directory) for p in paths)
    directory = target
    response = dict(kind='parallel_review', action=None, assignments=[])
    receipt = dict(review_id=2, response_sha256=hashlib.sha256(canonical_json(response)).hexdigest(), decisions=[])
    state['dispatch'].update(reviewer=None, receipts={'2': receipt})
    state['dispatch']['verifiers']['jobs']['3'] = dict(id=3,
        identity=dict(lane='sound', panel_sha256='panel', input_hashes={'Tablet/A.lean': 'source'}, compiler='none'),
        required=True, reviewer_requested=False,
        status=dict(status='resolved', result_sha256='a' * 64, stale_reason=None), request=dict(id=7))
    prepared = json.loads((directory / 'prepared.json').read_text())
    prepared.update(source_head=git(collector.repo, 'rev-parse', 'HEAD'), request_id=2 if kind == 'review' else 7)
    prepared['history']['kind'] = 'Review' if kind == 'review' else 'SoundVerifier'
    put(directory / 'prepared.json', prepared)
    put(directory / 'execution/response.json', response)
    put(collector.repo / retention.HISTORY, dict(state=state))
    git(collector.repo, 'add', '.')
    git(collector.repo, 'commit', '-qm', 'Commit nonworker terminal owner')
    put(collector.root.parent / 'protocol_state.json', state)
    return collector, directory, retention.terminal_owners(state), paths, state


def viewer_display(path):
    script = Path(__file__).parents[1] / 'viewer/parallel_state.py'
    spec = importlib.util.spec_from_file_location('retirement_viewer', script)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.retired_display(path)


def test_nonworker_retirement_preserves_sources_handoffs_transcripts_and_display(owner_setup):
    collector, directory, observed, paths, _ = owner_setup
    extra = ['repo/.trellis/runtime/runtime/staging/handoff.json', 'repo/.trellis/reviewer/scratch.md',
             'runtime/burst-homes/worker/.codex/sessions/transcript.jsonl', 'worker-handoffs.json']
    for name in extra:
        put(directory / name, {'retain': name})
    before = {p: p.read_bytes() for p in directory.rglob('*') if p.is_file() and p not in paths}
    result = collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert result['removed'] == 3 and not any(p.exists() for p in paths)
    assert all(p.read_bytes() == content for p, content in before.items())
    record = json.loads((directory / retention.RECEIPT).read_text())
    assert record['format'] == retention.OWNER_FORMAT and record['owner'] == retained.owner_identity(directory)
    display = json.loads(paths[0].with_name('display-state.json').read_text())
    assert display['format'] == retention.OWNER_DISPLAY_FORMAT and display['owner'] == record['owner']
    projected = viewer_display(paths[0])
    assert projected['in_flight_request']['id'] == json.loads((directory / 'prepared.json').read_text())['request_id']
    assert collector.retire(directory, observed, apply=True)['removed'] == 0


def test_verifier_mutable_flags_and_native_request_omission_do_not_change_owner(owner_setup):
    collector, directory, _, paths, state = owner_setup
    if directory.name != 'verifier-3':
        return
    job = state['dispatch']['verifiers']['jobs']['3']
    job['required'] = False
    job['reviewer_requested'] = True
    del job['request']  # Native routing intentionally omits this large object.
    assert collector.retire(directory, retention.terminal_owners(state), apply=True)['removed'] == 3


@pytest.mark.parametrize('deferred', [False, True])
@pytest.mark.parametrize('change', ['identity', 'terminal_status', 'active'])
def test_uncommitted_or_active_owner_is_preserved(owner_setup, change, deferred):
    collector, directory, _, paths, state = owner_setup
    if directory.name == 'review-2':
        if change == 'active':
            state['dispatch']['reviewer'] = dict(request_id=2)
        elif change == 'identity':
            state['dispatch']['receipts']['2']['response_sha256'] = 'b' * 64
        else:
            del state['dispatch']['receipts']['2']
    else:
        job = state['dispatch']['verifiers']['jobs']['3']
        if change == 'active':
            job['status'] = dict(status='checking', result_sha256='a' * 64)
        elif change == 'identity':
            job['identity']['panel_sha256'] = 'different-panel'
        else:
            job['status']['stale_reason'] = 'different result'
    result = collector.retire(directory, retention.terminal_owners(state, defer_verifiers=deferred), apply=True)
    assert result['removed'] == 0 and all(p.exists() for p in paths)


@pytest.mark.parametrize('change', ['request_id', 'source_repo', 'source_head'])
def test_nonworker_preparation_must_match_committed_owner(owner_setup, change):
    collector, directory, observed, paths, _ = owner_setup
    prepared = json.loads((directory / 'prepared.json').read_text())
    prepared[change] = {'request_id': 999, 'source_repo': str(directory), 'source_head': '0' * 40}[change]
    put(directory / 'prepared.json', prepared)
    if directory.name == 'verifier-3' and change == 'request_id':
        # The immutable prepared request belongs to this execution; the final
        # equivalent ordinary request in the shared job need not have its ID.
        assert collector.retire(directory, observed, apply=True, budget={'files': 20})['removed'] == 3
        assert viewer_display(paths[0])['in_flight_request']['id'] == 999
    else:
        with pytest.raises((ValueError, subprocess.CalledProcessError)):
            collector.retire(directory, observed, apply=True)
        assert all(p.exists() for p in paths)


@pytest.mark.parametrize('deferred', [False, True])
def test_reused_owner_id_cannot_reuse_old_retirement_receipt(owner_setup, deferred):
    collector, directory, observed, paths, state = owner_setup
    collector.retire(directory, observed, apply=True, budget={'files': 0})
    if directory.name == 'review-2':
        state['dispatch']['receipts']['2']['response_sha256'] = 'b' * 64
    else:
        state['dispatch']['verifiers']['jobs']['3']['identity']['panel_sha256'] = 'replacement'
    put(collector.repo / retention.HISTORY, dict(state=state))
    git(collector.repo, 'add', '.')
    git(collector.repo, 'commit', '-qm', 'Invalid reuse of terminal identity')
    with pytest.raises(ValueError, match='changed identity'):
        collector.retire(directory, retention.terminal_owners(state, defer_verifiers=deferred), apply=True)
    assert all(p.exists() for p in paths)


@pytest.mark.parametrize('guard', ['pin', 'live', 'cleanup', 'tracked'])
def test_nonworker_guards_preserve_inputs(owner_setup, monkeypatch, guard):
    collector, directory, observed, paths, _ = owner_setup
    if guard == 'pin':
        pin = collector.root / 'reviews/2/retain-state.json' if directory.name == 'review-2' else directory / 'retain-builds.json'
        put(pin, {})
    elif guard in ('live', 'cleanup'):
        put(directory / 'execution/process.json', {'pid': 123})
        put(directory / 'execution/cleanup.json', {'pid': 123 if guard == 'live' else 456})
        monkeypatch.setattr('trellis.dispatch.process.alive', lambda _: guard == 'live')
    else:
        git(directory, 'init', '-q')
        git(directory, 'add', 'runtime/protocol_state.json')
        result = collector.retire(directory, observed, apply=True, budget={'files': 20})
        assert result['removed'] == 2 and paths[0].exists()
        return
    with pytest.raises(ValueError):
        collector.retire(directory, observed, apply=True)
    assert all(p.exists() for p in paths)


def test_committed_review_batch_can_recover_after_private_input_retirement(owner_setup):
    collector, directory, observed, paths, state = owner_setup
    if directory.name != 'review-2':
        return
    from trellis.dispatch.native import NativeLedger
    response = json.loads((directory / 'execution/response.json').read_text())
    prepared = dict(receipt=state['dispatch']['receipts']['2'], workers={})
    batch = put(collector.root / 'reviews/2/batch.json', dict(prepared=prepared))
    saved = batch.read_bytes()
    put(collector.root / 'reviews/2/workers/retained-capture.json', {'keep': True})
    collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert not any(p.exists() for p in paths)
    native = NativeLedger(collector.root.parent)
    assert native._dispatch_batch(2, response, [], {}, state=state, now_ms=1) == prepared
    assert batch.read_bytes() == saved
    assert (collector.root / 'reviews/2/workers/retained-capture.json').exists()


def test_unrelated_worker_publication_does_not_block_equal_numeric_nonworker(owner_setup):
    collector, directory, observed, paths, _ = owner_setup
    identity = 'c' * 64
    put(collector.root / 'publication/pending.json', dict(identity=identity))
    put(collector.root / 'publication' / identity / 'record.json', dict(task_id=retained.owner_identity(directory)['id']))
    assert collector.retire(directory, observed, apply=True)['removed'] == 3


def test_nonworker_background_uses_native_witness_and_respects_bounds(owner_setup):
    collector, directory, _, paths, state = owner_setup
    from trellis.dispatch.storage import BackgroundCollector
    (collector.root.parent / 'protocol_state.json').write_text('must not parse canonical again')
    # Other eligible owners need not have private directories at all.
    background = BackgroundCollector(collector.root, repo=collector.repo)
    background._review_snapshots = None
    for _ in range(3):
        assert background.schedule(state)
        background._thread.join(5)
        assert not background._thread.is_alive()
        if not any(p.exists() for p in paths):
            break
    assert not any(p.exists() for p in paths)


@pytest.mark.parametrize('change', ['kind', 'id', 'format'])
def test_nonworker_viewer_rejects_other_owner_or_worker_format(owner_setup, change):
    collector, directory, observed, paths, _ = owner_setup
    collector.retire(directory, observed, apply=True, budget={'files': 20})
    path = paths[0].with_name('display-state.json')
    value = json.loads(path.read_text())
    if change == 'format':
        value['format'] = retention.DISPLAY_FORMAT
        value['task_id'] = value['owner']['id']
    elif change == 'kind':
        value['owner']['kind'] = 'review' if value['owner']['kind'] == 'verifier' else 'verifier'
    else:
        value['owner']['id'] += 1
    put(path, value)
    with pytest.raises(ValueError):
        viewer_display(paths[0])


def test_nonworker_cli_and_exclusive_exclusion(owner_setup):
    collector, directory, _, paths, _ = owner_setup
    raw = subprocess.check_output([sys.executable, '-m', 'trellis.dispatch.terminal_retention',
        '--runtime', str(collector.root.parent), '--repo', str(collector.repo), '--owner', directory.name])
    assert json.loads(raw)['files'] == 3 and all(p.exists() for p in paths)
    with pytest.raises(ValueError, match='unsupported'):
        collector.retire(collector.root / 'exclusive-2', {}, apply=True)


def test_schedule_never_serializes_verifier_identities(owner_setup, monkeypatch):
    collector, directory, _, _, state = owner_setup
    from trellis.dispatch.storage import BackgroundCollector
    def no_hash(_):
        raise AssertionError('scheduling serialized a verifier identity')
    monkeypatch.setattr(retention._DeferredVerifierIdentity, 'resolve', no_hash)
    monkeypatch.setattr('threading.Thread.start', lambda _: None)
    background = BackgroundCollector(collector.root, repo=collector.repo)
    assert background.schedule(state)
    deferred = background._terminal_tasks['verifier-3']
    assert deferred.value['identity'] is state['dispatch']['verifiers']['jobs']['3']['identity']


def test_exhausted_owner_skips_hash_and_git_decode(owner_setup, monkeypatch):
    collector, directory, observed, paths, state = owner_setup
    collector.retire(directory, observed, apply=True, budget={'files': 20})
    def forbidden(*args):
        raise AssertionError('completed private inputs triggered expensive work')
    monkeypatch.setattr(retention._DeferredVerifierIdentity, 'resolve', forbidden)
    monkeypatch.setattr(collector, 'witness', forbidden)
    observed = retention.terminal_owners(state, defer_verifiers=True)
    assert collector.retire(directory, observed, apply=True)['status'] == 'absent'


@pytest.mark.parametrize('request_variant', ['original', 'newer', 'null', 'omitted', 'retired'])
@pytest.mark.parametrize('deferred', [False, True])
def test_verifier_retirement_uses_committed_owner_not_final_request(owner_setup, request_variant, deferred):
    collector, directory, _, paths, state = owner_setup
    if directory.name != 'verifier-3':
        return
    job = state['dispatch']['verifiers']['jobs']['3']
    if request_variant == 'omitted':
        del job['request']
    else:
        job['request'] = {'original': dict(id=7), 'newer': dict(id=9), 'null': None,
            'retired': {'format': 'historical-request-reference', 'commit': 'old-history'}}[request_variant]
    put(collector.repo / retention.HISTORY, dict(state=state))
    git(collector.repo, 'add', '.')
    git(collector.repo, 'commit', '--allow-empty', '-qm', 'Equivalent verifier request after execution')
    observed = retention.terminal_owners(state, defer_verifiers=deferred)
    result = collector.retire(directory, observed, apply=True, budget={'files': 20})
    assert result['removed'] == 3 and not any(p.exists() for p in paths)
    assert viewer_display(paths[0])['in_flight_request']['id'] == 7


def test_verifier_retirement_restart_after_request_replacement_preserves_owner_guards(owner_setup):
    collector, directory, observed, paths, state = owner_setup
    if directory.name != 'verifier-3':
        return
    first = collector.retire(directory, observed, apply=True, budget={'files': 1})
    assert first['removed'] == 1
    original_receipt = (directory / retention.RECEIPT).read_bytes()
    state['dispatch']['verifiers']['jobs']['3']['request'] = dict(id=10)
    put(collector.repo / retention.HISTORY, dict(state=state))
    git(collector.repo, 'add', '.')
    git(collector.repo, 'commit', '-qm', 'Later equivalent verifier request')
    restarted = retention.Collector(collector.root, collector.repo)
    put(directory / 'retain-builds.json', {})
    with pytest.raises(ValueError, match='operator pin'):
        restarted.retire(directory, retention.terminal_owners(state), apply=True, budget={'files': 20})
    (directory / 'retain-builds.json').unlink()
    result = restarted.retire(directory, retention.terminal_owners(state), apply=True, budget={'files': 20})
    assert result['removed'] == 2 and not any(p.exists() for p in paths)
    assert (directory / retention.RECEIPT).read_bytes() == original_receipt
    assert viewer_display(paths[0])['in_flight_request']['id'] == 7

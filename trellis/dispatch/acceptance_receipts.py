"""Host-only, exact-key acceptance reuse and bounded rejection observations.

These receipts do not approve a worker response. Every reuse still enters the
ordinary publication preflight and native source-backed transaction.
"""
from __future__ import annotations

import json
import hashlib
from itertools import islice
import os
from pathlib import Path
import re
import stat

from trellis.private_state import parse_private_state

from . import candidate
from .publication import atomic

_PROBE_SEED_MAX_BYTES = 64 << 20
_PROBE_SEED_MAX_ENTRIES = 8192
_PROBE_SEED_MAX_DONORS = 8


def implementation_identity():
    from trellis.acceptance_identity import source_identity
    identity = source_identity(Path(__file__).resolve().parents[2])
    if not identity.startswith('trellis-worker-acceptance-v2:sha256:'):
        raise RuntimeError('acceptance implementation identity unavailable')
    return identity


def key(task_id, identity, task, acceptance_sha256, source):
    return dict(version=1, task_id=task_id, candidate_sha256=identity,
        **({'submission_turn': task['worker_attempt']['turn']} if task.get('worker_attempt') is not None else {}),
        task_binding_sha256=candidate.digest(candidate.canonical_json(
            dict(assignment=task['assignment'], binding=task.get('binding')))),
        implementation_identity=implementation_identity(),
        **({'authority_input_hashes': source['authority_inputs']} if 'authority_inputs' in source else {}),
        acceptance_sha256=acceptance_sha256, input_hashes=source['input_hashes'])


def _regular_path(path):
    """Host receipts and promoted evidence must not be aliases of writable data."""
    path = Path(path).absolute()
    for parent in reversed((path, *path.parents)):
        if parent.is_symlink():
            raise candidate.CandidateError(f'acceptance evidence traverses a symlink: {path}')
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        raise candidate.CandidateError(f'acceptance evidence must be an independent regular file: {path}')
    return path


def regular(path):
    try:
        return _regular_path(path).read_bytes()
    except FileNotFoundError:
        from .retained_files import read_bytes
        try:
            return read_bytes(path)
        except ValueError as error:
            raise candidate.CandidateError(str(error)) from error


def regular_digest(path):
    """Bind large host snapshots without buffering a second protocol copy."""
    digest = hashlib.sha256()
    try:
        stream = _regular_path(path).open('rb')
    except FileNotFoundError:
        from .retained_files import chunks
        try:
            for chunk in chunks(path):
                digest.update(chunk)
        except ValueError as error:
            raise candidate.CandidateError(str(error)) from error
    else:
        with stream:
            for chunk in iter(lambda: stream.read(1 << 20), b''):
                digest.update(chunk)
    return digest.hexdigest()


def write_once(path, value):
    data = candidate.canonical_json(value)
    if Path(path).exists():
        if regular(path) != data:
            raise candidate.CandidateError(f'immutable acceptance receipt changed: {path}')
    else:
        atomic(path, data)


def input_hashes(repo):
    from trellis.runtime.kernel_cli import run_kernel_cli
    return run_kernel_cli(dict(action='dispatch_verifier_inputs', repo_path=str(repo)))['output']


def checked_sources(repo):
    repo = Path(repo)
    from .publication_artifacts import lexical_authority
    authority = lexical_authority(repo)
    if authority == repo or not authority.is_relative_to(repo):
        raise candidate.CandidateError('acceptance authority must be inside its isolated workspace')
    return dict(repo=candidate.source_manifest(repo), inputs=input_hashes(repo),
        authority=candidate.source_manifest(authority) if authority.is_dir() else None,
        authority_inputs=input_hashes(authority) if authority.is_dir() else None)


_RECEIPTS = ('canonical.json', 'prepared.json', 'base-manifest.json',
    'runtime/protocol_state.json', 'runtime/runtime_metadata.json', 'repo/trellis.config.json',
    'execution/recipe.json', 'execution/process.json', 'execution/permit.json',
    'execution/cleanup.json', 'execution/result.json', 'execution/response.json')


def _authenticate_execution(directory, expected_key):
    """Bind a reaped host launch independently of its mathematical outcome."""
    from .process import alive
    directory = Path(directory)
    if directory.parent.name != 'acceptance' or directory.parent.parent.name != str(expected_key['task_id']):
        raise candidate.CandidateError('acceptance belongs to a different task')
    task_root = directory.parent.parent
    turn = expected_key.get('submission_turn')
    if turn is not None and (type(turn) is not int or turn < 1):
        raise candidate.CandidateError('acceptance has an invalid submission ordinal')
    sealed = task_root / 'turns' / str(turn) / 'candidate' if turn is not None else task_root / 'candidate'
    sealed_record, sealed_identity = candidate.read(sealed)
    if (sealed_identity != expected_key['candidate_sha256'] or sealed_record['task_id'] != expected_key['task_id']
            or turn is not None and sealed_record.get('submission', {}).get('turn') != turn):
        raise candidate.CandidateError('sealed acceptance candidate changed')
    raw = {name: regular(directory / name) for name in _RECEIPTS}
    values = {name: parse_private_state(data) if name == 'runtime/protocol_state.json' else json.loads(data)
              for name, data in raw.items() if name != 'execution/response.json'}
    captured = values['canonical.json']
    if captured.get('key') != expected_key:
        raise candidate.CandidateError('acceptance key changed')
    if captured['acceptance_sha256'] != expected_key['acceptance_sha256'] or captured['source']['input_hashes'] != expected_key['input_hashes']:
        raise candidate.CandidateError('acceptance capture disagrees with its key')
    if captured['source'].get('authority_inputs') != expected_key.get('authority_input_hashes'):
        raise candidate.CandidateError('acceptance authority inputs disagree with its key')
    prepared = values['prepared.json']
    original = candidate.digest(candidate.canonical_json(dict(
        state=values['runtime/protocol_state.json'], sources=values['base-manifest.json'],
        config_sha256=captured['config_sha256'])))
    if prepared['identity'] != original:
        raise candidate.CandidateError('acceptance preparation receipt changed')
    from .driver import acceptance_identity
    if acceptance_identity(values['runtime/protocol_state.json']) != expected_key['acceptance_sha256']:
        raise candidate.CandidateError('acceptance prepared state changed')
    process = values['execution/process.json']
    if (values['execution/permit.json'] != process or values['execution/cleanup.json'] != process or alive(process)):
        raise candidate.CandidateError('acceptance launch/cleanup is not complete')
    recipe = values['execution/recipe.json']
    payload = recipe['payload']
    if (recipe['command'][1:] != ['-m', 'trellis.dispatch.acceptance']
            or payload.get('config_path') != str(directory / 'repo/trellis.config.json')
            or payload.get('runtime_root') != str(directory / 'runtime')
            or payload.get('candidate') != str(sealed)
            or payload.get('captured_inputs') != expected_key['input_hashes']):
        raise candidate.CandidateError('acceptance launch was not authorized for this workspace/key')
    checker = recipe.get('checker', {})
    if (checker.get('runtime') != str(directory.parents[3])
            or checker.get('task_runtime') != str(directory / 'runtime')
            or checker.get('compiler') != 'managed'
            or checker.get('scope') != str(directory.relative_to(directory.parents[2]))):
        raise candidate.CandidateError('acceptance lacks its authorized host compiler binding')
    result = values['execution/result.json']
    if (type(result.get('returncode')) is not int
            or result.get('response_sha256') != candidate.digest(raw['execution/response.json'])):
        raise candidate.CandidateError('acceptance response completion receipt changed')
    return raw, values


def authenticate(directory, expected_key):
    """Validate original preparation, successful completion and checked bytes."""
    directory = Path(directory)
    raw, values = _authenticate_execution(directory, expected_key)
    if values['execution/result.json']['returncode'] != 0:
        raise candidate.CandidateError('acceptance did not complete successfully')
    checked = json.loads(raw['execution/response.json'])
    if ('acceptance_context_sha256' in checked
            and regular_digest(directory / 'runtime/acceptance-context.json') != checked['acceptance_context_sha256']):
        raise candidate.CandidateError('acceptance baseline context changed')
    if checked.get('candidate_sha256') != expected_key['candidate_sha256'] or checked.get('stale_reason') or not isinstance(checked.get('response'), dict):
        raise candidate.CandidateError('acceptance did not check this candidate')
    if checked.get('checked_sources') != checked_sources(directory / 'repo'):
        raise candidate.CandidateError('checked acceptance sources changed')
    return dict(version=1, key=expected_key,
        files={name: candidate.digest(data) for name, data in raw.items()}), checked


def complete(directory, checked):
    captured = json.loads(regular(Path(directory) / 'canonical.json'))
    if 'key' not in captured:
        return  # Old rounds may finish normally, but are never reuse inputs.
    receipt, actual = authenticate(directory, captured['key'])
    if actual != checked:
        raise candidate.CandidateError('checked result differs from the host completion')
    write_once(Path(directory) / 'completed.json', receipt)


def reusable(directory, expected_key):
    try:
        saved = json.loads(regular(Path(directory) / 'completed.json'))
        if (saved.get('key') != expected_key
                or (Path(directory) / 'retry-authorized.json').exists()
                or (Path(directory) / 'freshness-recheck.json').exists()):
            return None
        receipt, checked = authenticate(directory, expected_key)
        return checked if receipt == saved else None
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return None  # Missing/corrupt/stale evidence only causes a fresh check.



def retry_cache_donor(directory, current_key, config_sha256, previous_rounds):
    """Select a drained interrupted host attempt as an optional leaf-cache seed.

    This does not authenticate a verdict. Implementation changes may reuse only
    the ordinary versioned artifact/derivation caches, whose current consumers
    still check source closures, replay bundles, helper bytes and full inputs.
    """
    directory = Path(directory)
    for previous in islice(previous_rounds, _PROBE_SEED_MAX_DONORS):
        previous = Path(previous)
        if (previous.parent != directory.parent or not previous.name.isdigit()
                or not directory.name.isdigit() or int(previous.name) >= int(directory.name)):
            continue
        try:
            retry = json.loads(regular(previous / 'retry-authorized.json'))
            captured = json.loads(regular(previous / 'canonical.json'))
            old_key = captured['key']
            if (not isinstance(current_key.get('authority_input_hashes'), dict)
                    or not current_key['authority_input_hashes']
                    or captured.get('config_sha256') != config_sha256
                    or dict(old_key, acceptance_sha256=current_key['acceptance_sha256'],
                            implementation_identity=current_key['implementation_identity']) != current_key):
                continue
            _, values = _authenticate_execution(previous, old_key)
            result = values['execution/result.json']
            if result['returncode'] == 0:
                continue
            # Both existing writers create this marker only after exact channel
            # drain. A result/cleanup alone does not prove the checker drained.
            event = retry.get('event', {})
            if not (retry.get('failed_result') == result or
                    (retry.get('kind') == 'bounded_infrastructure_retry'
                     and event.get('kind') == 'check_retry'
                     and event.get('task_id') == current_key['task_id']
                     and event.get('candidate_sha256') == current_key['candidate_sha256'])):
                continue
            prepared = values['prepared.json']
            if prepared.get('prepared_config_sha256') != candidate.digest(regular(previous / 'repo/trellis.config.json')):
                continue
            return previous
        except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
            continue
    return None

def seed_retry_replay(directory, current_key, previous):
    """Use the existing post-overlay installer for drained host artifacts only."""
    directory = Path(directory)
    marker = directory / 'replay-cache-donor.json'
    if previous is None or marker.exists():
        return
    try:
        captured = json.loads(regular(directory / 'canonical.json'))
        previous = retry_cache_donor(directory, current_key, captured['config_sha256'], [previous])
        if previous is None:
            return
        write_once(marker, dict(version=1, donor_kind='drained_host_execution',
            donor_round=previous.name,
            donor_completion_sha256=candidate.digest(regular(previous / 'execution/result.json')),
            current_key_sha256=candidate.digest(candidate.canonical_json(current_key))))
    except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
        return  # Optional artifact evidence stays cold on incomplete recovery.


def _probe_cache_entries(source, limit):
    with os.scandir(source) as entries:
        for entry in islice(entries, limit):
            yield Path(entry.path)


def seed_probe_caches(directory, current_key, previous_rounds):
    """Donate host observations, never an old verdict, to a fresh host round.

    Only the acceptance-state digest may differ. The destination checker still
    establishes current kernel replay and recomputes every ordinary probe key;
    normalization, source observation and native publication run afresh. Worker
    repositories and sync fingerprints are not cache donors. The same single
    authentication may authorize an optional, separately validated host replay
    donor; no artifact is installed until after the current candidate overlay.
    """
    from trellis.checker import sync
    directory = Path(directory)
    namespaces = {
        sync.SEMANTIC_PAYLOAD_DIRNAME: sync.SEMANTIC_PAYLOAD_CACHE_VERSION,
        sync.PRINT_AXIOMS_DIRNAME: sync.PRINT_AXIOMS_CACHE_VERSION,
        sync.LOCAL_CLOSURE_AXIOMS_DIRNAME: sync.LOCAL_CLOSURE_AXIOMS_CACHE_VERSION,
    }
    pattern = re.compile(r'[a-f0-9]{64}(?:-principal-[a-f0-9]{64})?(?:-premises-[a-f0-9]{64})?(?:-noax)?')
    remaining = _PROBE_SEED_MAX_BYTES
    entries_remaining = _PROBE_SEED_MAX_ENTRIES
    for previous in islice(previous_rounds, _PROBE_SEED_MAX_DONORS):
        if remaining <= 0 or entries_remaining <= 0:
            break
        previous = Path(previous)
        if previous == directory or previous.parent != directory.parent or not previous.name.isdigit():
            continue
        copies = {}
        try:
            from .publication_artifacts import lexical_authority
            from trellis.checker.portable_replay import DIRECTORY
            portable = (lexical_authority(previous / 'repo') / DIRECTORY).is_dir()
            if not portable and not any((previous / 'runtime/checker-state' / name).is_dir() for name in namespaces):
                continue
            old_key = json.loads(regular(previous / 'completed.json'))['key']
            if (not isinstance(old_key, dict)
                    or dict(old_key, acceptance_sha256=current_key['acceptance_sha256']) != current_key
                    or reusable(previous, old_key) is None):
                continue
            for namespace, version in namespaces.items():
                if remaining <= 0 or entries_remaining <= 0:
                    break
                source = previous / 'runtime/checker-state' / namespace
                if not source.is_dir() or source.is_symlink():
                    continue
                for path in _probe_cache_entries(source, entries_remaining):
                    entries_remaining -= 1
                    if path.suffix != '.json' or not pattern.fullmatch(path.stem):
                        continue
                    try:
                        if path.lstat().st_size > min(16 << 20, remaining):
                            continue
                        data = regular(path)
                        record = json.loads(data)
                        if (not isinstance(record, dict) or record.get('cache_version') != version
                                or record.get('key_blob_sha256') != path.stem):
                            continue
                        if namespace == sync.SEMANTIC_PAYLOAD_DIRNAME:
                            successful = record.get('ok') is True and isinstance(record.get('payload'), str) and not record.get('error')
                        else:
                            result = record.get('response') if namespace == sync.LOCAL_CLOSURE_AXIOMS_DIRNAME else record
                            successful = (isinstance(result, dict) and result.get('returncode') == 0
                                and not result.get('timed_out') and not result.get('spawn_error')
                                and (namespace != sync.LOCAL_CLOSURE_AXIOMS_DIRNAME or result.get('status') == 'ok'))
                        if not successful:
                            continue
                        target = directory / 'runtime/checker-state' / namespace / path.name
                        if target.exists() or target.is_symlink():
                            continue  # Never overwrite evidence already obtained here.
                        # A failed directory fsync may follow a successful
                        # rename, so attempted copies consume the budget too.
                        remaining -= len(data)
                        atomic(target, data)
                        copies[namespace] = copies.get(namespace, 0) + 1
                    except (OSError, ValueError, candidate.CandidateError):
                        continue
            if portable:
                # Protected host preparation, outside all agent mounts. Reuse
                # the authentication above rather than parsing another huge
                # protocol snapshot. The checker later validates actual bytes.
                write_once(directory / 'replay-cache-donor.json', dict(version=1,
                    donor_round=previous.name,
                    donor_completion_sha256=candidate.digest(regular(previous / 'completed.json')),
                    current_key_sha256=candidate.digest(candidate.canonical_json(current_key))))
            if copies:
                write_once(directory / 'probe-cache-seed.json', dict(version=1,
                    donor_round=previous.name,
                    donor_completion_sha256=candidate.digest(regular(previous / 'completed.json')),
                    copied=copies))
                return copies
            if portable:
                return {}
        except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError):
            if copies:
                return copies  # A missing advisory summary does not authorize more copying.
            continue  # Optional transfer failure leaves the ordinary cold path.
    return {}


def differences(before, after, *, limit=48, max_path=512):
    """Bounded structural paths and hashes only: never log private field values."""
    rows = []
    truncated = False
    def summary(value, present=True):
        data = candidate.canonical_json(value) if present else b''
        return dict(present=present, sha256=candidate.digest(data), size_bytes=len(data))
    def walk(left, right, pointer='', lp=True, rp=True):
        nonlocal truncated
        if lp == rp and left == right:
            return
        if len(rows) >= limit:
            truncated = True
            return
        if lp and rp and isinstance(left, dict) and isinstance(right, dict):
            for name in sorted(left.keys() | right.keys()):
                segment = str(name).replace('~', '~0').replace('/', '~1')
                walk(left.get(name), right.get(name), pointer + '/' + segment, name in left, name in right)
        elif lp and rp and isinstance(left, list) and isinstance(right, list) and len(left) == len(right):
            for index, (a, b) in enumerate(zip(left, right)):
                walk(a, b, pointer + '/' + str(index))
        else:
            rows.append(dict(path=pointer[:max_path], path_sha256=candidate.digest(pointer.encode()),
                captured=summary(left, lp), current=summary(right, rp)))
    walk(before, after)
    return dict(differences=rows, truncated=truncated,
        captured_sha256=candidate.digest(candidate.canonical_json(before)),
        current_sha256=candidate.digest(candidate.canonical_json(after)))


def reject(directory, *, task_id, identity, task, captured, source, private, native, now_ms, state):
    from .driver import acceptance_identity
    current_key = key(task_id, identity, task, acceptance_identity(private), source)
    old = parse_private_state(regular(Path(directory) / 'runtime/protocol_state.json'))
    def normalized(value):
        value = dict(value, request_seq=0)
        if value.get('in_flight_request'):
            value['in_flight_request'] = dict(value['in_flight_request'], id=0)
        return value
    evidence = dict(version=1, task_id=task_id, round_id=Path(directory).name,
        candidate_sha256=identity, observed_at_ms=now_ms,
        implementation_identity=current_key['implementation_identity'],
        captured_acceptance_sha256=captured.get('acceptance_sha256'),
        current_acceptance_sha256=current_key['acceptance_sha256'],
        key=differences(captured.get('key'), current_key),
        acceptance=differences(normalized(old), normalized(private)),
        inputs=differences(captured['source']['input_hashes'], source['input_hashes']),
        canonical_state_sha256=candidate.digest(candidate.canonical_json(state)),
        observation_cache=getattr(native, 'last_source_observation', None))
    # A round may be reconsidered after a transient mismatch. Each actual
    # observation remains independently immutable and bounded.
    name = candidate.digest(candidate.canonical_json(evidence))
    write_once(Path(directory) / 'rejections' / (name + '.json'), evidence)

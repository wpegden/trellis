"""One host acceptance process, owned and reaped like every compiler caller."""
from __future__ import annotations

import contextlib
import json
from trellis.private_state import read_private_state
from pathlib import Path
import sys

from . import candidate
from .publication import atomic

class StaleInput(candidate.CandidateError):
    """An authenticated candidate reads bytes that no longer match its task."""


GENERATED = frozenset({'Tablet/INDEX.md', 'Tablet/README.md', 'Tablet/header.tex'})


def proof_prefix_digest(data):
    """Hash the unique BODY prefix with the native FILESPEC line boundary."""
    if data is None:
        return None
    try:
        content = data.decode('utf-8')
    except UnicodeDecodeError:
        return None
    offset, marker_end = 0, None
    for line in content.split('\n'):
        end = min(len(content), offset + len(line) + 1)
        if line.rstrip('\r').strip() == '-- BODY':
            if marker_end is not None:
                return None
            marker_end = end
        offset = end
    return None if marker_end is None else candidate.digest(content[:marker_end].encode('utf-8'))


def validate_overlay(record, repo, captured, proof_body_rebase_inputs=None):
    """Check captured inputs and write conflicts without changing source."""
    prefixes = proof_body_rebase_inputs or {}
    for path, expected in record['inputs'].items():
        actual = captured.get(path, 'absent' if expected == 'absent' else None)
        if actual != expected:
            # A prefix grants only eligibility for fresh acceptance, never a
            # certificate. Owned candidate writes retain exact before hashes.
            data = candidate.regular_bytes(repo, path) if path in prefixes else None
            if (path in record['changes'] or path not in prefixes or data is None
                    or candidate.digest(data) != actual
                    or proof_prefix_digest(data) != prefixes[path]):
                raise StaleInput(f'stale task input: {path}')
    for path, change in record['changes'].items():
        data = candidate.regular_bytes(repo, path)
        actual = None if data is None else candidate.digest(data)
        if actual != change['before'] or captured.get(path) != change['before']:
            raise candidate.CandidateError(f'candidate source conflicts with captured work: {path}')


def overlay(sealed, repo, captured, proof_body_rebase_inputs=None):
    """Overlay on captured current inputs; publication rechecks canonical."""
    record, identity = candidate.read(sealed)
    validate_overlay(record, repo, captured, proof_body_rebase_inputs)
    for path, change in record['changes'].items():
        destination = Path(repo) / path
        if change['after'] is None:
            destination.unlink()
        else:
            atomic(destination, candidate.regular_bytes(Path(sealed) / 'files', path))
    return record, identity


def unchanged_closure_stuck(repo, state, record, captured, prepared_config_sha256=None):
    """Cheap closure Stuck rejection against the unchanged captured baseline.

    The parent has checked the sealed candidate's scope. Partial proof edits
    stay sealed and are never overlaid, published or certified on this path.
    False eligibility falls back to full acceptance; publication still checks
    current task/input identities and invokes the native failure transition.
    """
    request = state['in_flight_request']
    raw = record.get('response')
    if (request.get('phase') != 'ProofFormalization'
            or (request.get('task_execution') or {}).get('task_kind') != 'lean_closure'
            or not isinstance(raw, dict) or raw.get('outcome') != 'stuck'
            or any(captured.get(path) != expected for path, expected in record['inputs'].items())):
        return None
    node = request.get('active_node')
    if not node:
        return None
    # Canonical inputs remain the publication/reuse identity. Only the host's
    # authenticated preparation may supply relocated private carrier bytes.
    prepared = dict(captured)
    if prepared_config_sha256 is not None and 'trellis.config.json' in prepared:
        prepared['trellis.config.json'] = prepared_config_sha256
    # Check every source and recorded absence, including immutable carriers.
    # A prefix-compatible dependency edit deliberately takes the full path.
    before = candidate.source_manifest(repo)
    if any(captured.get(path) != digest for path, digest in before.items()):
        return None
    for path, expected in prepared.items():
        data = candidate.regular_bytes(repo, path)
        if ('absent' if data is None else candidate.digest(data)) != expected:
            return None
    from trellis.runtime.kernel_cli import run_kernel_cli
    checked = run_kernel_cli(dict(action='check_unchanged_closure_stuck', repo_path=str(repo),
        request=request, baseline=state['live'], raw_payload=raw))
    if checked.get('status') != 'dispatch_ok':
        raise RuntimeError('unexpected unchanged closure response from native checker')
    result = checked.get('output')
    if result is None:
        return None
    if candidate.source_manifest(repo) != before:
        raise candidate.CandidateError('source changed during unchanged closure acceptance')
    for path, expected in prepared.items():
        data = candidate.regular_bytes(repo, path)
        if ('absent' if data is None else candidate.digest(data)) != expected:
            raise candidate.CandidateError('input changed during unchanged closure acceptance: ' + path)
    response = result.get('response')
    if not isinstance(response, dict):
        return None
    if response.get('acceptance_check_passed'):
        raise RuntimeError('unchanged closure failure cannot certify proof progress')
    if record['changes']:
        print('[acceptance] closure Stuck: partial edits retained as unaccepted evidence; '
              'no proof checking or source publication', file=sys.stderr)
    result['acceptance_path'] = 'unchanged_closure_stuck'
    return result


def check(payload):
    from trellis.checker.resource_failure import host_scope
    with host_scope(payload['runtime_root']):
        return _check(payload)


def _check(payload):
    from trellis.checker.resource_failure import require_healthy, memory_phase
    from trellis.checking import build_trellis_worker_acceptance_context, normalize_trellis_worker_result_data
    from trellis.config import load_config
    from trellis.supervisor_workspace import sync_supervisor_workspace, propagate_tablet_back_to_worker
    config = load_config(Path(payload['config_path']))
    repo = config.repo_path
    state = read_private_state(Path(payload['runtime_root']) / 'protocol_state.json')
    request = state['in_flight_request']
    require_healthy(payload['runtime_root'])
    record, identity = candidate.read(payload['candidate'])
    memory_phase(payload['runtime_root'], 'baseline', identity)
    # Even transport failures are ordinary worker responses, never a source
    # transaction. Preserve their existing bounded retry classification.
    if record['response'] is None:
        response = dict(record.get('bridge_response') or {})
        if response.get('status') != 'Malformed':
            raise RuntimeError('worker returned neither raw output nor a Malformed transport response')
        response.update(request_id=request['id'], cycle=request['cycle'])
        return dict(response=response, candidate_sha256=identity)
    result = unchanged_closure_stuck(repo, state, record, payload['captured_inputs'],
                                     payload.get('prepared_config_sha256'))
    if result is not None:
        result['candidate_sha256'] = identity
        atomic(Path(payload['runtime_root']) / 'acceptance-result.json', candidate.canonical_json(result))
        return result
    # A candidate may have waited behind a dependency's publication. Reject
    # stale inputs before building the expensive authoritative baseline. The
    # overlay repeats these same guards after baseline preparation.
    try:
        validate_overlay(record, repo, payload['captured_inputs'], payload.get('proof_body_rebase_inputs'))
    except candidate.CandidateError as exc:
        return dict(candidate_sha256=identity, stale_reason=str(exc))
    authority = Path(sync_supervisor_workspace(repo)['authoritative_repo_path']).resolve()
    before = build_trellis_worker_acceptance_context(authority, request,
        paper_source_path=config.workflow.paper_tex_path, goal_prose_path=getattr(config, 'goal_file', None))
    require_healthy(payload['runtime_root'])
    if not before.get('ok') or not isinstance(before.get('data'), dict):
        raise RuntimeError('authoritative acceptance baseline failed: ' + '; '.join(before.get('errors', [])))
    context_bytes = candidate.canonical_json(before['data'])
    context_sha256 = candidate.digest(context_bytes)
    atomic(Path(payload['runtime_root']) / 'acceptance-context.json', context_bytes)
    del context_bytes
    propagate_tablet_back_to_worker(authority, repo, mirror_build=False)
    try:
        overlay(payload['candidate'], repo, payload['captured_inputs'], payload.get('proof_body_rebase_inputs'))
    except candidate.CandidateError as exc:
        return dict(candidate_sha256=identity, stale_reason=str(exc))
    memory_phase(payload['runtime_root'], 'candidate', identity)
    authority = Path(sync_supervisor_workspace(repo)['authoritative_repo_path']).resolve()
    # Baseline remains canonical. Portable artifacts/facts enter only after the
    # immutable candidate overlay, through the checker-owned writer lock.
    from .replay_donation import install_via_checker, validation_fact_donor, validation_fact_context
    donation = install_via_checker(Path(payload['runtime_root']))
    donor = validation_fact_donor(Path(payload['runtime_root']), donation)
    fact_context = validation_fact_context(Path(payload['runtime_root']))
    result = normalize_trellis_worker_result_data(record['response'], repo=authority,
        acceptance_context=before['data'], **({'validation_fact_donor': donor} if donor else {}),
        **({'validation_fact_context': fact_context} if fact_context else {}))
    require_healthy(payload['runtime_root'])
    if donation is not None:
        result['replay_donation'] = donation
    result['acceptance_context_sha256'] = context_sha256
    # Native diagnostics come only from this host's freshly prepared gate and
    # checker output. Never copy a worker-authored trace from its payload/context.
    result['acceptance_read_trace'] = dict(version=1, producer='host_acceptance_native',
        baseline=before['data'].get('acceptance_read_trace'),
        check=result.get('acceptance_read_trace'))
    propagate_tablet_back_to_worker(authority, repo, mirror_build=False)
    response = result.get('response')
    if not isinstance(response, dict):
        # Keep the ordinary normalizer's malformed/transport classification and
        # complete evidence instead of turning an agent artifact into a host
        # crash. The ordinary native retry policy consumes this response.
        from trellis.runtime.bridge import _save_worker_normalization_failure
        response = _save_worker_normalization_failure(runtime_root=Path(payload['runtime_root']),
            request=request, raw_payload=record['response'],
            acceptance_context_path=Path(payload['runtime_root']) / 'acceptance-context.json',
            authoritative_repo_path=repo, normalized_result=result)
    response['kind'] = 'worker'
    from .enrichment import enrich
    response = enrich(Path(payload['runtime_root']), response)
    require_healthy(payload['runtime_root'])
    result['response'] = response
    result['candidate_sha256'] = identity
    atomic(Path(payload['runtime_root']) / 'acceptance-result.json', candidate.canonical_json(result))
    return result


def main():
    payload = json.load(sys.stdin)
    with contextlib.redirect_stdout(sys.stderr):
        result = check(payload)
        if not result.get('stale_reason'):
            from .acceptance_receipts import checked_sources
            result['checked_sources'] = checked_sources(Path(payload['config_path']).parent)
    json.dump(result, sys.stdout)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())

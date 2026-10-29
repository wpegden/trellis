"""Journal exact host supervisor artifacts as an optional publication cache.

Only replay-attested artifacts produced in the reaped acceptance workspace are
eligible. Native publication still validates every byte-bound certificate and
its current dependencies; a cache miss falls back to ordinary issuance.
"""
from pathlib import Path
import json
import re

from . import candidate
from .acceptance_receipts import regular


_NAME = re.compile(r'[A-Za-z_][A-Za-z0-9_\']*\Z')
_SUFFIXES = ('.olean', '.olean.server', '.olean.private', '.olean.srcclosure',
    '.olean.hash', '.olean.server.hash', '.olean.private.hash', '.ilean', '.ilean.hash', '.ir', '.ir.hash', '.trace')


def paths(node):
    if not isinstance(node, str) or not _NAME.fullmatch(node):
        raise candidate.CandidateError('invalid publication artifact owner')
    return [f'lib/lean/Tablet/{node}{suffix}' for suffix in _SUFFIXES] + [
        f'ir/Tablet/{node}.c', f'ir/Tablet/{node}.c.hash']


def lexical_authority(repo):
    # Reject aliases before the normal path helper resolves them. Checking the
    # resolved destination alone would miss a symlinked supervisor ancestor.
    repo = Path(repo).absolute()
    authority = repo / '.trellis/supervisor/repo'
    for path in reversed((authority, *authority.parents)):
        if path.is_symlink():
            raise candidate.CandidateError('supervisor artifact authority traverses a symlink')
    return authority


def records(response):
    return dict((response.get('local_closure_revalidation') or {}).get('refreshed', []))


def stage(staged, canonical, journal, response):
    from trellis.atomic_actions import observations as obs
    from trellis.checker import portable_replay as portable
    authority = lexical_authority(staged)
    if authority == staged or not authority.is_relative_to(staged):
        raise candidate.CandidateError('artifact publication requires a private supervisor')
    snapshot = obs.SourceClosureSnapshot(authority)
    nodes = {}
    environments = {}
    environment_bytes = 0
    for node, record in records(response).items():
        expected = (record.get('node_certificate') or {}).get('artifact_bundle')
        if not expected:
            continue  # Backend/legacy records with no Lean certificate.
        names = paths(node)
        try:
            contents = {}
            for name in names:
                path = authority / '.lake/build' / name
                contents[name] = regular(path) if path.exists() or path.is_symlink() else None
            if (obs._artifact_bundle(authority, node) != expected
                    or not obs.olean_is_content_current(authority, node, source_snapshot=snapshot)
                    or not obs.olean_has_current_kernel_replay(authority, node)):
                continue
            for name, data in contents.items():
                if data is not None:
                    candidate._write(Path(journal) / 'artifacts' / name, data)
            nodes[node] = dict(source_closure_sha256=snapshot.closure_hash(node), artifact_bundle=expected,
                files={name: candidate.digest(data) if data is not None else None for name, data in contents.items()})
            identity = (obs._read_olean_provenance(authority, node).get('kernel_replay') or {}).get('portable_environment_sha256')
            if identity and identity not in environments and len(environments) < 128:
                try:
                    environment = portable.environment_for(authority, identity)
                    data = portable._json(environment)
                    if environment_bytes + len(data) <= 64 << 20:
                        from .publication import atomic
                        atomic(Path(journal) / portable.DIRECTORY / (identity + '.json'), data)
                        environments[identity] = True
                        environment_bytes += len(data)
                except (OSError, ValueError, candidate.CandidateError):
                    pass  # Legacy/incomplete portability keeps ordinary replay.
        except (OSError, candidate.CandidateError, obs.SourceClosureSnapshotTooLarge):
            # No authority is inferred from an incomplete or shared-writable
            # build. Leave this node cold for the native issuer.
            continue
    if not snapshot.is_current():
        return dict(version=1, nodes={})
    return dict(version=1, nodes=nodes,
        **({'portable_environments': sorted(environments)} if environments else {}))


def validate(journal, handoff):
    if handoff.get('version') != 1:
        raise candidate.CandidateError('unsupported publication artifact handoff')
    for node, entry in handoff['nodes'].items():
        if set(entry['files']) != set(paths(node)):
            raise candidate.CandidateError('publication artifact inventory is incomplete')
        for name, expected in entry['files'].items():
            path = Path(journal) / 'artifacts' / name
            if expected is not None and candidate.digest(regular(path)) != expected:
                raise candidate.CandidateError('publication artifact evidence changed: ' + name)
    from trellis.checker import portable_replay as portable
    identities = handoff.get('portable_environments', [])
    if not isinstance(identities, list) or len(identities) > 128:
        raise candidate.CandidateError('invalid portable publication inventory')
    total = 0
    for identity in identities:
        environment = portable.environment_for(Path(journal), identity)
        total += len(portable._json(environment))
        if total > 64 << 20:
            raise candidate.CandidateError('portable publication inventory exceeds limit')


def install(repo, journal, handoff, checkpoint):
    from trellis.atomic_actions import observations as obs
    from trellis.supervisor_workspace import sync_supervisor_workspace
    from .publication import atomic
    validate(journal, handoff)
    if not handoff['nodes']:
        return
    # The normal supervisor synchronizer establishes current canonical sources
    # and pinned package inputs before any copied provenance can be consumed.
    authority = lexical_authority(repo)
    if Path(sync_supervisor_workspace(repo)['authoritative_repo_path']) != authority:
        raise candidate.CandidateError('canonical artifact authority changed')
    snapshot = obs.SourceClosureSnapshot(authority)
    installed = []
    for node, entry in handoff['nodes'].items():
        if snapshot.closure_hash(node) != entry['source_closure_sha256']:
            continue  # Changed policy/source/toolchain: ordinary cold rebuild.
        for name, expected in entry['files'].items():
            destination = authority / '.lake/build' / name
            for parent in (destination, *destination.parents):
                if parent.is_symlink():
                    raise candidate.CandidateError('artifact destination traverses a symlink')
                if parent == authority:
                    break
            if expected is None:
                if destination.exists():
                    destination.unlink()
                    candidate._sync_dir(destination.parent)
            else:
                # Atomic writes create independent regular inodes. Read-only
                # provenance overlays are established by the ordinary compiler.
                atomic(destination, regular(Path(journal) / 'artifacts' / name))
            checkpoint('artifact:' + name)
        if (not obs.olean_is_content_current(authority, node, source_snapshot=snapshot)
                or not obs.olean_has_current_kernel_replay(authority, node)
                or obs._artifact_bundle(authority, node) != entry['artifact_bundle']):
            obs._purge_olean_artifacts(authority, node)
        else:
            installed.append(node)
    closure = snapshot.materialization_order(installed)
    if not snapshot.is_current() or any(
            not obs.olean_is_content_current(authority, node, source_snapshot=snapshot)
            or not obs.olean_has_current_kernel_replay(authority, node) for node in closure):
        for node in installed:
            obs._purge_olean_artifacts(authority, node)
        return
    # Exact portable import environments can cross roots. Validate them once
    # per protected operation; a miss still takes ordinary independent replay.
    from trellis.checker import portable_replay as portable
    retained = set()
    identities = handoff.get('portable_environments', [])
    portable_owners = [node for node in installed
        if (obs._read_olean_provenance(authority, node).get('kernel_replay') or {}).get('portable_environment_sha256') in identities]
    fresh, _stats = portable.capture(authority, portable_owners,
        timeout_secs=portable.CAPTURE_SECONDS, bwrap_role='lake_compiler') if portable_owners else (None, {})
    if fresh is not None:
        fresh_identity = portable.digest(fresh)
        atomic(authority / portable.DIRECTORY / (fresh_identity + '.json'), portable._json(fresh))
        current_view = portable.EnvironmentView(fresh)
    for identity in handoff.get('portable_environments', []):
        try:
            environment = portable.environment_for(Path(journal), identity)
            owners = [node for node in installed
                      if (obs._read_olean_provenance(authority, node).get('kernel_replay') or {}).get('portable_environment_sha256') == identity]
            if fresh is None:
                continue
            before_view = portable.EnvironmentView(environment)
            for node in owners:
                record = obs._read_olean_provenance(authority, node)
                # Use the same exact-byte adapter relocation rule as donation.
                # Installation paths alone do not change imported proof meaning.
                if before_view.same_owner(current_view, node):
                    rebound = portable.rebind_record(authority, node, record, fresh)
                    rebound['kernel_replay']['portable_environment_sha256'] = fresh_identity
                    if obs._write_olean_provenance(authority, node,
                            rebound):
                        retained.add(node)
        except (OSError, ValueError, KeyError, RecursionError, candidate.CandidateError):
            continue
    # No Lean elaboration or certificate probe is substituted here. Legacy or
    # incompatible environments still establish their imported-module boundary.
    for node in installed:
        if node in retained:
            continue
        provenance = obs._read_olean_provenance(authority, node)
        provenance.pop('kernel_replay', None)
        atomic(obs._olean_srcclosure_sidecar_path(authority, node), candidate.canonical_json(provenance))
    replay = obs._kernel_replay_tablet_modules(authority, closure,
        timeout_secs=3600, bwrap_role='lake_compiler', capture_portable=True)
    if replay.get('returncode') != 0 or not snapshot.is_current():
        for node in installed:
            obs._purge_olean_artifacts(authority, node)
    checkpoint('artifacts_replayed')


def authorized(server, token):
    # Unlike ordinary diagnostic operations, publication never accepts the
    # dormant UID-only mode or an agent/advisory burst token.
    try:
        registry = json.loads(regular(server.burst_tokens_path))
        return (isinstance(token, str) and registry.get('runtime_root') == str(server.runtime_root.resolve())
            and token in registry.get('tokens', [])
            and any(entry.get('token') == token and entry.get('kind') == 'supervisor'
                    for entry in registry.get('entries', []) if isinstance(entry, dict)))
    except (OSError, ValueError, candidate.CandidateError):
        return False


def install_pending(server):
    """Only the canonical checker owns the build/probe exclusion lock."""
    root = server.runtime_root / 'dispatch/publication'
    # Read the pointer after waiting for active probes/builds. A timed-out
    # request must not install its old packet after a later native commit.
    with server._workspace_lock, server._sync_lock:
        pending = json.loads(regular(root / 'pending.json'))
        identity = pending.get('identity')
        if not isinstance(identity, str) or not re.fullmatch(r'[a-f0-9]{64}', identity):
            raise candidate.CandidateError('invalid artifact publication journal')
        journal = root / identity
        raw = regular(journal / 'record.json')
        record = json.loads(raw)
        if (candidate.digest(raw) != identity or record.get('version') != 2
                or record.get('repo') != str(server.worker_repo.resolve())):
            raise candidate.CandidateError('artifact publication belongs to a different journal/repository')
        if candidate.source_manifest(server.worker_repo) != record['after']:
            raise candidate.CandidateError('artifact publication sources are not installed')
        from trellis.runtime.kernel_cli import run_kernel_cli
        result = run_kernel_cli(dict(action='dispatch_publication_receipt', root=str(server.runtime_root),
            payload=record['kernel_event']))
        if result.get('status') != 'dispatch_ok':
            raise candidate.CandidateError('native publication receipt query failed')
        if result['output'] is not None:
            return dict(identity=identity)
        nodes = list(record['artifacts']['nodes'])
        server._invalidate_compile_cache_for_sync([name + '.lean' for name in nodes])
        install(server.worker_repo, journal, record['artifacts'], lambda _: None)
    return dict(identity=identity)


def install_via_checker(runtime, identity):
    from trellis.atomic_actions.checker_client import _round_trip, _next_request_id
    result = _round_trip(Path(runtime) / 'sockets/checker.sock',
        dict(op='install_publication_artifacts', request_id=_next_request_id()), timeout_secs=3600)
    if result.get('identity') != identity:
        raise candidate.CandidateError('checker installed a different publication journal')

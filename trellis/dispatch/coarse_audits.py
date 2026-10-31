"""Scoped audit inputs and verified history views; no second scheduling service."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import subprocess

from .candidate import canonical_json
from .publication import atomic


def input_in_scope(path, context):
    if path in ('Tablet/INDEX.md', 'Tablet/README.md'):
        return False
    if path.startswith('Tablet/'):
        name = path[len('Tablet/'):]
        for suffix in ('.lean', '.tex'):
            if name.endswith(suffix):
                node = name[:-len(suffix)]
                return node in context['nodes'] or node in ('Preamble', 'Axioms')
    if path.startswith('process-memory/'):
        return path.split('/')[1] in ('global', *context['goals'])
    return not path.startswith(('.trellis/', '.trellis-history/'))


def read_history(repo, reference):
    digest, size = reference['sha256'], reference['bytes']
    if (type(size) is not int or not 0 < size <= 8 * 1024 * 1024
            or len(digest) != 64 or any(c not in '0123456789abcdef' for c in digest)):
        raise ValueError('invalid coarse audit history reference')
    relative = f'.trellis-history/coarse-audits/{digest}.json'
    path = Path(repo) / relative
    if any(p.is_symlink() for p in (path.parent.parent, path.parent, path)):
        raise ValueError('audit history cannot follow symlinks')
    if path.is_file():
        if path.stat().st_size != size:
            raise ValueError('coarse audit history has wrong size')
        data = path.read_bytes()
    else:
        size_result = subprocess.run(['git', '-C', str(repo), 'cat-file', '-s', f'HEAD:{relative}'], capture_output=True)
        if size_result.returncode or size_result.stdout.strip() != str(size).encode():
            raise ValueError(f'coarse audit history has wrong committed size: {digest}')
        result = subprocess.run(['git', '-C', str(repo), 'cat-file', 'blob', f'HEAD:{relative}'],
                                capture_output=True)
        if result.returncode:
            raise ValueError(f'coarse audit history unavailable: {digest}')
        data = result.stdout
    if len(data) != size or hashlib.sha256(data).hexdigest() != digest:
        raise ValueError('coarse audit history has wrong size or digest')
    report = json.loads(data)
    if report.get('version') != 1:
        raise ValueError('unsupported coarse audit history version')
    if bool(report.get('evidence')) == bool(report.get('migrated_from')):
        raise ValueError('audit history must have a panel identity or a migration predecessor')
    return report, data


def materialize_recent(repo, private_state, *, limit=3):
    """Copy a bounded recent view. Older reports stay in the shared Git objects."""
    request = private_state['in_flight_request']
    scope = request.get('scoped_audit')
    goals = (private_state.get('coarse_goals') or {}).get('goals', {})
    selected = scope['goals'] if scope else (
        [private_state['coarse_task_goal']] if private_state.get('coarse_task_goal') else list(goals))
    index = {}
    for node in selected:
        reference = (scope.get('previous', {}).get(node) if scope else goals.get(node, {}).get('history_head'))
        rows = []
        for _ in range(limit):
            if reference is None:
                break
            report, data = read_history(repo, reference)
            if node not in report['context']['goals']:
                raise ValueError('audit report belongs to a different coarse goal')
            relative = f'.trellis/coarse-audit-history/{reference["sha256"]}.json'
            atomic(Path(repo) / relative, data)
            rows.append(dict(path=relative, reference=reference,
                             cycle=report['plan']['written_at_cycle'],
                             request_id=report['plan']['written_by_request']))
            reference = report['context']['previous'].get(node)
        if rows:
            index[node] = dict(recent=rows, older=reference)
    if index:
        atomic(Path(repo) / '.trellis/coarse-audit-history/index.json', canonical_json(index))


def restrict_evidence(repo, context):
    """Use the existing private workspace, with only the declared source view.

    Git history remains historical context. Authoritative freshness checks bind
    current evidence, and global actions require the ordinary run-wide audit.
    """
    repo = Path(repo)
    for surface in ('Tablet', 'process-memory'):
        root = repo / surface
        if not root.is_dir():
            continue
        for path in root.rglob('*'):
            if path.is_file() and not input_in_scope(path.relative_to(repo).as_posix(), context):
                path.unlink()
    atomic(repo / '.trellis/audit-evidence.json', canonical_json(context))

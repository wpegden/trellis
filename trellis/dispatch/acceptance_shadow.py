"""Offline comparison of native diagnostics in two host-completed rounds.

This is not a cache/freshness decision. The existing completed.json is trusted
host evidence; its pinned files are checked without re-running source scans or
native RPCs. Historical source/artifact freshness is deliberately NOT asserted.
Worker workspaces cannot create these host completion receipts. Never invoke
this helper under the publication lock or use its result to approve a response.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import stat

from . import acceptance_receipts as receipts, candidate

_MAX_BYTES = 1 << 30  # Offline pair ceiling; callers may explicitly increase it.
_MAX_JSON_BYTES = 16 << 20  # Parsed completion/canonical/response JSON only.
_HASH_CHUNK_BYTES = 1 << 20
_MAX_TRACE_BYTES = 64 << 10
_MAX_GROUPS = 16


def _completed(directory, budget):
    directory = Path(directory).absolute()

    def read(path, *, parse=False):
        for parent in (path, *path.parents):
            if parent.is_symlink():
                raise ValueError('shadow evidence must not traverse a symlink')
        if not path.exists():
            from .retained_files import chunks, logical_size
            size = logical_size(path)
            if size > budget[0] or (parse and size > _MAX_JSON_BYTES):
                raise ValueError('shadow retained evidence exceeds its byte limit')
            digest = hashlib.sha256()
            data = bytearray() if parse else None
            for chunk in chunks(path):
                budget[0] -= len(chunk)
                if budget[0] < 0:
                    raise ValueError('shadow pair byte budget exceeded')
                digest.update(chunk)
                if parse:
                    data.extend(chunk)
                    if len(data) > _MAX_JSON_BYTES:
                        raise ValueError('shadow parsed JSON exceeds its byte limit')
            return digest.hexdigest(), data
        # Reject devices/FIFOs before opening; recheck the opened regular file.
        info = path.stat()
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise ValueError('shadow evidence must be an independent regular file')
        # Hash the opened file's actual bytes, never a metadata identity or an
        # alias of writable evidence. Large protocol snapshots stay streamed.
        with path.open('rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > budget[0]:
                raise ValueError('shadow evidence is nonregular or exceeds the pair byte budget')
            if parse and info.st_size > _MAX_JSON_BYTES:
                raise ValueError('shadow parsed JSON exceeds its byte limit')
            digest = hashlib.sha256()
            data = bytearray() if parse else None
            while True:
                limit = min(_HASH_CHUNK_BYTES, budget[0] + 1)
                if parse:
                    limit = min(limit, _MAX_JSON_BYTES - len(data) + 1)
                chunk = stream.read(limit)
                if not chunk:
                    break
                budget[0] -= len(chunk)
                if budget[0] < 0:
                    raise ValueError('shadow pair byte budget exceeded')
                digest.update(chunk)
                if parse:
                    data.extend(chunk)
                    if len(data) > _MAX_JSON_BYTES:
                        raise ValueError('shadow parsed JSON exceeds its byte limit')
        return digest.hexdigest(), data

    _, raw = read(directory / 'completed.json', parse=True)
    saved = json.loads(raw)
    del raw
    key = saved['key']
    if (saved.get('version') != 1 or set(saved['files']) != set(receipts._RECEIPTS)
            or directory.parent.name != 'acceptance'
            or directory.parent.parent.name != str(key['task_id'])
            or (directory / 'retry-authorized.json').exists()):
        raise ValueError('not a supported immutable host completion')
    values = {}
    for name in receipts._RECEIPTS:
        parsed = name in ('canonical.json', 'execution/response.json')
        digest, data = read(directory / name, parse=parsed)
        if digest != saved['files'][name]:
            raise ValueError('host completion evidence changed: ' + name)
        if parsed:
            values[name] = json.loads(data)
        del data
    checked = values['execution/response.json']
    if (values['canonical.json']['key'] != key
            or checked.get('candidate_sha256') != key['candidate_sha256']
            or checked.get('stale_reason') or not isinstance(checked.get('response'), dict)):
        raise ValueError('completion disagrees with candidate/key')
    trace = checked.get('acceptance_read_trace')
    if trace is None:
        return key, None
    if (len(candidate.canonical_json(trace)) > _MAX_TRACE_BYTES
            or trace.get('version') != 1 or trace.get('producer') != 'host_acceptance_native'):
        raise ValueError('unsupported host trace envelope')
    return key, trace


def _groups(trace):
    if trace is None:
        return {}
    groups = {}
    for stage in ('baseline', 'check'):
        native = trace.get(stage)
        if native is None:
            continue
        if (native.get('version') != 1 or native.get('coverage') != 'PARTIAL'
                or native.get('authority') != 'diagnostic_only'):
            raise ValueError('unsupported native trace')
        rows = native['groups']
        if not isinstance(rows, list) or len(rows) > _MAX_GROUPS:
            raise ValueError('too many native groups')
        for row in rows:
            name = row['name']
            if not isinstance(name, str) or not re.fullmatch(r'[a-z_.]{1,80}', name):
                raise ValueError('invalid native group name')
            identity = stage + '.' + name
            if identity in groups:
                raise ValueError('duplicate native group')
            groups[identity] = row
    return groups


def _digest(operand):
    if not isinstance(operand, dict) or operand.get('status') != 'recorded':
        return None
    value = operand.get('sha256')
    return value if isinstance(value, str) and re.fullmatch('[0-9a-f]{64}', value) else None


def _success_fact(group):
    fact = group.get('success_fact')
    if (group.get('name') != 'validation.plan' or group.get('purpose') != 'observation'
            or group.get('status') != 'completed' or not isinstance(fact, dict)
            or fact.get('projection') != 'worker_validation_success_v1'
            or fact.get('status') != 'recorded_successful_steps'):
        return None
    return _digest(fact.get('digest'))


def compare_completed_rounds(before, after, *, max_bytes=None):
    """Read exactly two retained rounds; return bounded hashes/statuses, no writes.

    Equal operands with unequal results are a diagnostic discrepancy, never a
    mathematical Invalid (results can include location-dependent diagnostics).
    Native successful-fact agreement is reported separately from raw diagnostic
    differences. It remains partial shadow evidence, never reuse authority.
    Missing/budget-limited groups are inconclusive. Full
    candidate/task/implementation/source equality is required even in shadow.

    max_bytes caps total offline I/O (default 1 GiB), including both receipts
    and every pinned file. Large nonparsed files use 1 MiB chunks. Increasing
    max_bytes does not relax the separate 16 MiB parsed-JSON/64 KiB trace limits.
    """
    if Path(before).absolute().parent != Path(after).absolute().parent:
        raise ValueError('shadow pairing requires the same host task directory')
    if max_bytes is None:
        max_bytes = _MAX_BYTES
    if type(max_bytes) is not int or max_bytes < 1:
        raise ValueError('shadow pair byte budget must be a positive integer')
    budget = [max_bytes]
    old_key, old_trace = _completed(before, budget)
    new_key, new_trace = _completed(after, budget)
    # Match the existing donation boundary; this does not change key/reusable.
    stable_old = {k: v for k, v in old_key.items() if k != 'acceptance_sha256'}
    stable_new = {k: v for k, v in new_key.items() if k != 'acceptance_sha256'}
    if stable_old != stable_new:
        raise ValueError('shadow rounds differ in candidate/task/implementation/source binding')
    old, new = _groups(old_trace), _groups(new_trace)
    rows = []
    for name in sorted(old.keys() | new.keys()):
        left, right = old.get(name), new.get(name)
        status = 'inconclusive'
        facts = 'inconclusive'
        if left and right:
            metadata = ('source_phase', 'purpose', 'status')
            a, b = _digest(left.get('input')), _digest(right.get('input'))
            x, y = _digest(left.get('result')), _digest(right.get('result'))
            if all(left.get(k) == right.get(k) for k in metadata) and a and b:
                if a != b:
                    status = 'operands_changed'
                elif left.get('purpose') == 'refresh_only':
                    status = 'refresh_only_operands_equal'
                elif left.get('status') == 'completed' and x and y:
                    status = 'observations_equal' if x == y else 'equal_operands_different_results'
                if a == b:
                    old_fact, new_fact = _success_fact(left), _success_fact(right)
                    if old_fact and new_fact:
                        facts = 'successful_facts_equal' if old_fact == new_fact else 'successful_facts_differ'
        row = dict(group=name, status=status)
        if (left and 'success_fact' in left) or (right and 'success_fact' in right):
            row['success_fact_status'] = facts
        rows.append(row)
    return dict(version=1, coverage='PARTIAL', authority='diagnostic_only',
        acceptance_changed=old_key['acceptance_sha256'] != new_key['acceptance_sha256'],
        traces_present=old_trace is not None and new_trace is not None,
        authenticated_bytes=max_bytes - budget[0], groups=rows)

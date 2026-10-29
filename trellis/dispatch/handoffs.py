"""Bounded advisory files from completed workers for a reviewer or auditor."""
import json
from pathlib import Path, PurePosixPath
import stat

from trellis.worker_scratch import capture_reviewer_scratch, reviewer_scratch_paths
from .candidate import canonical_json, digest, safe_relative
from .publication import atomic
from .storage import execution_directories, worker_execution_coordinates
from .process import alive
from .review import evidence_paths


MAX_FILES = 32
MAX_BYTES = 32 * 1024 * 1024
MAX_REPORT_CHARS = 64 * 1024
MAX_FINDING_BYTES = 512 * 1024
MAX_FINDING_FILES = 8
MAX_EVIDENCE_FILES = 16
MAX_CANDIDATE_MANIFEST_BYTES = 16 * 1024 * 1024
MAX_SOURCE_BYTES = 128 * 1024


def reviewer_rationales(root, receipts):
    """Recent receipt-bound scheduling explanations, never current authority."""
    result = []
    for receipt in sorted(receipts, key=lambda row: row['review_id'])[-3:]:
        review_id = receipt['review_id']
        if type(review_id) is not int or review_id < 0:
            continue
        try:
            data, _ = _read(root, f'review-{review_id}/execution/response.json', 4 * 1024 * 1024)
            response = json.loads(data)
            if (not isinstance(response, dict) or response.get('kind') != 'parallel_review'
                    or digest(canonical_json(response)) != receipt.get('response_sha256')):
                continue
            reason = response.get('reason')
            if isinstance(reason, str) and reason.strip():
                result.append(dict(review_id=review_id, response_sha256=receipt['response_sha256'],
                    reason=reason[:2000], truncated=len(reason) > 2000))
        except (OSError, ValueError):
            continue  # Missing optional history does not block current work.
    return result


def _read(root, relative, limit, *, truncate=False):
    path = Path(root)
    for part in ('.', *Path(relative).parts):
        path = path / part
        if path.is_symlink():
            raise ValueError('worker findings cannot traverse a symlink')
    if not stat.S_ISREG(path.stat().st_mode):
        raise ValueError('worker findings require a regular file')
    with path.open('rb') as stream:
        data = stream.read(limit + 1)
    if len(data) > limit and not truncate:
        raise ValueError('worker findings exceed the file size limit')
    return data[:limit], len(data) > limit


def _selected_candidate(directory, executions=None):
    """The single candidate selection policy used by findings and retention."""
    if executions is None:
        executions = execution_directories(directory)
    turns = sorted((path.parent for path in executions if path.parent.parent.name == 'turns'),
                   key=lambda path: int(path.name), reverse=True)
    candidates = [path / 'candidate' for path in turns] + [directory / 'candidate']
    return next((path for path in candidates if (path / 'candidate.json').exists()), None)


def _findings(directory, task):
    """Capture once after resolution; these bytes never authorize acceptance."""
    nodes = task.get('assignment', {}).get('scope', {}).get('nodes', [])
    if not nodes:
        return None
    executions = execution_directories(directory)
    recorded = False
    for execution in executions:
        relative = execution.relative_to(directory)
        if not (execution / 'process.json').exists():
            continue
        process = json.loads(_read(directory, relative / 'process.json', 65536)[0])
        cleanup = json.loads(_read(directory, relative / 'cleanup.json', 65536)[0])
        if process != cleanup or alive(process):
            raise ValueError('worker findings require acknowledged process cleanup')
        recorded |= execution.parent == directory or worker_execution_coordinates(execution) is not None
    if not recorded:
        return None
    sealed = _selected_candidate(directory, executions)
    record = None
    if sealed is not None:
        relative = sealed.relative_to(directory)
        raw = _read(directory, relative / 'candidate.json', MAX_CANDIDATE_MANIFEST_BYTES)[0]
        identity = _read(directory, relative / 'sha256', 128)[0].strip().decode()
        record = json.loads(raw)
        if digest(raw) != identity or record.get('task_id') != task['id']:
            raise ValueError('worker findings candidate identity mismatch')
    has_evidence = record is not None and any(path.startswith('reference/model-evidence/')
        for path in record['changes'])
    # Preserve prior advisory captures; a new reviewer may discover previously
    # omitted sealed evidence without rewriting the original handoff.
    cached = directory / ('unaccepted-evidence-findings.json' if has_evidence else 'unaccepted-findings.json')
    if cached.exists() or cached.is_symlink():
        return _read(directory, cached.name, MAX_FINDING_BYTES)[0]

    base_bytes = _read(directory, 'base-manifest.json', 4 * 1024 * 1024)[0]
    base = json.loads(base_bytes)
    value = dict(schema=1, task_id=task['id'], advisory=True, accepted=False,
        notice='Unaccepted prior work. Recheck any reused proof or evidence within the current assigned scope.',
        base_manifest_sha256=digest(base_bytes), sources=[], diagnostics=[], unavailable=[])
    if len(nodes) > MAX_FINDING_FILES:
        value['source_scan_truncated'] = True
    assignment = task.get('assignment', {})
    for key in ('profile', 'task_kind', 'base_snapshot'):
        if isinstance(assignment.get(key), str):
            value[key] = assignment[key][:256]
    value['terminal_status'] = {key: item[:2000] for key, item in task['status'].items() if isinstance(item, str)}
    try:
        prepared = json.loads(_read(directory, 'prepared.json', 65536)[0])
        value.update({key: prepared[key] for key in ('request_id', 'source_head')
                      if type(prepared.get(key)) is int or isinstance(prepared.get(key), str)})
    except (OSError, ValueError):
        pass

    if sealed is not None:
        value.update(source_origin='sealed_candidate', candidate_sha256=identity)
        if sealed.parent.parent.name == 'turns':
            value['submission_turn'] = int(sealed.parent.name)
    else:
        value['source_origin'] = 'retained_source'
    recovered_sources = None
    def source_bytes(relative):
        nonlocal recovered_sources
        try:
            return _read(directory, relative, MAX_SOURCE_BYTES, truncate=True)
        except FileNotFoundError:
            if sealed is not None:
                raise
            from .source_recovery import Sources
            if recovered_sources is None:
                recovered_sources = Sources(directory)
            return recovered_sources.read(str(relative.relative_to('repo')), MAX_SOURCE_BYTES, truncate=True)
    for node in sorted(set(nodes))[:MAX_FINDING_FILES]:
        for suffix in ('.lean', '.tex'):
            if len(value['sources']) >= MAX_FINDING_FILES:
                break
            path = f'Tablet/{node}{suffix}'
            safe_relative(path)
            change = record['changes'].get(path) if record else None
            if record is not None and not change:
                continue
            relative = (sealed.relative_to(directory) / 'files' / path) if sealed else Path('repo') / path
            try:
                content, truncated = source_bytes(relative)
            except FileNotFoundError:
                if base.get(path) is not None and (record is None or change['after'] is None):
                    value['sources'].append(dict(path=path, before=base[path], deleted=True))
                continue
            after = digest(content) if not truncated else None
            if record is not None and not truncated and after != change['after']:
                raise ValueError('worker findings candidate source differs from its seal')
            if not truncated and after == base.get(path):
                continue
            value['sources'].append(dict(path=path, before=base.get(path), after=after,
                content=content.decode(errors='replace'), truncated=truncated))
    # Only sealed candidates can contribute evidence outside assigned Tablet
    # nodes. These advisory bytes do not grant that path to a future worker.
    if record is not None:
        evidence = []
        for path, change in sorted(record['changes'].items()):
            if not path.startswith('reference/model-evidence/'):
                continue
            try:
                evidence_paths([path])
            except ValueError:
                continue
            if len(evidence) >= MAX_EVIDENCE_FILES:
                value['evidence_scan_truncated'] = True
                break
            row = dict(path=path, before=change['before'], after=change['after'])
            evidence.append(row)
            if change['after'] is None:
                row['deleted'] = True
                continue
            relative = sealed.relative_to(directory) / 'files' / path
            try:
                content, _ = _read(directory, relative, MAX_SOURCE_BYTES)
                if digest(content) != change['after']:
                    raise ValueError('worker findings candidate evidence differs from its seal')
                row['content'] = content.decode('utf-8')
            except UnicodeDecodeError:
                row['content_unavailable'] = 'sealed evidence is not UTF-8 text'
            except FileNotFoundError:
                row['content_unavailable'] = 'sealed evidence file is unavailable'
            except ValueError as exc:
                # Size-limited files remain discoverable by their candidate
                # path and seal hash; do not display unauthenticated excerpts.
                if str(exc) != 'worker findings exceed the file size limit':
                    raise
                row['content_unavailable'] = str(exc)
        if evidence:
            value['evidence'] = evidence
    if not value['sources'] and not value.get('evidence'):
        return None
    worker_executions = [path for path in executions if worker_execution_coordinates(path) is not None
                         and (path / 'process.json').is_file()]
    execution = max(worker_executions, key=worker_execution_coordinates) if worker_executions else directory / 'execution'
    coordinates = worker_execution_coordinates(execution)
    latest = directory / 'turns' / str(coordinates[0]) if coordinates else directory
    artifact = (f'trellis_worker_{value.get("request_id")}_submission-{coordinates[0]}' if coordinates else
                f'trellis_worker_{value.get("request_id")}_result')
    if coordinates and coordinates[1]:
        artifact += f'-transport-{coordinates[1]}'
    diagnostics = [
        ('worker_response', execution.relative_to(directory) / 'response.json'),
        ('checker_feedback', latest.relative_to(directory) / 'feedback.json'),
        ('raw_submission', Path('repo/.trellis/runtime/runtime/staging') / f'{artifact}.raw.json')]
    checks = directory / 'repo/.trellis/check-results'
    if checks.is_dir() and not checks.is_symlink():
        diagnostics.extend(('check_diagnostic', path.relative_to(directory))
                           for path in sorted(checks.glob('*.txt'))[-3:])
    for label, relative in diagnostics:
        try:
            data, truncated = _read(directory, relative, MAX_REPORT_CHARS, truncate=True)
            # Project response fields; provider receipts and full snapshots are
            # private bookkeeping, not reviewer context.
            if label != 'check_diagnostic' and not truncated:
                try:
                    parsed = json.loads(data)
                except ValueError:
                    if label != 'raw_submission':
                        raise
                    parsed = None  # Malformed worker-authored JSON is useful evidence too.
                if parsed is None:
                    value['diagnostics'].append(dict(kind=label, text=data.decode(errors='replace'), truncated=False))
                    continue
                parsed = parsed.get('response', parsed)
                parsed = parsed.get('raw', parsed)
                if isinstance(parsed, dict):
                    data = canonical_json({key: parsed[key] for key in
                        ('status', 'outcome', 'summary', 'comments', 'deterministic_rejection_reasons')
                        if key in parsed})
            if label != 'check_diagnostic' and truncated:
                value['unavailable'].append(f'{label} exceeds the diagnostic scan limit')
                continue
            value['diagnostics'].append(dict(kind=label, text=data.decode(errors='replace'), truncated=truncated))
        except FileNotFoundError:
            pass
        except (ValueError, AttributeError) as exc:
            value['unavailable'].append(str(exc)[:500])
    try:
        config = json.loads(_read(directory, execution.relative_to(directory) / 'clock/config.json', 65536)[0])
        value.update({key: config['binding'][key][:256] for key in ('provider', 'model', 'reasoning_effort')
                      if isinstance(config['binding'].get(key), str)})
    except (OSError, ValueError, KeyError):
        try:
            call = json.loads(_read(directory, Path('repo/.trellis/chats/live') / artifact / 'call.json', 65536)[0])
            value.update({key: call[key][:256] for key in ('provider', 'model') if isinstance(call.get(key), str)})
        except (OSError, ValueError, AttributeError):
            pass
    data = canonical_json(value)
    while len(data) > MAX_FINDING_BYTES and (value['diagnostics'] or value['sources']
            or any('content' in row for row in value.get('evidence', []))):
        if value['diagnostics']:
            value['diagnostics'].pop()
        elif any('content' in row for row in value.get('evidence', [])):
            row = next(row for row in reversed(value['evidence']) if 'content' in row)
            del row['content']
            row['content_unavailable'] = 'evidence content exceeds the total findings limit'
        else:
            value['sources'].pop()
        value['truncated'] = True
        data = canonical_json(value)
    if (not value['sources'] and not value.get('evidence')) or len(data) > MAX_FINDING_BYTES:
        return None
    atomic(cached, data)
    return data


def relevant_tasks(tasks, nodes, *, limit=32, per_node=2, recent=0):
    """Prefer useful same-node findings over a tail of unrelated dispatches."""
    wanted = set(nodes)
    if not wanted and not recent:
        return []
    counts = {}
    selected = []
    for task in sorted(tasks, key=lambda row: row['id'], reverse=True):
        if task['status']['status'] != 'resolved':
            continue
        matching = wanted.intersection(task.get('assignment', {}).get('scope', {}).get('nodes', []))
        relevant = any(counts.get(node, 0) < per_node for node in matching)
        if relevant or len(selected) < recent:
            selected.append(task)
            for node in matching:
                counts[node] = counts.get(node, 0) + 1
            if len(selected) == limit:
                break
    return selected


def worker_handoffs(dispatch_root, reviewer_repo, tasks):
    """Copy scratch references and bounded unaccepted findings, never acceptance.

    The caller holds the dispatch lock before first reviewer or audit launch. Its task
    list is already the bounded recent-history projection; only reaped tasks
    are read. The frozen context prevents recopying on adoption.
    """
    result, remaining, files_left = [], MAX_BYTES, MAX_FILES
    for task in tasks:
        if task['status']['status'] != 'resolved':
            continue
        task_id = task['id']
        if type(task_id) is not int or task_id < 1:
            raise ValueError('worker handoff requires a positive task ID')
        report = task.get('report') or {}
        text = '\n'.join(report.get(key, '')[:MAX_REPORT_CHARS]
                         for key in ('summary', 'comments') if isinstance(report.get(key), str))
        entry = dict(task_id=task_id, files=[], unavailable=[])
        if any(isinstance(report.get(key), str) and len(report[key]) > MAX_REPORT_CHARS
               for key in ('summary', 'comments')):
            entry['unavailable'].append('Report text exceeds the handoff scan limit; later file references were not copied.')
        try:
            paths = reviewer_scratch_paths(text)
        except ValueError as exc:
            entry['unavailable'].append(str(exc))
            paths = []
        for relative in paths:
            if not files_left or not remaining:
                entry['unavailable'].append('Further files exceed this review\'s handoff limit.')
                break
            files_left -= 1
            try:
                captured, missing = capture_reviewer_scratch(Path(dispatch_root) / str(task_id) / 'repo', relative)
                if missing:
                    entry['unavailable'].extend(missing)
                    continue
                data = captured[relative]
                if len(data) > remaining:
                    entry['unavailable'].append(f'{relative} (exceeds this review\'s remaining handoff limit)')
                    continue
                suffix = PurePosixPath(relative).parts[2:]
                local = PurePosixPath('.trellis/scratch/worker-handoffs') / f'task-{task_id}'
                local = local.joinpath(*suffix).as_posix()
                destination = Path(reviewer_repo) / local
                # A partially prepared context may be resumed, but existing
                # notes are never overwritten with different producer bytes.
                previous, _ = capture_reviewer_scratch(Path(reviewer_repo), local)
                if previous and previous[local] != data:
                    raise ValueError('saved handoff path already contains different bytes')
                if not previous:
                    atomic(destination, data)
                remaining -= len(data)
                entry['files'].append(dict(source=relative, path=local, sha256=digest(data)))
            except (OSError, ValueError) as exc:
                entry['unavailable'].append(f'{relative}: {str(exc)[:500]}')
        if files_left and remaining:
            try:
                data = _findings(Path(dispatch_root) / str(task_id), task)
                if data is not None:
                    if len(data) > remaining:
                        raise ValueError('Unaccepted findings exceed this review\'s remaining handoff limit.')
                    local = f'.trellis/scratch/worker-handoffs/task-{task_id}/unaccepted-findings.json'
                    previous, _ = capture_reviewer_scratch(Path(reviewer_repo), local)
                    if previous and previous[local] != data:
                        raise ValueError('saved findings already contain different bytes')
                    if not previous:
                        atomic(Path(reviewer_repo) / local, data)
                    remaining -= len(data)
                    files_left -= 1
                    entry['files'].append(dict(source='unaccepted-findings.json', path=local,
                                              sha256=digest(data), advisory=True))
            except (OSError, ValueError) as exc:
                entry['unavailable'].append(str(exc)[:500])
        if entry['files'] or entry['unavailable']:
            result.append(entry)
    return result

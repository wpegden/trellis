"""Bounded read-only views of an existing, digest-bound human package.

Opening a review uses the ordinary full validation. Subsequent file reads check
package identity and the selected snapshot bytes, not the entire evidence tree.
Human decisions still run the original complete validation in Driver.decide.
"""
from __future__ import annotations

import difflib
import hashlib
import os
from pathlib import Path
import re
import stat

from scripts.pv_source_tree import _relative_path
from trellis.pv_revision.store import RevisionStoreError, _read, _state_source, digest

MAX_FILE_BYTES = 1024 * 1024
MAX_LINES = 20000
PAGE_LINES = 300


class InlineViewLimit(RevisionStoreError):
    """A valid file needs the download surface instead of an inline view."""


def verified_text(source, relative, entry):
    """Read one confined regular file, rejecting symlinks and changed bytes."""
    _relative_path(tuple(relative.split('/')))
    file = _state_source(Path(source), relative)
    descriptor = os.open(file, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, 'rb') as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode):
            raise RevisionStoreError('Snapshot file is not regular')
        if metadata.st_size > MAX_FILE_BYTES:
            raise InlineViewLimit('File exceeds the 1 MiB inline limit; use the review archive')
        raw = stream.read(MAX_FILE_BYTES + 1)
    if len(raw) > MAX_FILE_BYTES or hashlib.sha256(raw).hexdigest() != entry['sha256_of_raw_bytes']:
        raise RevisionStoreError('Snapshot file bytes no longer match this review')
    try:
        text = raw.decode('utf-8')
    except UnicodeDecodeError:
        return None
    if '\0' in text:
        return None
    if len(text.splitlines()) > MAX_LINES:
        raise InlineViewLimit('File exceeds the 20000-line inline limit; use the review archive')
    return text


def current_package(root, expected):
    root = Path(root).resolve()
    if not isinstance(expected, str) or not re.fullmatch('[a-f0-9]{64}', expected):
        raise RevisionStoreError('An exact review package digest is required')
    state = _read(root / 'state.json')
    if state.get('status') not in {'awaiting_human', 'accepted_for_modeling', 'successor_preparing', 'successor_started', 'all_requirements_retired'}:
        raise RevisionStoreError('Revision has no current reviewable package')
    package = _read(_state_source(root, f'reviews/{expected}/review.json'))
    if package.get('digest') != expected or digest({k: v for k, v in package.items() if k != 'digest'}) != expected or package != state.get('human_package'):
        raise RevisionStoreError('Review package changed; reopen the review')
    for key in ('round', 'original_collection', 'proposed_collection', 'original_goals', 'proposed_goals',
                'negated_goals', 'original_manifest', 'source_manifest', 'source', 'plan', 'tasks', 'attempts', 'aggregate_scope'):
        if package.get(key) != state.get(key):
            raise RevisionStoreError('Review candidate changed; reopen the review')
    for key, default in (('aggregate_checks', {}), ('aggregate_check_history', []), ('operator_actions', [])):
        if package.get(key, default) != state.get(key, default):
            raise RevisionStoreError('Review evidence changed; reopen the review')
    if package.get('original_evidence') != state.get('handoff', {}):
        raise RevisionStoreError('Review evidence changed; reopen the review')
    history = [item for item in state.get('history', []) if item.get('task', {}).get('role') != 'aggregate_scope']
    if package.get('controller_evidence', []) != history:
        raise RevisionStoreError('Review history changed; reopen the review')
    if package.get('human_code_edits', []) != [item['code_edits'] for item in state.get('human_decisions', []) if 'code_edits' in item]:
        raise RevisionStoreError('Human code amendment history changed; reopen the review')
    if package.get('comment_amendment') != state.get('comment_amendment'):
        raise RevisionStoreError('Comment review changed; reopen the review')
    if package.get('review_repackaging') != state.get('review_repackaging'):
        raise RevisionStoreError('Review packaging changed; reopen the review')
    return package


def review_index(package):
    """The navigable inventory omits patches and large check artifacts."""
    keys = ('schema', 'digest', 'candidate_digest', 'evidence_digest', 'comment_amendment', 'review_repackaging', 'original_collection', 'proposed_collection', 'plan', 'aggregate_scope', 'scope_passed')
    result = {key: package.get(key) for key in keys}
    result['files'] = [dict(row, group='production') for row in package['aggregate_diff']['changes']]
    result['files'].extend(dict(row, group='evidence') for row in package.get('evidence_diff', {}).get('changes', []))
    result['source_files'] = source_files(package)
    result['attempts'] = [{key: a.get(key) for key in ('id', 'task_id', 'round', 'accepted', 'authorization', 'response', 'scope')}
                          for a in package['attempts']]
    result['task_diffs'] = [{**{key: d.get(key) for key in ('attempt_id', 'task_id', 'accepted')},
                             'paths': [c['path'] for c in d['diff']['changes']]}
                            for d in package['task_diffs']]
    return result


def source_files(package):
    before = {row['relative_path']: row for row in package['original_manifest']['files']}
    after = {row['relative_path']: row for row in package['source_manifest']['files']}
    evidence = {row['relative_path'] for row in package.get('evidence', {}).get('source_manifest', {}).get('files', [])}
    return [dict(path=path, before=before.get(path), after=after.get(path), group='evidence' if path in evidence else 'production',
                 operation='add' if path not in before else 'delete' if path not in after
                 else 'unchanged' if before[path] == after[path] else 'modify')
            for path in sorted(before.keys() | after.keys())]


def file_view(root, expected, relative, *, mode='diff', context=3, offset=0):
    _relative_path(tuple(relative.split('/')))
    if mode not in {'diff', 'before', 'after', 'edit'} or type(context) is not int or not 0 <= context <= 100 or type(offset) is not int or not 0 <= offset <= 100000:
        raise RevisionStoreError('Invalid file view selection')
    root = Path(root).resolve()
    package = current_package(root, expected)
    change = next((row for row in source_files(package) if row['path'] == relative), None)
    if change is None:
        raise RevisionStoreError('File is not in this review package')
    texts = {}
    for side, source in (('before', 'sources/original'), ('after', package['source'])):
        texts[side] = verified_text(_state_source(root, source), relative, change[side]) if change[side] else ''
    result = {'digest': expected, 'path': relative, 'mode': mode, 'context': context, 'offset': offset,
              'before_hash': (change['before'] or {}).get('sha256_of_raw_bytes'),
              'after_hash': (change['after'] or {}).get('sha256_of_raw_bytes'),
              'before_exists': change['before'] is not None, 'after_exists': change['after'] is not None}
    if any(t is None for t in texts.values()):
        return dict(result, binary=True, rows=[], total=0)
    if mode == 'edit':
        if _read(root / 'state.json').get('human_package', {}).get('digest') != expected:
            raise RevisionStoreError('Review package changed during read')
        return dict(result, binary=False, text=texts['after'], original_text=texts['before'])
    lines = {side: text.splitlines(keepends=True) for side, text in texts.items()}
    rows = []
    if mode != 'diff':
        rows = [{'kind': 'context', mode: i + 1, 'text': line} for i, line in enumerate(lines[mode])]
    else:
        # Bounded to two 1 MiB/20000-line inputs; no tree reads or copies.
        matcher = difflib.SequenceMatcher(None, lines['before'], lines['after'])
        for group in matcher.get_grouped_opcodes(context):
            first, last = group[0], group[-1]
            def interval(start, stop):
                count = stop - start
                begin = start + 1 if count else start
                return str(begin) if count == 1 else f'{begin},{count}'
            rows.append({'kind': 'hunk', 'text': f'@@ -{interval(first[1], last[2])} +{interval(first[3], last[4])} @@'})
            for tag, i1, i2, j1, j2 in group:
                if tag == 'equal':
                    rows.extend({'kind': 'context', 'before': i + 1, 'after': j1 + i - i1 + 1, 'text': lines['before'][i]} for i in range(i1, i2))
                else:
                    if tag in {'replace', 'delete'}:
                        rows.extend({'kind': 'delete', 'before': i + 1, 'text': lines['before'][i]} for i in range(i1, i2))
                    if tag in {'replace', 'insert'}:
                        rows.extend({'kind': 'add', 'after': j + 1, 'text': lines['after'][j]} for j in range(j1, j2))
    # Recheck identity after the reads, so a concurrent new round fails closed.
    if _read(root / 'state.json').get('human_package', {}).get('digest') != expected:
        raise RevisionStoreError('Review package changed during read')
    return dict(result, binary=False, rows=rows[offset:offset + PAGE_LINES], total=len(rows))

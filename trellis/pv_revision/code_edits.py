"""Exact human amendment contents, stored once outside active request state."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import stat

from trellis.pv_revision.store import RevisionStoreError, _state_source, _write, digest
from trellis.pv_revision.review import MAX_FILE_BYTES, MAX_LINES, source_files, verified_text
from scripts.pv_source_tree import canonical_json

MAX_EDIT_BYTES = 4 * MAX_FILE_BYTES
MAX_REQUEST_BYTES = 8 * MAX_FILE_BYTES
MAX_EDIT_FILES = 64


def _summary(document):
    return dict(package_digest=document['package_digest'], files=[dict(
        path=e['path'], base_sha256=e['base_sha256'],
        requested_sha256=hashlib.sha256(e['text'].encode('utf-8')).hexdigest() if e['text'] is not None else None,
        operation='delete' if e['text'] is None else 'add' if e['base_sha256'] is None else 'modify',
    ) for e in document['edits']])


def save(root, package, edits):
    """Validate the whole request before storing feedback; never edit sources."""
    if not isinstance(edits, list) or not 1 <= len(edits) <= MAX_EDIT_FILES:
        raise RevisionStoreError('Code amendments require 1–64 edited files')
    root = Path(root).resolve()
    inventory = {row['path']: row for row in source_files(package)}
    seen, checked, total = set(), [], 0
    for edit in edits:
        if not isinstance(edit, dict) or set(edit) != {'path', 'base_sha256', 'text'}:
            raise RevisionStoreError('Each code edit requires path, base_sha256 and text')
        path = edit['path']
        if not isinstance(path, str) or path not in inventory or path in seen:
            raise RevisionStoreError('Code edit path must be unique and belong to this review package')
        seen.add(path)
        row = inventory[path]
        expected = (row['after'] or {}).get('sha256_of_raw_bytes')
        if edit['base_sha256'] != expected:
            raise RevisionStoreError(f'Code edit base changed for {path}; reopen the review')
        # A deleted file can be restored; its original bytes still must be text.
        entry = row['after'] or row['before']
        source = package['source'] if row['after'] else 'sources/original'
        base = verified_text(_state_source(root, source), path, entry)
        if base is None:
            raise RevisionStoreError('Inline code amendments require UTF-8 text files')
        text = edit['text']
        if text is not None:
            if not isinstance(text, str) or '\0' in text:
                raise RevisionStoreError('Edited code must be UTF-8 text without NUL, or null for deletion')
            size = len(text.encode('utf-8'))
            total += size
            if size > MAX_FILE_BYTES or len(text.splitlines()) > MAX_LINES or total > MAX_EDIT_BYTES:
                raise RevisionStoreError('Code edits exceed 1 MiB/20000 lines per file or 4 MiB total')
        if text == (base if row['after'] else None):
            raise RevisionStoreError(f'Code edit does not change {path}')
        checked.append(dict(edit))
    document = dict(schema='trellis-human-code-edits/v1', package_digest=package['digest'],
                    source=package['source'], edits=sorted(checked, key=lambda e: e['path']))
    if len(canonical_json(document)) > MAX_REQUEST_BYTES:
        raise RevisionStoreError('Encoded code amendment exceeds 8 MiB')
    identity = digest(document)
    relative = f'human-edits/{identity}.json'
    _write(_state_source(root, relative), document, immutable=True)
    return dict(path=relative, sha256=identity, **_summary(document))


def load(root, reference):
    """Verify one immutable feedback object before agent use or export."""
    if not isinstance(reference, dict) or not re.fullmatch('[a-f0-9]{64}', str(reference.get('sha256', ''))):
        raise RevisionStoreError('Invalid human code amendment reference')
    identity = reference['sha256']
    if reference.get('path') != f'human-edits/{identity}.json':
        raise RevisionStoreError('Invalid human code amendment path')
    path = _state_source(Path(root).resolve(), reference['path'])
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, 'rb') as stream:
        meta = os.fstat(stream.fileno())
        if not stat.S_ISREG(meta.st_mode) or meta.st_size > MAX_REQUEST_BYTES:
            raise RevisionStoreError('Invalid human code amendment evidence file')
        raw = stream.read(MAX_REQUEST_BYTES + 1)
    if len(raw) > MAX_REQUEST_BYTES or hashlib.sha256(raw).hexdigest() != identity:
        raise RevisionStoreError('Human code amendment evidence changed')
    document = json.loads(raw)
    if document.get('schema') != 'trellis-human-code-edits/v1' or reference != dict(
            path=reference['path'], sha256=identity, **_summary(document)):
        raise RevisionStoreError('Human code amendment metadata changed')
    return document


def references(root, state):
    result = []
    for decision in state.get('human_decisions', []):
        if 'code_edits' not in decision:
            continue
        reference = decision['code_edits']
        document = load(root, reference)
        if decision['action'] != 'amend' or document['package_digest'] != decision['digest']:
            raise RevisionStoreError('Human code amendment belongs to a different decision')
        result.append(reference)
    return result

"""Optional review annotations, validated before the controller records them."""
from __future__ import annotations

import json
from pathlib import Path

from scripts.pv_source_tree import _relative_path

CHARACTERIZATIONS = {'requirement-text change', 'source-dependent interpretation change', 'implementation-only repair'}


def validate_notes(role, result):
    for field in ('goal_notes', 'code_notes'):
        if field not in result:
            continue
        allowed = role == 'audit' if field == 'goal_notes' else role in {'scope', 'aggregate_scope'}
        if not allowed:
            raise ValueError(f'{role} cannot author {field}')
        notes = result[field]
        if not isinstance(notes, list) or len(notes) > 100 or len(json.dumps(notes)) > 128000:
            raise ValueError('Review notes must be a list of at most 100 entries and 128000 serialized characters')
        for note in notes:
            keys = {'goal_ids', 'reason', 'characterization'} if field == 'goal_notes' else {'path', 'range', 'goal_ids', 'reason'}
            if not isinstance(note, dict) or set(note) - keys:
                raise ValueError('Unknown review note fields; author role is controller-owned')
            if not isinstance(note.get('reason'), str) or not note['reason'].strip() or len(note['reason']) > 4000:
                raise ValueError('Review note reason must contain 1–4000 characters')
            ids = note.get('goal_ids')
            if not isinstance(ids, list) or not ids or len(ids) > 100 or any(not isinstance(g, str) or not g or len(g) > 256 for g in ids) or len(ids) != len(set(ids)):
                raise ValueError('Review notes require 1–100 unique nonempty goal IDs, each at most 256 characters')
            if field == 'goal_notes':
                if not isinstance(note.get('characterization'), str) or note['characterization'] not in CHARACTERIZATIONS:
                    raise ValueError('Unknown goal note characterization')
            else:
                value = note.get('path')
                if not isinstance(value, str) or len(value) > 1024:
                    raise ValueError('Code note requires a confined relative file path')
                _relative_path(tuple(value.split('/')))
                if 'range' in note:
                    span = note['range']
                    if not isinstance(span, dict) or set(span) != {'side', 'start', 'end'} or not isinstance(span['side'], str) or span['side'] not in {'before', 'after'}:
                        raise ValueError('Code note range requires side, start and end')
                    if any(type(span[k]) is not int for k in ('start', 'end')) or not 1 <= span['start'] <= span['end'] <= 1000000:
                        raise ValueError('Invalid code note line range')


def bind_notes(role, result, state, *, root: Path, before=None, after=None, before_source=None, after_source=None):
    """Validate real relationships and stamp roles from the scheduled task only.

    Absence stays absent: old plans/verdicts retain their exact serialized shape.
    The caller already copied the agent response before invoking this function.
    """
    known = {g['id'] for g in state['original_goals']} | {g['id'] for g in state['proposed_goals']}
    field = 'goal_notes' if role == 'audit' else 'code_notes'
    for note in result.get(field, []):
        if not set(note['goal_ids']) <= known:
            raise ValueError('Review note references unknown goal IDs')
        if field == 'code_notes':
            from trellis.pv_revision.store import changed_paths, _state_source
            if note['path'] not in changed_paths(before, after):
                raise ValueError('Code note must identify an actually changed file')
            if 'range' in note:
                span = note['range']
                manifest, source = (before, before_source) if span['side'] == 'before' else (after, after_source)
                entry = next((f for f in manifest['files'] if f['relative_path'] == note['path']), None)
                if entry is None:
                    raise ValueError('Code note range names an absent file side')
                from trellis.pv_revision.review import InlineViewLimit, verified_text
                try:
                    text = verified_text(_state_source(root, source), note['path'], entry)
                except InlineViewLimit as error:
                    raise ValueError(f'{error}; omit range for a file-level note') from error
                if text is None:
                    raise ValueError('Code note range requires a UTF-8 text file; omit range for a file-level note')
                if span['end'] > len(text.splitlines()):
                    raise ValueError('Code note range exceeds text file lines')
        note['author_role'] = role

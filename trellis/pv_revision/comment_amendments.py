"""Exact human comment edits with a focused review and explicitly inherited checks."""
from copy import deepcopy
import hashlib
import re

from trellis.pv_revision import code_edits, store

# Replacing existing full-line Rust comments preserves line numbers and
# existing note anchors. This deliberately does not classify arbitrary edits.
def comment_skeleton(text):
    i, start, pieces = 0, 0, []
    while i < len(text):
        if text.startswith('//', i):
            end = text.find('\n', i)
            end = len(text) if end < 0 else end
            if end > i and text[end - 1] == '\r':
                end -= 1
            kind = '///' if text.startswith('///', i) and not text.startswith('////', i) else '//!' if text.startswith('//!', i) else '//'
            if not text[text.rfind('\n', 0, i) + 1:i].strip():
                pieces.append(text[start:i + len(kind)])
                pieces.append('\0')
                start = end
            i = end
        elif text.startswith('/*', i):
            depth, i = 1, i + 2
            while i < len(text) and depth:
                if text.startswith('/*', i):
                    depth, i = depth + 1, i + 2
                elif text.startswith('*/', i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
            if depth:
                raise ValueError('Unterminated Rust block comment')
        elif text[i] == 'r' and (raw := re.match(r'r(#+)?"', text[i:])):
            close = '"' + (raw[1] or '')
            end = text.find(close, i + len(raw[0]))
            if end < 0:
                raise ValueError('Unterminated Rust raw string')
            i = end + len(close)
        elif text[i] == '"':
            i += 1
            while i < len(text) and text[i] != '"':
                i += 2 if text[i] == '\\' else 1
            if i >= len(text):
                raise ValueError('Unterminated Rust string')
            i += 1
        elif text[i] == "'" and (char := re.match(r"'(?:[^'\\\r\n]|\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|[^\r\n]))'", text[i:])):
            i += len(char[0])
        else:
            i += 1
    return ''.join([*pieces, text[start:]])


def base_package(root, identity):
    if not isinstance(identity, str) or not re.fullmatch('[a-f0-9]{64}', identity):
        raise ValueError('An exact prior review digest is required')
    package = store._read(root / 'reviews' / identity / 'review.json')
    if package.get('digest') != identity or store.digest({k: v for k, v in package.items() if k != 'digest'}) != identity:
        raise ValueError('Prior comment review evidence changed')
    if store.source_manifest(store._state_source(root, package['source'])) != package['source_manifest']:
        raise ValueError('Prior comment review source changed')
    return package


def implementation_base(root, package):
    """Find the fully reviewed implementation, including across failed comment reviews."""
    seen = set()
    while package.get('comment_amendment'):
        if package['digest'] in seen:
            raise ValueError('Cyclic comment review history')
        seen.add(package['digest'])
        prior = base_package(root, package['comment_amendment']['base_review'])
        for key in (*PRESERVED, 'original_evidence'):
            if package.get(key) != prior.get(key):
                raise ValueError(f'Comment review history changed preserved {key}')
        package = prior
    if not package['scope_passed']:
        raise ValueError('Comment-only changes require a scope-passed implementation review')
    return package


def validate_edits(root, package, edits):
    checked = implementation_base(root, package)
    _validate_edits(root, package, edits)
    if checked['digest'] == package['digest']:
        return
    # Check the complete proposed result against the checked implementation,
    # rather than treating a failed comment review as a new trusted baseline.
    current = {row['relative_path']: row for row in package['source_manifest']['files']}
    original = {row['relative_path']: row for row in checked['source_manifest']['files']}
    if current.keys() != original.keys():
        raise ValueError('Comment review changed the source inventory')
    replacements = {edit['path']: edit['text'] for edit in edits}
    cumulative = []
    from trellis.pv_revision.review import verified_text
    for path, row in current.items():
        if path not in replacements and row == original[path]:
            continue
        text = replacements.get(path)
        if text is None:
            text = verified_text(store._state_source(root, package['source']), path, row)
        if text is None:
            raise ValueError('Comment review contains a non-text change')
        if hashlib.sha256(text.encode('utf-8')).hexdigest() != original[path]['sha256_of_raw_bytes']:
            cumulative.append(dict(path=path, base_sha256=original[path]['sha256_of_raw_bytes'], text=text))
    if cumulative:
        _validate_edits(root, checked, cumulative)


def _validate_edits(root, package, edits):
    if not isinstance(edits, list) or not edits:
        raise ValueError('Comment-only review requires exact code edits')
    if package['schema'] != store.REVIEW_SCHEMA:
        raise ValueError('Separate production from evidence before requesting comment-only changes')
    evidence = {row['relative_path'] for row in package['evidence']['source_manifest']['files']}
    inventory = {row['relative_path']: row for row in package['source_manifest']['files']}
    for edit in edits:
        if (not isinstance(edit, dict) or set(edit) != {'path', 'base_sha256', 'text'}
                or not isinstance(edit['path'], str) or not isinstance(edit['text'], str)):
            raise ValueError('Comment edits require a path, base hash and replacement text')
        path = edit['path']
        if path in evidence:
            raise ValueError('Comment-only changes are limited to production files; use a normal amendment for evidence changes')
        if not path.endswith('.rs') or path not in inventory or edit['text'] is None:
            raise ValueError('Comment-only changes currently support existing Rust files')
        from trellis.pv_revision.review import verified_text
        before = verified_text(store._state_source(root, package['source']), path, inventory[path])
        if before is None or edit['base_sha256'] != inventory[path]['sha256_of_raw_bytes']:
            raise ValueError('Comment edit base changed')
        if (before == edit['text'] or len(before.splitlines()) != len(edit['text'].splitlines())
                or comment_skeleton(before) != comment_skeleton(edit['text'])):
            raise ValueError('Comment-only changes must replace existing full-line Rust comments, preserving comment kind, line breaks and all other bytes; use a normal amendment for other edits')


PRESERVED = ('original_collection', 'original_goals', 'negated_goals', 'original_manifest',
             'proposed_collection', 'proposed_goals', 'plan', 'tasks', 'attempts',
             'aggregate_scope', 'aggregate_checks', 'aggregate_check_history')


def _edited_files(package, document):
    files = deepcopy(package['source_manifest']['files'])
    changes = {e['path']: e['text'].encode('utf-8') for e in document['edits']}
    for row in files:
        raw = changes.get(row['relative_path'])
        if raw is not None:
            row.update(sha256_of_raw_bytes=hashlib.sha256(raw).hexdigest(), byte_length=len(raw))
    return files


def prepare(root, state, package, reference):
    document = code_edits.load(root, reference)
    if document['package_digest'] != package['digest'] or document['source'] != package['source']:
        raise ValueError('Comment changes belong to a different review')
    validate_edits(root, package, document['edits'])
    candidate = deepcopy(state)
    for key in PRESERVED:
        if key in package:
            candidate[key] = deepcopy(package[key])
    source = f"sources/comments-{reference['sha256']}"
    # A deterministic destination permits recovery after a crash before save.
    directory = root / source
    if not directory.exists():
        store.copy_source_tree(root / package['source'], directory)
        for edit in document['edits']:
            (directory / edit['path']).write_bytes(edit['text'].encode('utf-8'))
    manifest = store.source_manifest(directory)
    if manifest['files'] != _edited_files(package, document):
        raise ValueError('Prepared comment source differs from exact requested edits')
    record = dict(base_review=package['digest'], code_edits=reference, source=source, source_manifest=manifest,
                  checks_rerun=False)
    record['binding'] = store.digest(record)
    candidate.update(source=source, source_manifest=manifest, comment_amendment=record,
                     status='active', phase='comment_scope', next={'role':'comment_scope'},
                     round=state['round'] + 1, feedback='', consecutive_response_errors=0)
    for key in ('human_package', 'review_repackaging', 'pending_attempt', 'halt_reason'):
        candidate.pop(key, None)
    return candidate


def validate(root, state, *, reviewed=False):
    record = state['comment_amendment']
    unsigned = {k:v for k,v in record.items() if k not in {'binding', 'review'}}
    if record['binding'] != store.digest(unsigned) or record['checks_rerun'] is not False:
        raise ValueError('Comment review binding changed')
    base = base_package(root, record['base_review'])
    document = code_edits.load(root, record['code_edits'])
    if document['package_digest'] != base['digest'] or document['source'] != base['source']:
        raise ValueError('Comment edit evidence belongs to another review')
    validate_edits(root, base, document['edits'])
    if state['source'] != record['source'] or state['source_manifest'] != record['source_manifest']:
        raise ValueError('Comment review candidate changed')
    for key in PRESERVED:
        if state.get(key) != base.get(key):
            raise ValueError(f'Comment amendment changed preserved {key}')
    if state.get('handoff', {}) != base['original_evidence']:
        raise ValueError('Comment amendment changed predecessor evidence')
    if state['source_manifest']['files'] != _edited_files(base, document):
        raise ValueError('Comment candidate differs from exact requested edits')
    if reviewed:
        review = record.get('review', {})
        if review.get('binding') != record['binding'] or review.get('decision') not in {'pass','fail'}:
            raise ValueError('Missing or stale focused comment review')
    return base

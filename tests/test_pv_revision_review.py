"""Frozen review reads and optional note identity/validation contracts."""
from copy import deepcopy
import json

import pytest

from trellis.json_io import save_json
from trellis.pv_revision.driver import validate_response
from trellis.pv_revision.review import file_view, review_index, current_package
from trellis.pv_revision.store import build_review_package_from_state, write_review_package, source_manifest, review_readme
from test_pv_revision_driver import make_driver, plan, review, worker, verdict
from test_pv_revision_store import driver_state


def goal_note(**changes):
    return dict(goal_ids=['retained'], characterization='source-dependent interpretation change',
                reason='The definition now refers to the revised source.', **changes)


def code_note():
    return dict(path='lib.rs', goal_ids=['g', 'retained'], reason='Bounded implementation preserves the retained diagnostic.',
                range={'side': 'after', 'start': 1, 'end': 1})


def noted_verdict():
    return lambda root, state, task: dict(verdict()(root, state, task), code_notes=[code_note()])


def gate(tmp_path, notes=True):
    audit = plan()
    if notes:
        audit['goal_notes'] = [goal_note()]
    responses = [audit, review('work'), worker(), noted_verdict() if notes else verdict(),
                 review('complete'), noted_verdict() if notes else verdict()]
    driver = make_driver(tmp_path, responses, goals=[{'id': 'g', 'text': 'goal'}, {'id': 'retained', 'text': 'source definition'}])
    while responses:
        driver.step()
    assert driver.state['status'] == 'awaiting_human', driver.state.get('feedback')
    return driver


def test_notes_roles_retained_goals_and_existing_binding(tmp_path):
    driver = gate(tmp_path)
    pkg = driver.state['human_package']
    assert pkg['plan']['goal_notes'][0]['author_role'] == 'audit'
    assert pkg['attempts'][0]['scope']['code_notes'][0]['author_role'] == 'scope'
    assert pkg['aggregate_scope']['code_notes'][0]['author_role'] == 'aggregate_scope'
    assert pkg['proposed_goals'] == pkg['original_goals']
    assert 'goal_notes' not in driver.state['history'][0]['response'] or 'author_role' not in driver.state['history'][0]['response']['goal_notes'][0]
    assert 'goal_notes' in review_index(pkg)['plan']
    readme = review_readme(pkg)
    assert 'Structured review notes' in readme and 'Final scope code note' in readme and 'after lines 1–1' in readme
    old = pkg['digest']
    driver.state['aggregate_scope']['code_notes'][0]['reason'] = 'A changed explanation'
    updated = driver._package(driver.state)
    assert updated['digest'] != old and updated['evidence_digest'] != pkg['evidence_digest']
    assert updated['candidate_digest'] == pkg['candidate_digest']
    with pytest.raises(ValueError, match='stale'):
        driver.decide('accept', digest=old, reason='old note reviewed')


def test_missing_notes_do_not_augment_old_records(tmp_path):
    driver = gate(tmp_path, notes=False)
    pkg = driver.state['human_package']
    assert not any(key in pkg['plan'] for key in ('goal_notes', 'code_notes'))
    assert 'code_notes' not in pkg['aggregate_scope']
    assert driver._package(driver.state) == pkg
    assert current_package(driver.root, pkg['digest']) == pkg
    driver.decide('accept', digest=pkg['digest'], reason='existing identity')


def test_legacy_no_note_digest_is_exact(tmp_path):
    root, state = driver_state(tmp_path)
    # Relative paths make this historical package fixture location-independent.
    state['attempts'][0]['base_source'] = 'sources/original'
    pkg = build_review_package_from_state(root, state, schema='trellis-pv-revision-review/v1')
    assert pkg['digest'] == '6f89c7fa165431bc95007b8128f92d97a446417494cf61e5faa9d237fbd7f6e3'
    assert pkg['candidate_digest'] == 'a23f806ae9d219a380df45cd8f28c873ec198f2a7f9ec20cb9b909bdab6e8924'


@pytest.mark.parametrize('field,role', [('code_notes', 'audit'), ('goal_notes', 'scope'), ('code_notes', 'worker'), ('goal_notes', 'reviewer')])
def test_role_cannot_author_future_patch_or_audit_notes(field, role):
    with pytest.raises(ValueError, match='cannot author'):
        validate_response(role, {field: []})


@pytest.mark.parametrize('change', [
    {'goal_ids': []}, {'goal_ids': ['g', 'g']}, {'goal_ids': [3]}, {'goal_ids': ['x' * 257]},
    {'reason': ''}, {'reason': 'x' * 4001}, {'author_role': 'aggregate_scope'}, {'characterization': 'semantically unchanged'}, {'characterization': []}, {'characterization': {}},
])
def test_bad_goal_note_shape_rejected(change):
    p = plan(); n = goal_note(); n.update(change); p['goal_notes'] = [n]
    with pytest.raises(ValueError):
        validate_response('audit', p)


@pytest.mark.parametrize('change', [
    {'path': '../lib.rs'}, {'path': '/lib.rs'}, {'path': 'a\\lib.rs'}, {'path': 'a//lib.rs'}, {'path': 'a\n.rs'},
    {'range': {'side': 'future', 'start': 1, 'end': 2}}, {'range': {'side': [], 'start': 1, 'end': 2}}, {'range': {'side': 'after', 'start': 0, 'end': 2}},
    {'range': {'side': 'after', 'start': 2, 'end': 1}}, {'range': {'side': 'after', 'start': True, 'end': 2}},
    {'range': {'side': 'after', 'start': 1, 'end': 1000001}}, {'author_role': 'audit'},
])
def test_bad_code_note_shape_rejected(change):
    n = code_note(); n.update(change)
    with pytest.raises(ValueError):
        validate_response('aggregate_scope', {'code_notes': [n]})


def test_oversized_note_list_rejected():
    p = plan(); p['goal_notes'] = [goal_note() for _ in range(101)]
    with pytest.raises(ValueError, match='100 entries'):
        validate_response('audit', p)


def test_unknown_audit_goal_is_transactionally_rejected(tmp_path):
    p = plan(); p['goal_notes'] = [dict(goal_note(), goal_ids=['unknown'])]
    driver = make_driver(tmp_path, [p]); driver.step()
    assert driver.state['plan'] is None
    assert 'unknown goal' in driver.state['feedback']


@pytest.mark.parametrize('change,message', [
    ({'goal_ids': ['unknown']}, 'unknown goal'), ({'path': 'other.rs'}, 'actually changed'),
    ({'range': {'side': 'after', 'start': 1, 'end': 2}}, 'exceeds text file'),
])
def test_code_note_must_bind_actual_file_goal_and_range(tmp_path, change, message):
    def bad(root, state, task):
        n = code_note(); n['goal_ids'] = ['g']; n.update(change)
        return dict(verdict()(root, state, task), code_notes=[n])
    responses = [plan(), review('work'), worker(), bad]
    driver = make_driver(tmp_path, responses)
    while responses:
        driver.step()
    assert driver.state['attempts'] == [] and message in driver.state['feedback']


def frozen(tmp_path):
    root, state = driver_state(tmp_path)
    old = root / 'sources/original'; new = root / state['source']
    (old / 'code.rs').write_text(''.join(f'line {i}\n' for i in range(700)))
    (new / 'code.rs').write_text(''.join('new <script> & text\n' if i == 350 else f'line {i}\n' for i in range(700)))
    (new / 'added.rs').write_text('added\n')
    (old / 'deleted.rs').write_text('deleted\n')
    state['original_manifest'] = state['attempts'][0]['base'] = source_manifest(old)
    state['source_manifest'] = state['attempts'][0]['manifest'] = source_manifest(new)
    state['human_package'] = build_review_package_from_state(root, state)
    state['status'] = 'awaiting_human'
    save_json(root / 'state.json', state)
    write_review_package(root, state['human_package'])
    return root, state


def test_frozen_diff_context_full_file_and_paging(tmp_path, monkeypatch):
    root, state = frozen(tmp_path); d = state['human_package']['digest']
    # The on-demand path must not rebuild or rehash any whole evidence tree.
    monkeypatch.setattr('trellis.pv_revision.store.source_manifest', lambda *a: pytest.fail('whole-tree revalidation'))
    narrow = file_view(root, d, 'code.rs')
    assert narrow['rows'][0]['kind'] == 'hunk'
    assert [r for r in narrow['rows'] if r['kind'] == 'delete'][0]['before'] == 351
    assert [r for r in narrow['rows'] if r['kind'] == 'add'][0]['text'] == 'new <script> & text\n'
    assert file_view(root, d, 'code.rs', context=20)['total'] > narrow['total']
    full = file_view(root, d, 'code.rs', mode='after', offset=300)
    assert full['total'] == 700 and len(full['rows']) == 300 and full['rows'][0]['after'] == 301
    assert full['after_hash'] == next(f['sha256_of_raw_bytes'] for f in state['source_manifest']['files'] if f['relative_path'] == 'code.rs')
    assert file_view(root, d, 'added.rs')['rows'][0]['text'] == '@@ -0,0 +1 @@'
    assert file_view(root, d, 'deleted.rs')['rows'][0]['text'] == '@@ -1 +0,0 @@'
    assert file_view(root, d, 'added.rs')['before_exists'] is False
    assert file_view(root, d, 'deleted.rs')['after_exists'] is False
    assert file_view(root, d, 'added.rs', mode='before')['rows'] == []


@pytest.mark.parametrize('mutation', ['digest', 'state', 'artifact', 'bytes', 'symlink', 'unknown', 'escape', 'range'])
def test_frozen_reads_fail_closed(tmp_path, mutation):
    root, state = frozen(tmp_path); d = state['human_package']['digest']; path = 'code.rs'; kwargs = {}
    if mutation == 'digest': d = 'a' * 64
    elif mutation == 'state':
        state['plan']['summary'] = 'changed'; save_json(root / 'state.json', state)
    elif mutation == 'artifact':
        f = root / 'reviews' / d / 'review.json'; f.chmod(0o644); f.write_text('{}')
    elif mutation == 'bytes': (root / state['source'] / path).write_text('tampered')
    elif mutation == 'symlink':
        f = root / state['source'] / path; f.unlink(); f.symlink_to(root / 'sources/original' / path)
    elif mutation == 'unknown': path = 'missing.rs'
    elif mutation == 'escape': path = '../state.json'
    else: kwargs['context'] = 101
    with pytest.raises((ValueError, OSError)):
        file_view(root, d, path, **kwargs)


def test_binary_and_long_line_views(tmp_path):
    root, state = driver_state(tmp_path)
    new = root / state['source']; (new / 'code.rs').write_text('<script>' + 'x' * 50000)
    (new / 'keep.bin').write_bytes(b'\x00changed')
    state['source_manifest'] = state['attempts'][0]['manifest'] = source_manifest(new)
    state['human_package'] = build_review_package_from_state(root, state); state['status'] = 'awaiting_human'
    save_json(root / 'state.json', state); write_review_package(root, state['human_package']); d = state['human_package']['digest']
    assert file_view(root, d, 'keep.bin')['binary']
    assert len(file_view(root, d, 'code.rs', mode='after')['rows'][0]['text']) == 50008


@pytest.mark.parametrize('kind', ['binary', 'large', 'fifo'])
def test_note_range_readability_feedback_and_nonregular_reads(tmp_path, kind):
    from trellis.pv_revision.notes import bind_notes
    from trellis.pv_revision.review import verified_text
    import os
    root, state = driver_state(tmp_path)
    target = root / state['source'] / 'code.rs'
    if kind == 'fifo':
        entry = next(f for f in state['source_manifest']['files'] if f['relative_path'] == 'code.rs')
        target.unlink(); os.mkfifo(target)
        with pytest.raises(ValueError, match='not regular'):
            verified_text(root / state['source'], 'code.rs', entry)
        return
    target.write_bytes(b'\0binary' if kind == 'binary' else b'x' * (1024 * 1024 + 1))
    state['source_manifest'] = source_manifest(root / state['source'])
    result = {'code_notes': [dict(path='code.rs', goal_ids=['G1'], reason='Relevant change', range={'side': 'after', 'start': 1, 'end': 1})]}
    with pytest.raises(ValueError, match='omit range for a file-level note'):
        bind_notes('aggregate_scope', result, state, root=root, before=state['original_manifest'], after=state['source_manifest'],
                   before_source='sources/original', after_source=state['source'])

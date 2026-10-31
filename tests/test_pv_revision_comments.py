"""Human comment amendments never rerun implementation checks or rewrite receipts."""
from copy import deepcopy
import json
import zipfile

import pytest

from trellis.pv_revision import comment_amendments as comments, store
from trellis.pv_revision.review import current_package, file_view, review_index
from test_pv_revision_driver import make_driver, plan, review, worker, verdict
from test_pv_revision_code_edits import edit


def gate(tmp_path):
    responses = [plan(), review('work'), worker('/// Count within bounds.\nfn count() -> i32 { 1 }\n'), verdict(), review('complete'), verdict()]
    def checker(source, _):
        return {'ok': True, 'source': store.source_manifest(source)}
    driver = make_driver(tmp_path, responses, checker=checker)
    while responses:
        driver.step()
    def forbidden(*_):
        pytest.fail('A comment amendment must not rerun the implementation checker')
    driver.checker = forbidden
    return driver, responses


@pytest.mark.parametrize('before,after,eligible', [
    ('// before\nfn f() {}\n', '// after\nfn f() {}\n', True),
    ('    /// before\r\nfn f() {}\r\n', '    /// after\r\nfn f() {}\r\n', True),
    ('//! before\n', '//! after\n', True),
    ('// before\n', '/// after\n', False),
    ('//// before\n', '/// after\n', False),
    ('/// before\n', '//! after\n', False),
    ('// before\nfn f() {1}\n', '// after\nfn f() {2}\n', False),
    ('// before\n', '// after\r\n', False),
    ('// before\n', '// after\n// added\n', False),
    ('let s = "\n// before\n";', 'let s = "\n// after\n";', False),
    ('let s = r###"\n// before\n"###;', 'let s = r###"\n// after\n"###;', False),
    ('let s = br#"\n// before\n"#;', 'let s = br#"\n// after\n"#;', False),
    ('/* outer /* nested */\n// before\n*/', '/* outer /* nested */\n// after\n*/', False),
    ('let s = "\\\"";\n// before\n', 'let s = "\\\"";\n// after\n', True),
    ('let c = \'"\';\n// before\n', 'let c = \'"\';\n// after\n', True),
    ("fn f<'a>(x: &'a str) {}\n// before\n", "fn f<'a>(x: &'a str) {}\n// after\n", True),
    ('fn f() {} // before\n', 'fn f() {} // after\n', False),
])
def test_bounded_rust_comment_classification(before, after, eligible):
    assert (comments.comment_skeleton(before) == comments.comment_skeleton(after)) is eligible


def test_repeated_comment_review_preserves_receipts_and_original_diff(tmp_path):
    driver, responses = gate(tmp_path)
    original = deepcopy(driver.state['human_package'])
    for text in ('/// One within the signed range.\nfn count() -> i32 { 1 }\n', '/// The result is one.\nfn count() -> i32 { 1 }\n'):
        prior = deepcopy(driver.state['human_package'])
        driver.decide('amend', digest=prior['digest'], reason='Only wording', comment_only=True, code_edits=[edit(prior, text)])
        assert driver.state['phase'] == 'comment_scope'
        context = driver.context(driver.state['next'])
        assert 'history' not in context and 'attempts' not in context
        assert context['prior_review'].endswith(prior['digest'] + '/review.json')
        assert driver.state['aggregate_checks'] == original['aggregate_checks']
        responses.append(verdict()); driver.step()
        assert driver.state['status'] == 'awaiting_human', driver.state['feedback']
        package = driver.state['human_package']
        assert driver._package(driver.state) == package == current_package(driver.root, package['digest'])
        assert package['scope_passed'] and package['comment_amendment']['checks_rerun'] is False
        for key in comments.PRESERVED:
            assert package.get(key) == original.get(key)
        assert package['candidate_digest'] != prior['candidate_digest']
        assert package['aggregate_checks']['fix']['source'] == original['source_manifest']
        assert '-old\n' in package['aggregate_diff']['patch']
        assert file_view(driver.root, package['digest'], 'lib.rs', mode='edit')['original_text'] == 'old\n'
        assert 'not rerun' in store.review_readme(package)
        assert review_index(package)['comment_amendment'] == package['comment_amendment']
    with zipfile.ZipFile(store.export_review_package(driver.root, driver.state)) as archive:
        assert archive.read(f"reviews/{original['digest']}/review.json")
        assert archive.read(f"reviews/{prior['digest']}/review.json")
    driver.decide('accept', digest=package['digest'], reason='Human approved')
    assert driver.state['status'] == 'accepted_for_modeling'


def test_comment_review_failure_blocks_ratification_without_starting_full_audit(tmp_path):
    driver, responses = gate(tmp_path); p=driver.state['human_package']
    driver.decide('amend', digest=p['digest'], reason='Wording', comment_only=True, code_edits=[edit(p,'/// ```compile_fail\nfn count() -> i32 { 1 }\n')])
    responses.append(verdict('fail'));driver.step()
    p=driver.state['human_package']
    assert driver.state['status'] == 'awaiting_human' and not p['scope_passed']
    with pytest.raises(ValueError, match='not passed scope'):
        driver.decide('accept', digest=p['digest'], reason='approve')
    driver.decide('amend', digest=p['digest'], reason='Needs broader review', amendment='Fix doctest implications')
    assert driver.state['phase'] == 'audit' and 'comment_amendment' not in driver.state


@pytest.mark.parametrize('comment', ['The result is at most one.', 'Count within bounds.'])
def test_failed_comment_review_can_be_corrected_without_rechecking_implementation(tmp_path, comment):
    driver, responses = gate(tmp_path)
    checked = deepcopy(driver.state['human_package'])
    driver.decide('amend', digest=checked['digest'], reason='Wording', comment_only=True,
                  code_edits=[edit(checked, '/// The result is below one.\nfn count() -> i32 { 1 }\n')])
    responses.append(verdict('fail')); driver.step()
    failed = deepcopy(driver.state['human_package'])
    failed_bytes = (driver.root / 'reviews' / failed['digest'] / 'review.json').read_bytes()
    driver.decide('amend', digest=failed['digest'], reason='Use the inclusive bound', comment_only=True,
                  code_edits=[edit(failed, f'/// {comment}\nfn count() -> i32 {{ 1 }}\n')])
    context = driver.context(driver.state['next'])
    assert context['implementation_source'] == str(driver.root / checked['source'])
    assert context['prior_comment_review']['decision'] == 'fail'
    if comment == 'Count within bounds.':
        assert context['cumulative_changes']['patch'] == ''
    else:
        assert '-/// Count within bounds.' in context['cumulative_changes']['patch']
    assert '-/// The result is below one.' in context['changes']['patch']
    responses.append(lambda root, state, task: dict(verdict()(root, state, task),
        editorial_notes=['An exact value could also be stated.']))
    driver.step()
    package = driver.state['human_package']
    assert package['scope_passed'] and driver.state['phase'] == 'human_gate'
    for key in comments.PRESERVED:
        assert package.get(key) == checked.get(key)
    assert package['aggregate_checks']['fix']['source'] == checked['source_manifest']
    assert 'Editorial note: An exact value' in store.review_readme(package)
    assert (driver.root / 'reviews' / failed['digest'] / 'review.json').read_bytes() == failed_bytes
    with zipfile.ZipFile(store.export_review_package(driver.root, driver.state)) as archive:
        assert archive.read(f"reviews/{failed['digest']}/review.json") == failed_bytes


def test_failed_comment_correction_cannot_hide_prior_implementation_edits(tmp_path):
    driver, responses = gate(tmp_path)
    checked = deepcopy(driver.state['human_package'])
    driver.decide('amend', digest=checked['digest'], reason='Wording', comment_only=True,
                  code_edits=[edit(checked, '/// Bad comment.\nfn count() -> i32 { 1 }\n')])
    responses.append(verdict('fail')); driver.step()
    failed = deepcopy(driver.state['human_package'])
    # A malformed intermediate package cannot make implementation edits eligible
    # merely because the latest correction changes only a comment.
    path = driver.root / failed['source'] / 'lib.rs'
    path.write_text('/// Bad comment.\nfn count() -> i32 { 2 }\n')
    failed['source_manifest'] = store.source_manifest(path.parent)
    with pytest.raises(ValueError, match='all other bytes'):
        comments.validate_edits(driver.root, failed,
            [edit(failed, '/// Corrected comment.\nfn count() -> i32 { 2 }\n')])
    failed['plan']['summary'] = 'Changed requirements'
    with pytest.raises(ValueError, match='preserved plan'):
        comments.implementation_base(driver.root, failed)


@pytest.mark.parametrize('notes', ['text', [None], ['']])
def test_editorial_notes_require_nonempty_strings(notes):
    from trellis.pv_revision.driver import validate_response
    response = dict(decision='pass', reason='Comment metadata only', binding='binding', editorial_notes=notes)
    with pytest.raises(ValueError, match='editorial'):
        validate_response('comment_scope', response)


@pytest.mark.parametrize('defect', ['code', 'delete', 'no_edits', 'not_bool', 'tamper_goal', 'tamper_source', 'stale_verdict'])
def test_comment_boundaries(tmp_path, defect):
    driver, responses=gate(tmp_path);p=driver.state['human_package'];before=deepcopy(driver.state)
    edits=[edit(p,'/// New wording.\nfn count() -> i32 { 1 }\n')]
    if defect == 'code': edits[0]['text']+='fn other() {}\n'
    if defect == 'delete': edits[0]['text']=None
    if defect in {'code','delete','no_edits','not_bool'}:
        with pytest.raises(ValueError):
            driver.decide('amend',digest=p['digest'],reason='wording',comment_only='yes' if defect=='not_bool' else True,code_edits=[] if defect=='no_edits' else edits)
        assert driver.state == before
        return
    driver.decide('amend',digest=p['digest'],reason='wording',comment_only=True,code_edits=edits)
    if defect=='tamper_goal':
        driver.state['proposed_goals'][0]['text']='different'
    elif defect=='tamper_source':
        (driver.root/driver.state['source']/'other.rs').write_text('changed')
    else:
        responses.append(verdict(binding='forged'));driver.step()
        assert driver.state['status']=='active' and 'Stale' in driver.state['feedback'];return
    with pytest.raises(ValueError): driver._current()


def test_guarded_redirect_of_stopped_legacy_amendment(tmp_path):
    driver, responses=gate(tmp_path);prior=deepcopy(driver.state['human_package'])
    edits=[edit(prior,'/// New wording.\nfn count() -> i32 { 1 }\n')]
    driver.decide('amend',digest=prior['digest'],reason='Comments only',code_edits=edits)
    old=deepcopy(driver.state)
    with pytest.raises(ValueError,match='Pause'):
        driver.redirect_comment_amendment(digest=prior['digest'],reason='User requested focused path')
    (driver.root/'pause').touch()
    preview=driver.redirect_comment_amendment(digest=prior['digest'],reason='User requested focused path')
    assert driver.state==old and not list((driver.root/'sources').glob('comments-*'))
    with pytest.raises(ValueError,match='repeat'):
        driver.redirect_comment_amendment(digest=prior['digest'],reason='User requested focused path',apply=True,state_digest='bad')
    driver.redirect_comment_amendment(digest=prior['digest'],reason='User requested focused path',apply=True,state_digest=preview['state_digest'])
    assert driver.state['human_decisions']==old['human_decisions']
    assert driver.state['history']==old['history']
    assert driver.state['phase']=='comment_scope'
    responses.append(verdict());driver.step()
    assert driver.state['human_package']['scope_passed']


def test_comment_path_excludes_harness_even_when_lexically_only_a_comment(tmp_path):
    driver, responses=gate(tmp_path)
    # Create a real declared evidence file in the accepted attempt fixture.
    source=driver.root/driver.state['source']
    (source/'evidence.rs').write_text('// Evidence explanation\n')
    driver.state['source_manifest']=driver.state['attempts'][0]['manifest']=store.source_manifest(source)
    driver.state['tasks']['fix']['authorization']['paths'].append('evidence.rs')
    driver.state['tasks']['fix']['authorization']['evidence_paths']=['evidence.rs']
    driver.state['attempts'][0]['authorization']=deepcopy(driver.state['tasks']['fix']['authorization'])
    driver.state['aggregate_checks']={};driver.state['aggregate_check_history']=[]
    driver.state['human_package']=driver._package(driver.state)
    package=driver.state['human_package']
    assert [r['relative_path'] for r in package['evidence']['source_manifest']['files']]==['evidence.rs']
    before=deepcopy(driver.state)
    with pytest.raises(ValueError,match='limited to production'):
        driver.decide('amend',digest=package['digest'],reason='Evidence wording',comment_only=True,
            code_edits=[edit(package,'// Updated evidence explanation\n','evidence.rs')])
    assert driver.state==before


def test_comment_chain_exports_repackaged_legacy_evidence(tmp_path):
    driver, responses=gate(tmp_path)
    legacy=store.build_review_package_from_state(driver.root,driver.state,schema=store.LEGACY_REVIEW_SCHEMA)
    driver.state['human_package']=legacy
    store.write_review_package(driver.root,legacy);driver.save()
    preview=driver.repackage_review(digest=legacy['digest'],separation_reason='No harness files')
    driver.repackage_review(digest=legacy['digest'],separation_reason='No harness files',apply=True,state_digest=preview['state_digest'])
    package=driver.state['human_package']
    driver.decide('amend',digest=package['digest'],reason='Comment wording',comment_only=True,
        code_edits=[edit(package,'/// The result is one.\nfn count() -> i32 { 1 }\n')])
    responses.append(verdict());driver.step()
    assert driver.state['status']=='awaiting_human',driver.state['feedback']
    with zipfile.ZipFile(store.export_review_package(driver.root,driver.state)) as archive:
        assert archive.read(f"reviews/{legacy['digest']}/review.json")
        assert archive.read(f"reviews/{package['digest']}/review.json")
    prior_file = driver.root / 'reviews' / legacy['digest'] / 'review.json'
    prior_file.chmod(0o600)
    prior_file.write_text('{}')
    with pytest.raises(ValueError, match='Prior comment review evidence changed'):
        store.export_review_package(driver.root, driver.state)


def test_redirect_rejects_advanced_audit_and_changed_evidence(tmp_path):
    driver,responses=gate(tmp_path);p=driver.state['human_package']
    driver.decide('amend',digest=p['digest'],reason='wording',code_edits=[edit(p,'/// Updated.\nfn count() -> i32 { 1 }\n')])
    (driver.root/'pause').touch()
    old=deepcopy(driver.state)
    driver.state['attempts'].append({'unreviewed':'new attempt'})
    with pytest.raises(ValueError,match='advanced'):
        driver.redirect_comment_amendment(digest=p['digest'],reason='redirect')
    driver.state=old;responses.append(plan());driver.step()
    with pytest.raises(ValueError,match='untouched'):
        driver.redirect_comment_amendment(digest=p['digest'],reason='redirect')

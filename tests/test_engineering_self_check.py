"""Differential checks against the source-matched native artifact checker."""
import copy
import hashlib
import json
from pathlib import Path
import subprocess
import sys

import pytest

from trellis import engineering_check as check


ROOT = Path(__file__).resolve().parents[1]
KERNEL = ROOT / 'kernel/target/debug/trellis_runtime_cli'
PLAN = dict(requirements=[dict(source='Section 3', description='Implement the algorithm.',
                              consequences='Preserve its invariant.')],
            interpretations=[], unspecified=[], departures=[])


def native(action, **fields):
    result = subprocess.run([str(KERNEL)], input=json.dumps(dict(action=action, **fields)),
                            text=True, capture_output=True, timeout=20, check=True)
    return json.loads(result.stdout)


@pytest.fixture(scope='module', autouse=True)
def compiled_identity():
    identity = native('acceptance_implementation_identity')
    sources = {entry['path']: entry['sha256'] for entry in identity['sources']}
    for name in ('trellis/engineering_check.py', 'kernel/src/engineering.rs'):
        assert sources[name] == hashlib.sha256((ROOT / name).read_bytes()).hexdigest()


def contract(purpose):
    result = native('engineering_artifact_contract', purpose=purpose)
    assert result['checker_sha256'] == hashlib.sha256(Path(check.__file__).read_bytes()).hexdigest()
    return result['contract']


def compare(purpose, raw, record=None):
    rule = contract(purpose)
    outcome = native('check_engineering_artifacts', purpose=purpose, raw_payload=raw, record=record)['output']
    try:
        local = check.check(rule, raw, record)
    except check.Invalid:
        assert not outcome['ok'], (purpose, raw, record, outcome)
    else:
        assert outcome['ok'], (purpose, raw, record, outcome)
        assert local == outcome['data']
    return outcome


@pytest.mark.parametrize('purpose', ['source_reading', 'plan', 'implementation', 'plan_review',
                                    'code_review', 'retry_review', 'performance'])
def test_every_purpose_native_parity(purpose):
    raw = (dict(action='fail', reason='No usable implementation.') if purpose == 'performance' else
           dict(decision='revise' if purpose == 'retry_review' else 'accept', reason='Reviewed.')
           if purpose.endswith('review') else dict(summary='Completed.'))
    assert compare(purpose, raw, PLAN)['ok']
    for field in raw:
        for replacement in [None, True, 3, [], {}, '', ' \n\t']:
            changed = {**raw, field: replacement}
            compare(purpose, changed, PLAN)
    compare(purpose, {**raw, 'unexpected': 'extra'}, PLAN)


@pytest.mark.parametrize('change', ['extra_top', 'missing', 'empty', 'extra_entry', 'entry_type', 'list_type'])
def test_closed_record_schema(change):
    record = copy.deepcopy(PLAN)
    if change == 'extra_top': record['assessment'] = 'extra'
    elif change == 'missing': del record['departures']
    elif change == 'empty': record['requirements'] = []
    elif change == 'extra_entry': record['requirements'][0]['authority'] = 'extra'
    elif change == 'entry_type': record['requirements'][0] = 'text'
    else: record['departures'] = True
    assert not compare('source_reading', {'summary': 'Read.'}, record)['ok']


@pytest.mark.parametrize('text', ['\x1c', '\x1f', '\x85', '\u200b', '\u2000', '\ufeff', ' α ', '\t'])
def test_record_and_envelope_unicode_semantics(text):
    record = copy.deepcopy(PLAN); record['requirements'][0]['source'] = text
    compare('plan', {'summary': 'Plan.'}, record)
    compare('source_reading', {'summary': text}, PLAN)


COMMAND = dict(name='trial', argv=['cargo', 'run', '--', ''], cwd='implementation', kind='run')


@pytest.mark.parametrize('field,values', [
    ('name', ['', '1', 'x/y', 'x'*128, 'x'*129, ' α ', None, ' trial ']),
    ('argv', [[], [''], [' '], ['echo', ''], ['echo', '\0'], [True], ['echo']*257]),
    ('cwd', ['.', '..', '/tmp', 'implementation/x', 'implementation//x', 'harness', 'harness/../x', 'harness\\x', 'build\x85']),
    ('kind', ['run', ' Build ', 'fail', True]),
    ('timeout_seconds', [True, False, None, 0, -1, 0.001, 3600, 3600.1, '3']),
])
def test_performance_command_native_parity(field, values):
    for value in values:
        compare('performance', dict(action='measure', reason='Measure representative work.',
                                    commands=[{**COMMAND, field: value}]))


@pytest.mark.parametrize('raw', [
    dict(action='measure', reason='Measure.', commands=[]),
    dict(action='measure', reason='Measure.', commands=[COMMAND, COMMAND]),
    dict(action='measure', reason='Measure.', commands=[{**COMMAND, 'extra': 1}]),
    dict(action=' MEASURE ', reason=' Measure. ', commands=[COMMAND]),
    dict(action='improve', reason='Measured bottleneck.', instruction='Improve the implementation.'),
    dict(action='accept', reason='Ready.', evidence=['run1'], report='benchmarks/report.md'),
    dict(action='accept', reason='Ready.', evidence=['run1', 'run1'], report='benchmarks/report.md'),
    dict(action='accept', reason='Ready.', evidence=['run1'], report='elsewhere/report.md'),
    dict(action='fail', reason='Unavailable.', commands=[COMMAND]),
    dict(action='fail', reason='Unavailable.', system_feedback=None),
    dict(action='fail', reason='Unavailable.', system_feedback=3),
])
def test_performance_actions_native_parity(raw):
    compare('performance', raw)


def test_duplicate_keys_follow_existing_json_object_semantics(tmp_path):
    path = tmp_path/'result.json'
    path.write_text('{"summary":"old","summary":"current"}')
    assert compare('source_reading', check.load(path), PLAN)['data'] == {'summary': 'current'}


@pytest.mark.parametrize('discarded', [
    '"previous"', '1e400', '"\\ud800"', '{"\\ud800":0}',
    '[' * 126 + '0' + ']' * 126, '[' * 127 + '0' + ']' * 127,
    '[' * 1500 + '0' + ']' * 1500, '9' * 1000,
])
def test_text_parser_checks_discarded_duplicate_values(tmp_path, discarded):
    raw = '{"reason":' + discarded + ',"reason":"ok","decision":"accept"}'
    path = tmp_path / 'raw.json'; path.write_text(raw)
    native_result = native('check_engineering_artifact_text', purpose='plan_review',
                          raw_text=raw, record_text=None)['output']
    try:
        parsed = check.load(path)
        result = check.check(contract('plan_review'), parsed)
    except check.Invalid:
        assert not native_result['ok']
    else:
        assert native_result['ok'] and native_result['data'] == result


@pytest.mark.parametrize('change', ['version', 'unknown', 'missing', 'wrong_type'])
def test_unsupported_contract_refused(change):
    value = contract('source_reading')
    if change == 'version': value['version'] = 2
    elif change == 'unknown': value['execute'] = 'arbitrary'
    elif change == 'missing': del value['record_fields']
    else: value['max_commands'] = True
    with pytest.raises(check.Invalid): check.check(value, {'summary': 'Read.'}, PLAN)


def test_standalone_check_binds_exact_script_contract_and_marker(tmp_path):
    script = tmp_path/'checker.py'; script.write_bytes(Path(check.__file__).read_bytes())
    rules = tmp_path/'contract.json'; rules.write_text(json.dumps(contract('source_reading')))
    result = tmp_path/'result.json'; result.write_text('{"summary":"Read the supplied paper."}')
    (tmp_path/'reading.json').write_text(json.dumps(PLAN))
    done = tmp_path/'done'
    args = [sys.executable, '-I', '-S', str(script), str(rules), hashlib.sha256(rules.read_bytes()).hexdigest(),
            hashlib.sha256(script.read_bytes()).hexdigest(), str(result), str(tmp_path), str(done)]
    subprocess.run([*args, '--raw-only'], check=True, capture_output=True)
    assert not done.exists()
    subprocess.run(args, check=True, capture_output=True)
    assert done.is_file()
    done.unlink(); rules.write_text(rules.read_text()+'\n')
    bad = subprocess.run(args, capture_output=True, text=True)
    assert bad.returncode == 1 and 'stale' in bad.stdout and not done.exists()

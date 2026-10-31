"""Standalone engineering artifact check. No execution or acceptance authority."""
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sys


# Match Rust's Unicode White_Space predicate, used by the artifact normalizer.
WHITESPACE = '\t\n\v\f\r \x85\xa0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a\u2028\u2029\u202f\u205f\u3000'


class Invalid(ValueError):
    def __init__(self, path, rule):
        self.diagnostic = dict(path=path, rule=rule)
        super().__init__(path + ': ' + rule)


def _require(ok, path, rule):
    if not ok:
        raise Invalid(path, rule)


def _text(value, path, *, trim=True, optional=False):
    if optional and value is None:
        return ''
    _require(isinstance(value, str), path, 'type')
    try:
        value.encode('utf-8')
    except UnicodeError:
        raise Invalid(path, 'type') from None
    result = value.strip(WHITESPACE)
    _require(optional or bool(result), path, 'nonempty')
    return result if trim else value


def _closed(value, fields, path, optional=()):
    _require(isinstance(value, dict), path, 'type')
    _require(set(fields) <= value.keys(), path, 'required')
    _require(value.keys() <= set(fields) | set(optional), path, 'closed_fields')


def _identifier(value, path, limit):
    value = _text(value, path)
    _require(bool(re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]*', value)) and len(value) <= limit,
             path, 'type')
    return value


def _path(value, path, *, allow_dot=False):
    text = _text(value, path)
    _require(text == value and '\\' not in text and not any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in text),
             path, 'type')
    _require((allow_dot and text == '.') or all(part not in ('', '.', '..') for part in text.split('/')),
             path, 'type')
    return text


def _ascii_lower(value):
    return value.translate(str.maketrans('ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz'))


def _performance(raw, contract):
    _require(isinstance(raw, dict), 'result', 'type')
    action = _ascii_lower(_text(raw.get('action'), 'result.action'))
    fields = contract['performance_fields']
    _require(action in fields, 'result.action', 'enum')
    _closed(raw, fields[action], 'result', ['system_feedback'])
    _text(raw.get('system_feedback'), 'result.system_feedback', optional=True)
    result = dict(action=action, reason=_text(raw['reason'], 'result.reason'))
    if action == 'measure':
        commands = raw['commands']
        _require(isinstance(commands, list) and 1 <= len(commands) <= contract['max_commands'],
                 'result.commands', 'type')
        result['commands'], names = [], set()
        for index, command in enumerate(commands):
            path = 'result.commands[' + str(index) + ']'
            _closed(command, contract['command_fields'], path, ['timeout_seconds'])
            name = _identifier(command['name'], path + '.name', contract['identifier_bytes'])
            _require(name not in names, path + '.name', 'type')
            names.add(name)
            kind = _ascii_lower(_text(command['kind'], path + '.kind'))
            _require(kind in contract['command_kinds'], path + '.kind', 'enum')
            cwd = _path(command['cwd'], path + '.cwd', allow_dot=True)
            _require(cwd == '.' or cwd.split('/')[0] in contract['command_roots'], path + '.cwd', 'type')
            argv = command['argv']
            _require(isinstance(argv, list) and 1 <= len(argv) <= contract['max_arguments'], path + '.argv', 'type')
            for position, argument in enumerate(argv):
                apath = path + '.argv[' + str(position) + ']'
                _require(isinstance(argument, str) and '\0' not in argument, apath, 'type')
                if position == 0:
                    _text(argument, apath)
                else:
                    _text(argument, apath, optional=True)
            item = dict(name=name, kind=kind, cwd=cwd, argv=argv)
            if 'timeout_seconds' in command:
                seconds = command['timeout_seconds']
                _require(type(seconds) in (float, int), path + '.timeout_seconds', 'type')
                try:
                    finite = math.isfinite(seconds)
                except OverflowError:
                    finite = False
                _require(finite and 0 < seconds <= contract['max_timeout_seconds'], path + '.timeout_seconds', 'type')
                item['timeout_seconds'] = seconds
            result['commands'].append(item)
    elif action == 'improve':
        result['instruction'] = _text(raw['instruction'], 'result.instruction')
    elif action == 'accept':
        evidence = raw['evidence']
        _require(isinstance(evidence, list) and bool(evidence), 'result.evidence', 'type')
        result['evidence'] = [_identifier(item, 'result.evidence[' + str(i) + ']', contract['identifier_bytes'])
                              for i, item in enumerate(evidence)]
        _require(len(set(result['evidence'])) == len(result['evidence']), 'result.evidence', 'type')
        result['report'] = _path(raw['report'], 'result.report')
        _require(result['report'].startswith(contract['report_prefix']), 'result.report', 'type')
    elif action == 'reset_budget':
        for key in ('baseline_evidence','current_evidence'):
            values=raw[key]
            _require(isinstance(values,list) and bool(values), 'result.'+key, 'type')
            result[key]=[_text(value,'result.'+key) for value in values]
            _require(len(set(result[key]))==len(result[key]) and all(re.fullmatch('[0-9a-f]{32}',value) for value in result[key]),'result.'+key,'type')
        result['metric']=_text(raw['metric'],'result.metric')
        result['improvement_bps']=raw['improvement_bps']
        _require(type(result['improvement_bps']) is int and 1000<=result['improvement_bps']<=4294967295,'result.improvement_bps','type')
    return result


def validate_contract(contract):
    fields = {'version', 'purpose', 'record_path', 'record_fields', 'entry_fields', 'nonempty_list',
              'summary_fields', 'review_fields', 'decisions', 'performance_fields', 'command_fields',
              'command_kinds', 'command_roots', 'max_commands', 'max_arguments', 'max_timeout_seconds',
              'identifier_bytes', 'report_prefix'}
    _closed(contract, fields, 'contract')
    _require(type(contract['version']) is int and contract['version'] == 1, 'contract.version', 'enum')
    _require(contract['purpose'] in ('source_reading', 'plan', 'implementation', 'plan_review',
                                    'code_review', 'retry_review', 'performance'), 'contract.purpose', 'enum')
    for name in ('record_fields', 'entry_fields', 'summary_fields', 'review_fields', 'decisions',
                 'command_fields', 'command_kinds', 'command_roots'):
        value = contract[name]
        _require(isinstance(value, list) and all(isinstance(v, str) and v for v in value)
                 and len(set(value)) == len(value), 'contract.' + name, 'type')
    for name in ('max_commands', 'max_arguments', 'identifier_bytes'):
        _require(type(contract[name]) is int and contract[name] > 0, 'contract.' + name, 'type')
    _require(type(contract['max_timeout_seconds']) in (int, float)
             and math.isfinite(contract['max_timeout_seconds']) and contract['max_timeout_seconds'] > 0,
             'contract.max_timeout_seconds', 'type')
    _require(contract['nonempty_list'] in contract['record_fields'], 'contract.nonempty_list', 'type')
    _require(contract['record_path'] in (None, 'reading.json', 'plan.json'), 'contract.record_path', 'enum')
    _require(contract['report_prefix'] == 'benchmarks/', 'contract.report_prefix', 'enum')
    actions = contract['performance_fields']
    _closed(actions, ('measure', 'improve', 'accept', 'fail'), 'contract.performance_fields', ['reset_budget'] if contract['purpose']=='performance' else ())
    for action, fields in actions.items():
        _require(isinstance(fields, list) and all(isinstance(v, str) for v in fields)
                 and len(set(fields)) == len(fields), 'contract.performance_fields', 'type')


def check(contract, raw, record=None):
    """Validate only the closed artifact shapes named by kernel-authored data."""
    validate_contract(contract)
    purpose = contract['purpose']
    if purpose == 'performance':
        return _performance(raw, contract)
    if purpose.endswith('review'):
        _closed(raw, contract['review_fields'], 'result')
        decision = _ascii_lower(_text(raw['decision'], 'result.decision'))
        _require(decision in contract['decisions'], 'result.decision', 'enum')
        return dict(decision=decision, reason=_text(raw['reason'], 'result.reason'))
    _closed(raw, contract['summary_fields'], 'result')
    result = dict(summary=_text(raw['summary'], 'result.summary'))
    if contract['record_path'] is not None:
        path = contract['record_path']
        _closed(record, contract['record_fields'], path)
        for field in contract['record_fields']:
            items = record[field]
            _require(isinstance(items, list), path + '.' + field, 'type')
            _require(field != contract['nonempty_list'] or bool(items), path + '.' + field, 'nonempty')
            for index, entry in enumerate(items):
                entry_path = path + '.' + field + '[' + str(index) + ']'
                _closed(entry, contract['entry_fields'], entry_path)
                for name in contract['entry_fields']:
                    # Preserve original record text; Python's record predicate
                    # also treats the four ASCII information separators as space.
                    value = _text(entry[name], entry_path + '.' + name, trim=False)
                    _require(bool(value.strip()), entry_path + '.' + name, 'nonempty')
    return result


def performance_budget(data,budget):
    """Shared finite decision predicate; immutable host metadata supplies provenance."""
    _closed(budget,('schema','predecessor_sha256','used','remaining','hard_limit','guidance','baseline','measurements',
                   'current_candidate_sha256','current_reviewed_candidate_sha256'),'performance_budget')
    _require(budget['schema']=='trellis-native-performance-budget/v1' and budget['hard_limit']==4
             and budget['guidance']==3 and type(budget['used']) is int and budget['used']>=0
             and type(budget['remaining']) is int and budget['remaining']==max(0,4-budget['used']), 'performance_budget','stale')
    for key in ('predecessor_sha256','current_candidate_sha256','current_reviewed_candidate_sha256'):
        _require(budget[key] is None or isinstance(budget[key],str) and re.fullmatch('[0-9a-f]{64}',budget[key]),'performance_budget','stale')
    records=budget['measurements'];_require(isinstance(records,dict),'performance_budget','type')
    for run,row in records.items():
        _require(isinstance(run,str) and re.fullmatch('[0-9a-f]{32}',run),'performance_budget','type')
        _closed(row,('measurement_sha256','position','group','implementation','eligible','successful_run','current'),'performance_budget')
        _require(isinstance(row['measurement_sha256'],str) and re.fullmatch('[0-9a-f]{64}',row['measurement_sha256'])
                 and type(row['position']) is int and row['position']>=0,'performance_budget','type')
        for key,count in (('group',4),('implementation',3)):
            _require(isinstance(row[key],list) and len(row[key])==count and all(isinstance(value,str) and re.fullmatch('[0-9a-f]{64}',value) for value in row[key]),'performance_budget','type')
        _require(all(type(row[key]) is bool for key in ('eligible','successful_run','current')),'performance_budget','type')
    if data['action'] in {'improve','propose'}:
        _require(budget['remaining']>0,'performance_budget','failed_check')
    if data['action']!='reset_budget':return
    _require(type(data.get('improvement_bps')) is int and 1000<=data['improvement_bps']<=4294967295,'result.improvement_bps','type')
    baseline=budget['baseline']
    _require(isinstance(baseline,dict),'evidence','stale')
    _closed(baseline,('evidence','metric','group','implementation','after_position','fixed'),'performance_budget')
    _require(type(baseline['fixed']) is bool and type(baseline['after_position']) is int,'performance_budget','type')
    sides=[]
    for key in ('baseline_evidence','current_evidence'):
        ids=data[key]
        _require(isinstance(ids,list) and bool(ids) and len(set(ids))==len(ids) and all(run in records for run in ids),'evidence','stale')
        rows=[records[run] for run in ids]
        _require(len({tuple(row['group']) for row in rows})==1 and all(row['eligible'] for row in rows)
                 and any(row['successful_run'] for row in rows),'evidence','stale')
        sides.append(rows)
    prior,current=sides
    _require(prior[0]['group']==baseline['group'] and prior[0]['implementation']==baseline['implementation'],'evidence','stale')
    if baseline['fixed']:
        _require(sorted(data['baseline_evidence'])==sorted(baseline['evidence']) and data['metric']==baseline['metric'],'evidence','stale')
    _require(all(row['current'] for row in current) and prior[0]['implementation']!=current[0]['implementation']
             and min(row['position'] for row in current)>max(row['position'] for row in prior)
             and min(row['position'] for row in current)>baseline['after_position'],'evidence','stale')


def load(path):
    class Pairs(list):
        pass

    def invalid_constant(_):
        raise Invalid('artifact', 'type')

    def normalize(value, depth=0):
        # serde_json validates every value before duplicate keys are collapsed.
        if isinstance(value, (Pairs, list)):
            _require(depth < 127, 'artifact', 'type')
            if isinstance(value, Pairs):
                result = {}
                for key, item in value:
                    key.encode('utf-8')
                    result[key] = normalize(item, depth + 1)
                return result
            return [normalize(item, depth + 1) for item in value]
        if isinstance(value, str):
            value.encode('utf-8')
        elif type(value) in (int, float):
            _require(math.isfinite(value), 'artifact', 'type')
        return value

    try:
        parsed = json.loads(Path(path).read_bytes().decode('utf-8'),
                            parse_constant=invalid_constant, object_pairs_hook=Pairs)
        return normalize(parsed)
    except (UnicodeError, ValueError, OverflowError, RecursionError):
        raise Invalid('artifact', 'type') from None


def _implementation_source(record_root):
    source = Path(record_root) / 'source'
    if source.is_symlink() or not source.is_dir():
        return
    excluded = ('target', '__pycache__')
    found = set()
    def refuse(error):
        raise error
    for _, directories, files in os.walk(source, followlinks=False, onerror=refuse):
        found.update(name for name in (*directories, *files) if name in excluded)
        directories[:] = [name for name in directories if name not in excluded]
    for component in excluded:
        _require(component not in found, 'source.' + component, 'closed_fields')


def main(argv):
    # The bound shell command supplies these identities; no generated code.
    raw_only=bool(argv and argv[-1]=='--raw-only')
    arguments=argv[:-1] if raw_only else argv
    if len(arguments) not in (6,8) or len(arguments)==8 and arguments[6]!='--performance-budget-sha256':
        raise Invalid('arguments', 'type')
    contract_path, contract_sha, script_sha, result_path, record_root, done_path = arguments[:6]
    _require(hashlib.sha256(Path(__file__).read_bytes()).hexdigest() == script_sha, 'checker', 'stale')
    _require(hashlib.sha256(Path(contract_path).read_bytes()).hexdigest() == contract_sha, 'contract', 'stale')
    contract = load(contract_path)
    validate_contract(contract)
    has_budget=contract['purpose']=='performance' and 'reset_budget' in contract['performance_fields']
    _require(has_budget==(len(arguments)==8),'performance_budget','required')
    record = load(Path(record_root) / contract['record_path']) if contract['record_path'] else None
    data=check(contract, load(result_path), record)
    if has_budget:
        path=Path(record_root)/'performance-budget.json'
        _require(hashlib.sha256(path.read_bytes()).hexdigest()==arguments[7],'performance_budget','stale')
        performance_budget(data,load(path))
    if contract['purpose'] == 'implementation':
        _implementation_source(record_root)
    if not raw_only:
        # A marker only ends this provider turn. The host checks sealed bytes.
        fd = os.open(done_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        os.close(fd)
        print('Submitted. End your turn.')
    else:
        print('Artifact shape passed.')


if __name__ == '__main__':
    try:
        main(sys.argv[1:])
    except Invalid as error:
        print(json.dumps(error.diagnostic, sort_keys=True))
        raise SystemExit(1)
    except OSError:
        print('{"path":"artifact","rule":"required"}')
        raise SystemExit(1)

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
    _closed(actions, ('measure', 'improve', 'accept', 'fail'), 'contract.performance_fields')
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


def main(argv):
    # The bound shell command supplies these identities; no generated code.
    if len(argv) not in (6, 7) or (len(argv) == 7 and argv[-1] != '--raw-only'):
        raise Invalid('arguments', 'type')
    contract_path, contract_sha, script_sha, result_path, record_root, done_path = argv[:6]
    _require(hashlib.sha256(Path(__file__).read_bytes()).hexdigest() == script_sha, 'checker', 'stale')
    _require(hashlib.sha256(Path(contract_path).read_bytes()).hexdigest() == contract_sha, 'contract', 'stale')
    contract = load(contract_path)
    validate_contract(contract)
    record = load(Path(record_root) / contract['record_path']) if contract['record_path'] else None
    check(contract, load(result_path), record)
    if len(argv) == 6:
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

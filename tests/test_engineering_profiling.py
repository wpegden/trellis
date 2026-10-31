"""Checked Callgrind delivery uses the existing owned measurement boundary."""
import hashlib
import json
from pathlib import Path
import re

import pytest

from trellis.modeling import engineering as eng, performance as perf
from test_paper_performance_measurements import candidate, measure
from test_paper_engineering import root


@pytest.mark.parametrize('failure,expected', [
    ('none', 'available'), ('missing', 'missing_tools'), ('timeout', 'timeout'),
    ('execution', 'failed_execution'), ('output_limit', 'output_limit'),
    ('version', 'invalid_version'), ('profile', 'invalid_profile'),
    ('large_profile', 'invalid_profile'), ('large_counter', 'invalid_profile'),
    ('annotation_exit', 'failed_annotation'), ('annotation', 'invalid_annotation'),
    ('large_annotation', 'invalid_annotation'), ('missing_profile', 'invalid_profile')])
def test_checked_capability_categories(tmp_path, monkeypatch, failure, expected):
    monkeypatch.setattr(perf, '_selected_tools', lambda root: (dict(executables={}, route='rust_aeneas'), []))
    def executable(command, *args):
        name = command['argv'][0]
        return dict(argv0=name, sha256=None if failure == 'missing' and name == 'valgrind' else 'a' * 64), None
    monkeypatch.setattr(perf, '_executable', executable)
    monkeypatch.setattr(perf, '_sandbox', lambda workspace, roots, environment, command: command['argv'])
    def execute(argv, *, cwd, env, stdout, stderr, timeout):
        assert env == {} and timeout == 20
        stdout.write_text(''); stderr.write_text('private host diagnostic must never be published')
        stage = stdout.stem
        if stage == 'version':
            stdout.write_text('unexpected private output' if failure == 'version' else 'valgrind-3.25.1\n')
        elif stage == 'profile' and failure != 'missing_profile':
            profile = 'events: Ir\nsummary: 1234\n'
            if failure == 'profile': profile = 'events: Dr\nsummary: 1234\n'
            if failure == 'large_profile': profile = 'x' * (1024 * 1024 + 1)
            if failure == 'large_counter': profile = 'events: Ir\nsummary: ' + '9' * 5000 + '\n'
            (cwd / 'output/callgrind.out').write_text(profile)
        elif stage == 'annotation':
            stdout.write_text("Profile data file '/output/callgrind.out'\n1,234 (100.0%) PROGRAM TOTALS\n")
            if failure == 'annotation': stdout.write_text('1234 PROGRAM TOTALS\n')
            if failure == 'large_annotation': stdout.write_text('x' * (1024 * 1024 + 1))
        return dict(exit_code=1 if failure == 'execution' or failure == 'annotation_exit' and stage == 'annotation' else 0,
            timeout=failure == 'timeout', output_limit_exceeded=failure == 'output_limit', execution_error='')
    monkeypatch.setattr(perf, '_run_bounded', execute)
    result = eng._profiling_tools(tmp_path)
    assert result['status'] == expected and result['available'] == (expected == 'available')
    assert result['metric'] == 'Ir' and result['identity_scope'] == 'command entrypoints'
    encoded = json.dumps(result)
    assert str(tmp_path) not in encoded and 'private' not in encoded
    assert 'perf' not in result['commands']
    assert not {'id', 'record_sha256', 'successful_run'} & result.keys()


@pytest.mark.parametrize('error', [KeyboardInterrupt, RuntimeError])
def test_probe_preserves_cancellation_and_ownership_errors(tmp_path, monkeypatch, error):
    monkeypatch.setattr(perf, '_selected_tools', lambda root: (dict(executables={}), []))
    monkeypatch.setattr(perf, '_executable', lambda *args: (dict(sha256='a' * 64), None))
    monkeypatch.setattr(perf, '_sandbox', lambda *args: [])
    original = error('retained original interruption')
    def interrupted(*args, **kwargs): raise original
    monkeypatch.setattr(perf, '_run_bounded', interrupted)
    with pytest.raises(error) as caught: eng._profiling_tools(tmp_path)
    assert caught.value is original


def test_probe_rechecks_entrypoints_before_publication(tmp_path, monkeypatch):
    monkeypatch.setattr(perf, '_selected_tools', lambda root: (dict(executables={}), []))
    calls = 0
    def changed(*args):
        nonlocal calls
        calls += 1
        return dict(sha256='a' * 64 if calls <= 3 else 'b' * 64), None
    monkeypatch.setattr(perf, '_executable', changed)
    monkeypatch.setattr(perf, '_sandbox', lambda *args: [])
    monkeypatch.setattr(perf, '_run_bounded', lambda *args, **kwargs: dict(exit_code=1, timeout=False))
    with pytest.raises(RuntimeError, match='identity changed'): eng._profiling_tools(tmp_path)


@pytest.mark.parametrize('purpose', ['source_reading', 'plan', 'plan_review', 'implementation', 'code_review', 'performance'])
def test_profiling_delivery_scope_and_full_prompt(root, tmp_path, monkeypatch, purpose):
    state = json.loads((root / '.trellis/modeling/state.json').read_text())
    calls = []
    def unavailable(preparation):
        calls.append(preparation)
        return dict(available=False, status='missing_tools', metric='Ir')
    monkeypatch.setattr(eng, '_profiling_tools', unavailable)
    view = tmp_path / ('view-' + purpose)
    delivery = eng.delivery(root, state, dict(role='reviewer', purpose=purpose), view)
    prompt = (view / 'prompt.md').read_text()
    supplied = purpose in {'implementation', 'performance'}
    assert len(calls) == int(supplied)
    assert ('profiling-tools.json' in delivery['files']) == supplied
    assert prompt.count('profiling-tools.json describes') == int(supplied)
    assert prompt.count('Use any supplied examples as a starting point, and evaluate performance across a broader range of realistic inputs and workloads.') == int(purpose == 'performance')
    if supplied:
        raw = (view / 'profiling-tools.json').read_bytes()
        assert hashlib.sha256(raw).hexdigest() == delivery['files']['profiling-tools.json']
        assert not json.loads(raw)['available']
    if purpose == 'performance':
        assert prompt.count('Aggressively look for performance improvements') == 1
    assert all(value not in prompt for value in ('Aeneas', 'phase 2', 'Lean verification'))
    (tmp_path / 'prompt-audit.json').write_text(json.dumps(dict(purpose=purpose, prompt=prompt, files=delivery['files']), indent=2))


def test_real_optimized_rust_callgrind_and_retained_annotation(tmp_path):
    rust = Path('/path/to/trellis/.rustup/toolchains/nightly-2026-06-01-x86_64-unknown-linux-gnu/bin/rustc')
    root = candidate(tmp_path, tools=dict(cargo=str(rust.with_name('cargo')), rustc=str(rust)))
    capability = eng._profiling_tools(root)
    (tmp_path / 'capability.json').write_text(json.dumps(capability, indent=2))
    assert capability['available'], capability
    source = '''use std::hint::black_box;
#[inline(never)] pub fn hot_work(mut x: u64) -> u64 {
    for _ in 0..250_000 { x = black_box(x.wrapping_mul(1664525).wrapping_add(1013904223)); }
    x
}
#[inline(never)] pub fn cold_work(mut x: u64) -> u64 {
    for _ in 0..2_500 { x = black_box(x.wrapping_mul(1103515245).wrapping_add(12345)); }
    x
}
pub fn algorithm() -> u64 { black_box(hot_work(1)) ^ black_box(cold_work(7)) }
'''
    (root / 'work/crate/src/lib.rs').write_text(source)
    (root / 'benchmarks/harness/main.rs').write_text(
        '#[path="/implementation/src/lib.rs"] mod implementation;\n'
        'fn main(){let start=std::time::Instant::now();let x=implementation::algorithm();'
        'println!("checksum={} elapsed_ns={}",x,start.elapsed().as_nanos());}\n')
    commands = [
        dict(name='compile', argv=['rustc', '-C', 'opt-level=3', '-C', 'debuginfo=2', '--crate-name', 'callgrind_fixture',
                                  '/harness/main.rs', '-o', '/build/benchmark'], cwd='build', kind='build'),
        dict(name='native', argv=['/build/benchmark'], cwd='build', kind='run'),
        dict(name='callgrind', argv=capability['profile_argv'], cwd='build', kind='run'),
        dict(name='annotate', argv=[*capability['annotate_argv'][:-1], '--threshold=100', capability['annotate_argv'][-1]], cwd='build', kind='run')]
    record = perf.run_measurement(root, dict(sequence=1), measure(*commands))
    assert all(row['exit_code'] == 0 and not row['timeout'] for row in record['commands']), record['commands']
    assert perf.validate_evidence(root, {}, [record['id']])[0]['id'] == record['id']
    directory = root / perf.STORE / 'runs' / record['id']
    profile_path = directory / 'artifacts/output/callgrind.out'
    profile = profile_path.read_text()
    annotation = (directory / record['commands'][-1]['stdout']).read_text()
    assert int(re.search(r'(?m)^summary: ([0-9]+)$', profile)[1]) > 0
    assert 'hot_work' in profile and 'cold_work' in profile and 'cfn=' in profile
    assert 'hot_work' in annotation and 'cold_work' in annotation and 'lib.rs' in annotation
    assert 'wrapping_mul' in annotation and 'PROGRAM TOTALS' in annotation
    assert 'elapsed_ns=' in (directory / record['commands'][1]['stdout']).read_text()
    assert record['evidence_files']['artifacts/output/callgrind.out'] == hashlib.sha256(profile_path.read_bytes()).hexdigest()
    (tmp_path / 'measurement.json').write_text(json.dumps(dict(id=record['id'], record_sha256=record['record_sha256'],
        commands=record['commands'], profile_sha256=record['evidence_files']['artifacts/output/callgrind.out']), indent=2))

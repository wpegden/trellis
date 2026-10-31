"""Owned synthetic clock fixtures; no model, native runtime or Lean execution."""
import hashlib
import json
import os
from pathlib import Path
import time

import pytest

from trellis.agent_wrapper import provider_clock
from trellis.checker import provider_progress as progress
from trellis.checker.jobs import JobService, register_workspace
from trellis.dispatch.candidate import canonical_json
from trellis.dispatch.process import identity


def put(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(canonical_json(value))


def fixture(tmp_path, *, allowance=10000, turn=1, task_kind='lean_closure', session=None):
    root = tmp_path / 'execution'
    spec = dict(compiler='managed', scope='1/turns/1/execution')
    provider = dict(provider='codex', model='test', reasoning_effort='high')
    binding = dict(provider, home=str(tmp_path / 'home'), thread_id='') if session is None else session
    recipe = dict(checker=spec, payload=dict(request=dict(kind='Worker',
        task_execution=dict(task_id=1, task_kind=task_kind, compiler='managed', binding=provider),
        worker_submission_policy=dict(max_infrastructure_retries=2)),
        worker_turn=dict(turn=turn, remaining_ms=allowance, session=session,
                         execution_record=str(root / 'clock'))))
    put(root / 'recipe.json', recipe)
    for name in ('process', 'permit'):
        put(root / (name + '.json'), identity(os.getpid()))
    descriptor = progress.descriptor(root, spec)
    config = provider_clock.prepare(root / 'clock', allowance_ms=allowance,
        binding=binding, output=tmp_path / 'output')
    return root, spec, descriptor, config


def start(directory, *, ago_ms=1250, thread='thread'):
    config = json.loads((directory / 'config.json').read_bytes())
    common = dict(config_sha256=hashlib.sha256((directory / 'config.json').read_bytes()).hexdigest(),
                  owner=identity(os.getpid()))
    put(directory / 'owner.json', common)
    put(directory / 'started.json', dict(common, allowance_ms=config['allowance_ms'],
        monotonic_ns=time.monotonic_ns() - ago_ms * 1_000_000, started_at_ms=0))
    put(directory / 'session.json', dict(common, thread_id=thread))
    return common


def test_descriptor_precedes_clock_and_samples_without_writing(tmp_path):
    root, spec, binding, config = fixture(tmp_path)
    assert binding is not None
    assert binding['recipe_sha256'] == hashlib.sha256((root / 'recipe.json').read_bytes()).hexdigest()
    assert progress.sample(binding) is None
    start(root / 'clock')
    before = {str(p): p.read_bytes() for p in root.rglob('*') if p.is_file()}
    first = progress.sample(binding)
    assert 8600 <= first['remaining_ms'] <= 8750
    assert first['turn'] == first['segment'] == 1
    time.sleep(.002)
    assert progress.sample(binding)['remaining_ms'] < first['remaining_ms']
    assert before == {str(p): p.read_bytes() for p in root.rglob('*') if p.is_file()}


@pytest.mark.parametrize('mutation', ['recipe', 'permit', 'config', 'owner', 'future', 'finished', 'exited', 'malformed', 'symlink', 'fifo', 'spent'])
def test_unavailable_or_changed_clock_omits_advice(tmp_path, mutation):
    root, spec, binding, config = fixture(tmp_path)
    common = start(root / 'clock')
    clock = root / 'clock'
    if mutation == 'recipe':
        (root / 'recipe.json').write_text('{}')
    elif mutation == 'permit':
        put(root / 'permit.json', dict(identity(os.getpid()), start_ticks=0))
    elif mutation == 'config':
        put(clock / 'config.json', dict(config, allowance_ms=20000))
    elif mutation == 'owner':
        put(clock / 'owner.json', dict(common, owner=dict(identity(os.getpid()), boot_id='old-boot')))
    elif mutation == 'future':
        p = clock / 'started.json'; value=json.loads(p.read_bytes()); value['monotonic_ns']=time.monotonic_ns()+10**12;put(p,value)
    elif mutation in ('finished', 'exited'):
        put(clock / (mutation + '.json'), common)
    elif mutation == 'malformed':
        put(clock / 'session.json', [])
    elif mutation == 'symlink':
        (clock / 'session.json').rename(clock / 'real.json')
        (clock / 'session.json').symlink_to(clock / 'real.json')
    elif mutation == 'fifo':
        (clock / 'session.json').unlink()
        os.mkfifo(clock / 'session.json')
    else:
        start(clock, ago_ms=20000)
    assert progress.sample(binding) is None


def test_native_transport_and_correction_already_include_previous_debits(tmp_path):
    session = dict(provider='codex', model='test', reasoning_effort='high', home=str(tmp_path/'home'), thread_id='thread')
    root, _, binding, _ = fixture(tmp_path, allowance=6613294, turn=2, session=session)
    start(root / 'clock', ago_ms=1000)
    sample = progress.sample(binding)
    assert 6612000 < sample['remaining_ms'] <= 6612294
    assert sample['turn'] == 2


def test_same_launcher_segment_uses_last_started_and_remaining(tmp_path):
    root, _, binding, config = fixture(tmp_path)
    clock = root / 'clock'
    common = start(clock, ago_ms=5000)
    finished = dict(common, disposition='interrupted', provider_budget='limit', elapsed_ms=1000, thread_id='thread')
    put(clock / 'finished.json', finished)
    segment = clock / 'segments/2'
    value = dict(config, allowance_ms=9000, binding=dict(config['binding'], thread_id='thread'),
        root_config_sha256=hashlib.sha256((clock/'config.json').read_bytes()).hexdigest(),
        previous_finished_sha256=hashlib.sha256(canonical_json(finished)).hexdigest())
    put(segment / 'config.json', value)
    start(segment, ago_ms=500)
    sample = progress.sample(binding)
    assert sample['segment'] == 2 and 8400 <= sample['remaining_ms'] <= 8500
    # A second debit or use of the original five-second start would be wrong.
    put(segment / 'config.json', dict(value, allowance_ms=10000))
    assert progress.sample(binding) is None


@pytest.mark.parametrize('kind', ['ordinary', 'review', 'stuck_math_audit'])
def test_nonclosure_has_no_descriptor(tmp_path, kind):
    assert fixture(tmp_path, task_kind=kind)[2] is None


def test_managed_delivery_dedup_and_wait_preserve_persistent_results(tmp_path, capsys):
    from trellis import check_job
    root, _, binding, _ = fixture(tmp_path)
    start(root / 'clock')
    repo = tmp_path/'repo'; (repo/'Tablet').mkdir(parents=True)
    (repo/'Tablet/N.lean').write_text('example : True := by trivial\n')
    runtime = tmp_path/'runtime'
    env = register_workspace(runtime, repo, scope='closure', provider_progress=binding)
    token = env['TRELLIS_CHECK_JOBS_TOKEN']
    service = JobService(runtime)
    request = dict(action='submit', token=token, request_id='first', kind='incremental', target='N', timeout=30)
    job = service.handle(request)
    assert 'provider_progress' not in job
    result = dict(returncode=1, stdout='file.lean:4: error: retained diagnostic\n', stderr='original stderr')
    service._finish(job['job_id'], 'failed', result)
    original = service.db.execute('SELECT result FROM jobs WHERE id=?',(job['job_id'],)).fetchone()['result']
    replies = [service.handle(dict(action='status', token=token, job_id=job['job_id'])),
               service.handle(request), service.handle(dict(request, request_id='dedup'))]
    assert len({r['job_id'] for r in replies}) == 1
    for reply in replies:
        assert reply['result']['stdout'] == result['stdout']
        assert reply['result']['stderr'].startswith('original stderr\nHost timing advisory:')
        assert check_job.render(check_job.wait(reply)) == 1
    output=capsys.readouterr()
    assert output.out.count('retained diagnostic') == 3
    assert output.err.count('Host timing advisory:') == 3
    assert service.db.execute('SELECT result FROM jobs WHERE id=?',(job['job_id'],)).fetchone()['result'] == original
    assert json.loads(original) == result


@pytest.mark.parametrize('padding_bytes', [0, 81 * 1024 * 1024])
def test_real_launcher_grant_delivers_optional_clock_binding(tmp_path, monkeypatch, padding_bytes):
    from trellis.dispatch import resources
    from trellis.dispatch.process import _load_recipe
    runtime = tmp_path/'runtime'
    private = runtime/'dispatch/1/runtime'
    repo = private.parent/'repo'
    (repo/'.trellis').mkdir(parents=True)
    private.mkdir()
    put(private/'runtime_metadata.json', dict(repo_path=str(repo)))
    root, _, _, _ = fixture(private.parent/'turns/1')
    spec = dict(runtime=str(runtime), task_runtime=str(private), compiler='managed',
                scope='1/turns/1/execution', lifetime=None)
    recipe=json.loads((root/'recipe.json').read_bytes());recipe['checker']=spec
    recipe['payload']['captured_context']='x' * padding_bytes
    put(root/'recipe.json',recipe)
    expected_hash=hashlib.sha256((root/'recipe.json').read_bytes()).hexdigest()
    loaded=_load_recipe(root)
    assert loaded[0] == recipe and loaded[1] == expected_hash
    original_read=progress._read
    def no_second_recipe_read(path, *args):
        assert Path(path) != root/'recipe.json'
        return original_read(path, *args)
    monkeypatch.setattr(progress, '_read', no_second_recipe_read)
    env=resources.grant(root,spec,loaded_recipe=loaded)
    key=hashlib.sha256(env['TRELLIS_CHECK_JOBS_TOKEN'].encode()).hexdigest()
    cap=json.loads((runtime/'checker-state/jobs/capabilities'/f'{key}.json').read_bytes())
    assert cap['provider_progress']['execution']==str(root)
    assert cap['provider_progress']['remaining_ms']==10000
    assert cap['provider_progress']['recipe_sha256']==expected_hash
    assert len(canonical_json(cap['provider_progress'])) < 2048
    start(root/'clock')
    assert progress.sample(cap['provider_progress'])['remaining_ms'] > 8000
    resources.revoke(root)
    assert not (runtime/'checker-state/jobs/capabilities'/f'{key}.json').exists()


@pytest.mark.parametrize('mutation', ['replace', 'rewrite', 'symlink', 'hardlink', 'failed_metadata'])
def test_loaded_recipe_binding_is_optional_and_rejects_changed_records(tmp_path, monkeypatch, mutation):
    from trellis.dispatch import process
    root, spec, _, _ = fixture(tmp_path)
    path=root/'recipe.json'
    original=path.read_bytes()
    if mutation in ('symlink', 'hardlink'):
        copy=root/'original.json';copy.write_bytes(original)
        path.unlink()
        if mutation == 'symlink':path.symlink_to(copy)
        else:os.link(copy,path)
    if mutation == 'failed_metadata':
        monkeypatch.setattr(process.os, 'fstat', lambda _: (_ for _ in ()).throw(OSError('metadata unavailable')))
    loaded=process._load_recipe(root)
    assert loaded[0] == json.loads(original)
    if mutation in ('replace', 'rewrite'):
        if mutation == 'replace':
            replacement=root/'replacement.json';replacement.write_bytes(original);replacement.replace(path)
        else:path.write_bytes(original+b'\n')
    assert progress.descriptor(root,spec,loaded_recipe=loaded) is None
    assert path.read_bytes() in (original, original+b'\n')


def test_recipe_changed_during_launcher_read_loses_only_optional_binding(tmp_path, monkeypatch):
    from trellis.dispatch.process import _load_recipe
    root, spec, _, _ = fixture(tmp_path)
    path=root/'recipe.json';original=path.read_bytes();real_open=Path.open
    class ChangingStream:
        def __enter__(self):
            self.stream=real_open(path,'rb');return self
        def __exit__(self,*args):self.stream.close()
        def fileno(self):return self.stream.fileno()
        def read(self):
            raw=self.stream.read()
            with real_open(path,'ab') as writer:writer.write(b'\n')
            return raw
    monkeypatch.setattr(Path,'open',lambda self,*args,**kwargs: ChangingStream()
        if self==path and args==('rb',) else real_open(self,*args,**kwargs))
    loaded=_load_recipe(root)
    assert loaded == (json.loads(original),None,None)
    assert progress.descriptor(root,spec,loaded_recipe=loaded) is None


@pytest.mark.parametrize('kind', ['build', 'incremental', 'scratch', 'file', 'local_closure'])
def test_empty_completion_preserved_and_metadata_unchanged(tmp_path, capsys, kind):
    from trellis import check_job
    root, _, binding, _ = fixture(tmp_path); start(root / 'clock')
    reply=dict(state='succeeded', result=dict(returncode=0))
    value=progress.decorate(reply, dict(provider_progress=binding), kind)
    assert check_job.render(value) == 0
    assert capsys.readouterr().err.startswith('check-job: succeeded\nHost timing advisory:')
    assert reply == dict(state='succeeded', result=dict(returncode=0))
    assert progress.decorate(reply, dict(provider_progress=binding), 'tool_info') is reply
    assert progress.decorate(reply, {}, kind) is reply

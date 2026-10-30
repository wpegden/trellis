"""A failed host attempt may donate successful leaves, never its verdict."""
import json
from pathlib import Path
import shutil
from types import SimpleNamespace

import pytest

from trellis.atomic_actions import observations as obs
from trellis.checker import statement_transform as transform
from trellis.checker.protocol import CheckerRequest
from trellis.checker.server import CheckerServer
from trellis.dispatch import acceptance_receipts as receipts, candidate, workspace
from trellis.dispatch.driver import acceptance_identity, ProductionDriver
from trellis.dispatch.publication import atomic
from test_dispatch_workspace import project, trusted_build


def put(path, value):
    atomic(path, candidate.canonical_json(value))


@pytest.fixture
def interrupted_host(trusted_build, monkeypatch, request):
    repo, runtime, config, authority = trusted_build
    monkeypatch.setattr(receipts, 'implementation_identity', lambda: 'new-host-implementation')
    d = runtime / 'dispatch/1/acceptance/1'
    private = {'request_seq': 7, 'in_flight_request': {'id': 7, 'cycle': 2}}
    sources = candidate.source_manifest(repo)
    sealed = runtime / 'dispatch/1/candidate'
    original_b = (repo / 'Tablet/B.lean').read_bytes()
    changed = getattr(request, 'param', False)
    if changed:
        (repo / 'Tablet/B.lean').write_bytes(original_b + b'-- candidate proof edit\n')
    identity = candidate.seal(repo, sealed, task_id=1, base_manifest=sources,
                              inputs=sources, response={'outcome': 'Valid'})
    (repo / 'Tablet/B.lean').write_bytes(original_b)
    task = dict(id=1, assignment={'scope': {'nodes': ['B']}, 'compiler': 'managed'}, binding={})
    source = dict(input_hashes=sources, manifest=sources, authority_inputs={'.trellis/scripts/check.py': candidate.digest((repo / '.trellis/scripts/check.py').read_bytes())}, graph={'edges': {'B': ['A'], 'A': ['Preamble']}})
    key = receipts.key(1, identity, task, acceptance_identity(private), source)
    key['implementation_identity'] = 'old-host-implementation'
    config_sha = candidate.digest(config.read_bytes())
    workspace.prepare(runtime, repo, config, d, private, expected_manifest=sources, seed_packages=False)
    put(d / 'canonical.json', dict(key=key, source=source, config_sha256=config_sha,
                                  acceptance_sha256=key['acceptance_sha256']))
    process = dict(pid=99999999, start_ticks=1, boot_id='gone-test-launcher')
    for name in ('process', 'permit', 'cleanup'):
        put(d / f'execution/{name}.json', process)
    put(d / 'execution/recipe.json', dict(command=['python3', '-m', 'trellis.dispatch.acceptance'],
        payload=dict(config_path=str(d / 'repo/trellis.config.json'), runtime_root=str(d / 'runtime'),
                     candidate=str(sealed), captured_inputs=sources),
        checker=dict(runtime=str(runtime), task_runtime=str(d / 'runtime'), compiler='managed',
                     scope='1/acceptance/1')))
    if changed:
        from trellis.dispatch.acceptance import overlay
        overlay(sealed, d / 'repo', sources)
        checked_repo = d / 'repo/.trellis/supervisor/repo'
        (checked_repo / 'Tablet/B.lean').write_bytes((d / 'repo/Tablet/B.lean').read_bytes())
        obs._tablet_olean_path(checked_repo, 'B').write_bytes(b'host compiled candidate B')
        obs.write_olean_srcclosure(checked_repo, 'B', obs.tablet_source_closure_hash(checked_repo, 'B'))
        assert obs._write_kernel_replay_attestation(checked_repo, 'B', olean_sha256='')
    request = CheckerRequest(op='lean_statement_transform', request_id=8, timeout_secs=1,
        raw={'input': {'name': 'Original', 'statement': 'theorem Original : True := by',
                       'context': 'import Tablet.B\n'}})
    calls = []
    def compiler(repo, args, **kw):
        calls.append(repo)
        return dict(returncode=0, stdout=json.dumps({'status':'ok', 'result': {'refutation_type':'¬ True'}}))
    monkeypatch.setattr(obs, '_run_lake_command', compiler)
    monkeypatch.setattr(obs, 'materialize_tablet_oleans', lambda *a, **kw: pytest.fail('coherent host seed should not rebuild'))
    server = CheckerServer(d / 'runtime', parallelism=1)
    with server._workspace_lock:
        response, rc, _ = transform.handle(server, request)
    assert rc == 0 and len(calls) == 1
    # The completed leaf survives a later genuine-shaped host interruption.
    atomic(d / 'execution/response.json', b'')
    result = dict(returncode=-15, response_sha256=candidate.digest(b''))
    put(d / 'execution/result.json', result)
    put(d / 'retry-authorized.json', dict(failed_result=result, resumed_at=1))
    # Canonical has no result cache; its old artifacts are deliberately cold.
    shutil.rmtree(authority)
    current = dict(key, acceptance_sha256='fresh-admin-context', implementation_identity='new-host-implementation')
    dest = d.with_name('2')
    def select(current_key=None, config_digest=None):
        return receipts.retry_cache_donor(dest, current_key or current, config_digest or config_sha, [d])
    return SimpleNamespace(repo=repo, runtime=runtime, config=config, source=source, task=task,
        identity=identity, private=private, directory=d, destination=dest, key=key, current=current,
        config_sha=config_sha, select=select, request=request, calls=calls, response=response)


def test_interrupted_host_seeds_coherent_artifacts_and_completed_leaf(interrupted_host):
    f = interrupted_host
    donor = f.select()
    assert donor == f.directory
    assert receipts.reusable(donor, f.current) is None
    before = (donor / 'execution/result.json').read_bytes()
    workspace.prepare(f.runtime, f.repo, f.config, f.destination, f.private,
        expected_manifest=f.source['manifest'], seed_packages=False, host_cache_donor=donor)
    marker = json.loads((f.destination / 'prepared.json').read_text())
    assert marker['seeded_oleans'] == 3 and marker['seeded_statement_transforms'] == 1
    assert marker['host_cache_donor_round'] == '1'
    private = CheckerServer(f.destination / 'runtime', parallelism=1)
    with private._workspace_lock:
        response, rc, telemetry = transform.handle(private, f.request)
    assert rc == 0 and response == f.response
    assert telemetry['statement_transform_cache_hit'] and len(f.calls) == 1
    assert (donor / 'execution/result.json').read_bytes() == before
    for name in ('A', 'B', 'Preamble'):
        old = obs._tablet_olean_path(CheckerServer(donor / 'runtime').supervisor_repo, name)
        new = obs._tablet_olean_path(private.supervisor_repo, name)
        assert new.read_bytes() == old.read_bytes() and new.stat().st_ino != old.stat().st_ino
    assert not (f.destination / 'completed.json').exists()
    assert not (f.destination / 'execution/response.json').exists()


@pytest.mark.parametrize('damage', ['source', 'toolchain', 'config', 'candidate', 'cleanup', 'permit',
                                     'retry', 'result', 'response', 'recipe', 'private_config', 'worker_recipe', 'authority'])
def test_retry_seed_rejects_mismatched_or_unfinished_host_attempt(interrupted_host, damage):
    f = interrupted_host
    current = dict(f.current)
    if damage in ('source', 'toolchain'):
        current['input_hashes'] = dict(current['input_hashes'], **{
            'Tablet/A.lean' if damage == 'source' else 'lean-toolchain': 'different'})
    elif damage == 'config':
        assert f.select(config_digest='different') is None
        return
    elif damage == 'authority': current['authority_input_hashes'] = {'.trellis/scripts/check.py': 'different'}
    elif damage == 'candidate': current['candidate_sha256'] = 'different'
    elif damage in ('cleanup', 'permit'): (f.directory / f'execution/{damage}.json').unlink()
    elif damage == 'retry': (f.directory / 'retry-authorized.json').unlink()
    elif damage == 'result': put(f.directory / 'execution/result.json', {'returncode':-15, 'response_sha256':'changed'})
    elif damage == 'response': atomic(f.directory / 'execution/response.json', b'changed')
    elif damage == 'private_config': (f.directory / 'repo/trellis.config.json').write_text('{}')
    else:
        p=f.directory / 'execution/recipe.json';r=json.loads(p.read_text())
        if damage == 'worker_recipe': r['command']=['python3','-m','trellis.runtime.bridge_cli']
        else: r['checker']['task_runtime']=str(f.directory / 'other-runtime')
        put(p,r)
    assert f.select(current_key=current) is None


@pytest.mark.parametrize('damage', ['bundle', 'replay', 'source'])
def test_retry_seed_never_retains_tampered_or_unreplayed_bundle(interrupted_host, monkeypatch, damage):
    f=interrupted_host;donor=f.select();old=CheckerServer(donor / 'runtime').supervisor_repo
    if damage == 'bundle': obs._tablet_olean_path(old,'A').write_bytes(b'changed')
    elif damage == 'replay':
        record=obs._read_olean_provenance(old,'A');record.pop('kernel_replay');obs._write_olean_provenance(old,'A',record)
    else: (f.repo/'Tablet/A.lean').write_text('changed source')
    workspace.prepare(f.runtime,f.repo,f.config,f.destination,f.private,
        expected_manifest=candidate.source_manifest(f.repo),seed_packages=False,host_cache_donor=donor)
    server=CheckerServer(f.destination/'runtime',parallelism=1)
    assert not obs._tablet_olean_path(server.supervisor_repo,'A').exists()
    assert not obs._tablet_olean_path(server.supervisor_repo,'B').exists()
    rebuilds=[]
    monkeypatch.setattr(obs,'materialize_tablet_oleans',lambda *a,**kw: (rebuilds.append(a) or
        dict(returncode=1,stderr='new replay required')))
    with server._workspace_lock: response,rc,_=transform.handle(server,f.request)
    assert rc == 1 and rebuilds and len(f.calls)==1


def test_failed_leaf_and_old_axiom_cache_are_not_donated(interrupted_host):
    f=interrupted_host
    cache=f.directory/'runtime/checker-state/statement-transforms'
    path=next(cache.glob('*.json'));record=json.loads(path.read_text());record['response']['status']='invalid_statement';put(path,record)
    put(f.directory/'runtime/checker-state/print-axioms'/(('a'*64)+'.json'), {'cache_version':2,'returncode':0})
    workspace.prepare(f.runtime,f.repo,f.config,f.destination,f.private,
        expected_manifest=f.source['manifest'],seed_packages=False,host_cache_donor=f.select())
    assert json.loads((f.destination/'prepared.json').read_text())['seeded_statement_transforms']==0
    assert not (f.destination/'runtime/checker-state/print-axioms').exists()


def _portable_inventory(monkeypatch, source, tmp_path):
    """Only the external Lean ModuleData adapter is faked; binding stays real."""
    from trellis.checker import portable_replay as portable
    sysroot = tmp_path / 'toolchain'
    (sysroot / 'bin').mkdir(parents=True)
    (sysroot / 'lib/lean').mkdir(parents=True)
    for name in ('lean', 'lake', 'leanchecker'):
        p=sysroot/'bin'/name;p.write_bytes(name.encode());p.chmod(0o755)
    (sysroot/'lib/lean/Init.olean').write_bytes(b'pinned Init')
    def inventory(repo, args, **kwargs):
        assert '--replay-environment' in args
        root=repo/'.lake/build/lib/lean'
        paths={**{'Tablet.'+n:root/'Tablet'/(n+'.olean') for n in ('A','B','Preamble')},
               'Init':sysroot/'lib/lean/Init.olean'}
        deps={'Tablet.B':['Tablet.A'],'Tablet.A':['Tablet.Preamble'],'Tablet.Preamble':['Init']}
        rows=[dict(environment_header=True,sysroot=str(sysroot),path=str(sysroot/'bin'),
                   search_path=[str(root),str(sysroot/'lib/lean')])]
        for name,path in paths.items():
            if name=='Init': rows.append(dict(module=name,sysroot_import=str(path)))
            else: rows.append(dict(module=name,parts=[dict(level='exported',path=str(path))],
                imports=[dict(module=dep,olean=str(paths[dep]),import_all=False,is_exported=True,is_meta=False) for dep in deps[name]]))
        return dict(returncode=0,stdout='\n'.join(json.dumps(row) for row in rows),stderr='')
    monkeypatch.setattr(obs,'_run_lake_command',inventory)
    env,stats=portable.capture(source,['B'],timeout_secs=60,bwrap_role='lake_compiler')
    assert env is not None,stats
    portable.attach(source,['A','B','Preamble'],env)
    return portable


@pytest.mark.parametrize('interrupted_host', [True], indirect=True)
def test_candidate_edits_reuse_host_artifacts_only_after_overlay_then_normal_leaf_check(interrupted_host, monkeypatch, tmp_path):
    from trellis.dispatch import replay_donation as donation
    from trellis.dispatch.acceptance import overlay
    f=interrupted_host; donor=f.select();source=CheckerServer(donor/'runtime').supervisor_repo
    portable=_portable_inventory(monkeypatch,source,tmp_path)
    workspace.prepare(f.runtime,f.repo,f.config,f.destination,f.private,
        expected_manifest=f.source['manifest'],seed_packages=False,host_cache_donor=donor)
    dest=f.destination/'repo';authority=dest/'.trellis/supervisor/repo'
    # Changed candidate proof cannot donate to the canonical baseline.
    assert not obs._tablet_olean_path(authority,'B').exists()
    put(f.destination/'canonical.json',dict(key=f.current,source=f.source,config_sha256=f.config_sha))
    receipts.seed_retry_replay(f.destination,f.current,donor)
    previous,marker,completed=donation._marker(f.destination/'runtime')
    assert previous==donor and marker['donor_kind']=='drained_host_execution' and completed is None
    old_result=(donor/'execution/result.json').read_bytes()
    overlay(f.runtime/'dispatch/1/candidate',dest,f.source['input_hashes'])
    # The ordinary supervisor source sync precedes the same checker-owned
    # installer in acceptance._check. Reproduce its exact source state here.
    (authority/'Tablet/B.lean').write_bytes((dest/'Tablet/B.lean').read_bytes())
    invalidated=[]
    server=SimpleNamespace(runtime_root=f.destination/'runtime',supervisor_repo=authority,
        _invalidate_compile_cache_for_sync=lambda names:invalidated.extend(names))
    result=donation.install(server)
    assert result['status']=='installed' and result['covered_nodes']==['A','B','Preamble'],result
    assert obs._tablet_olean_path(authority,'B').read_bytes()==b'host compiled candidate B'
    assert donation.validation_fact_donor(f.destination/'runtime',result) is None
    assert (donor/'execution/result.json').read_bytes()==old_result
    # Still run the current derivation consumer with the real helper/input/
    # replay key. No old WorkerResponse or validation step is returned.
    current=CheckerServer(f.destination/'runtime',parallelism=1)
    with current._workspace_lock:
        response,rc,stats=transform.handle(current,f.request)
    assert rc==0 and response==f.response and stats['statement_transform_cache_hit']
    assert len(f.calls)==1
    assert not (f.destination/'completed.json').exists()
    assert not (f.destination/'execution/response.json').exists()


@pytest.mark.parametrize('damage',['cleanup','result','retry','candidate','current_key','kind'])
def test_drained_marker_revalidates_exact_host_terminal_evidence_at_install(interrupted_host,damage):
    from trellis.dispatch import replay_donation as donation
    f=interrupted_host
    workspace.prepare(f.runtime,f.repo,f.config,f.destination,f.private,
        expected_manifest=f.source['manifest'],seed_packages=False,host_cache_donor=f.select())
    put(f.destination/'canonical.json',dict(key=f.current,source=f.source,config_sha256=f.config_sha))
    receipts.seed_retry_replay(f.destination,f.current,f.directory)
    marker=f.destination/'replay-cache-donor.json'
    assert donation._marker(f.destination/'runtime')[2] is None
    if damage in ('cleanup','result'):(f.directory/f'execution/{damage}.json').unlink()
    elif damage=='retry':(f.directory/'retry-authorized.json').unlink()
    elif damage=='candidate':(f.runtime/'dispatch/1/candidate/sha256').write_text('different')
    else:
        value=json.loads(marker.read_text());value['current_key_sha256' if damage=='current_key' else 'donor_kind']='other';put(marker,value)
    with pytest.raises((OSError,ValueError,candidate.CandidateError)):
        donation._marker(f.destination/'runtime')

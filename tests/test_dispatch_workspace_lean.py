"""Real trusted-cache reuse, dependency invalidation, and invalid-proof rejection."""
import json, os, subprocess, threading, time, tempfile
import pytest
from trellis.host_runtime import worker_elan_home
from pathlib import Path
from trellis.checking import write_scripts
from trellis.runtime.bridge import _mint_burst_token, _register_burst_token
from trellis.checker.server import CheckerServer
from trellis.checker import admission
from trellis.atomic_actions import observations as obs
from trellis.atomic_actions.checker_client import client_materialize_tablet_oleans
from trellis.checker.workspaces import register, revoke
from trellis.dispatch.workspace import prepare
from trellis.dispatch.candidate import source_manifest
from trellis.supervisor_workspace import sync_supervisor_workspace



@pytest.mark.parametrize('mode', ['direct', 'shared', 'host'])
def test_real_seed_reuses_replay_and_invalidates_changed_dependencies(monkeypatch, mode):
    shared_cache, host_only = mode == 'shared', mode == 'host'
    if not (worker_elan_home()/'toolchains/leanprover--lean4---v4.30.0-rc2/bin/lean').is_file():
        pytest.skip('requires the pinned Lean toolchain')
    # Keep the central-checker Unix socket below Linux's pathname limit.
    monkeypatch.setattr(admission, 'ACTIVE', admission.ACTIVE)
    scratch=Path(__file__).resolve().parents[1]/'.work'
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='seed-', dir=scratch) as temporary:
        base=Path(temporary)
        repo=base/'repo'; rt=base/'run'; (repo/'Tablet').mkdir(parents=True); rt.mkdir()
        config=json.loads((Path(__file__).resolve().parents[1]/'examples/trellis.config.json').read_text());config['repo_path']=str(repo)
        (repo/'trellis.config.json').write_text(json.dumps(config))
        (repo/'lean-toolchain').write_text('leanprover/lean4:v4.30.0-rc2\n')
        (repo/'lakefile.toml').write_text('name = "replay_test"\nversion = "0.1.0"\n[[lean_lib]]\nname = "Tablet"\n')
        (repo/'lake-manifest.json').write_text('{"version":"1.2.0","packagesDir":".lake/packages","packages":[],"name":"replay_test","lakeDir":".lake"}')
        (repo/'Tablet/Preamble.lean').write_text('def seedZero : Nat := 0\n')
        (repo/'Tablet/A.lean').write_text('import Tablet.Preamble\ntheorem A : seedZero = 0 := by rfl\n')
        (repo/'Tablet/B.lean').write_text('import Tablet.A\ntheorem B : seedZero = 0 := A\n')
        (repo/'Tablet/C.lean').write_text('import Tablet.Preamble\ntheorem C : seedZero = 0 := by rfl\n')
        for args in [('init',),('config','user.name','Test'),('config','user.email','test@example.invalid'),
                     ('add','Tablet','trellis.config.json','lean-toolchain','lakefile.toml','lake-manifest.json'),('commit','-m','fixture')]:
            subprocess.run(['git','-C',str(repo),*args],check=True,capture_output=True)
        write_scripts(repo,repo/'.trellis')
        (rt/'runtime_metadata.json').write_text(json.dumps(dict(repo_path=str(repo),config_path=str(repo/'trellis.config.json'))))
        token=_mint_burst_token();_register_burst_token(rt,token=token,burst_id='seed-smoke',kind='supervisor',request_id=0,cycle=0)
        for key,value in dict(TRELLIS_KERNEL_CACHE_ROOT=str(rt),TRELLIS_CHECKER_TOKEN=token,
            TRELLIS_CHECK_JOBS='1',TRELLIS_CHECK_JOBS_PARALLELISM='1',LEAN_NUM_THREADS='1').items():
            monkeypatch.setenv(key,value)
        calls=[];original=obs._run_lake_command
        def run(repo,args,**kwargs):
            calls.append(dict(repo=str(repo),args=list(args)));return original(repo,args,**kwargs)
        monkeypatch.setattr(obs,'_run_lake_command',run)
        server=CheckerServer(rt);server.start();thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        results=[]; private_token=None
        def check(target):
            sync_supervisor_workspace(target)
            start=time.monotonic(); result=client_materialize_tablet_oleans(rt/'sockets/checker.sock',target,['A','B','C','Preamble'],timeout_secs=120)
            results.append(dict(seconds=time.monotonic()-start,result=result));return result
        try:
            first=check(repo);assert first['returncode']==0,json.dumps(first,indent=2)
            # Actual first advisory mirror receives the current Tablet cache,
            # independently of the package cache and authoritative workspace.
            from trellis.checker.job_workspace import capture, prepare_mirror, compiler_command
            mirror = base / 'advisory/repo'
            prepare_mirror(repo, mirror, capture(repo)[1])
            first_advisory = subprocess.run(compiler_command(mirror, ['lake', 'build', 'Tablet.B']),
                cwd=mirror, capture_output=True, text=True, timeout=120)
            assert first_advisory.returncode == 0, first_advisory.stdout + first_advisory.stderr
            assert 'Built Tablet.' not in first_advisory.stdout + first_advisory.stderr
            assert json.loads((mirror.parent / 'tablet-build-seed.json').read_text())['validated_nodes'] == 4
            target=rt/'dispatch/1'
            deferred = {}
            if shared_cache:
                from trellis.dispatch import batch_cache
                donor = rt/'dispatch/batch-caches/fixture'
                receipt = batch_cache.capture(donor, repo, {'fixture': 'real-trusted-cache'})
                deferred['deferred_build_cache'] = batch_cache.reference(donor, rt/'dispatch', receipt)
            prepare(rt,repo,repo/'trellis.config.json',target,{'in_flight_request':{'id':1}},
                    expected_manifest=source_manifest(repo),seed_packages=False, host_only=host_only, **deferred)
            if shared_cache:
                assert json.loads((target/'prepared.json').read_text())['seeded_oleans'] == 0
                assert not (target/'repo/.lake/build').exists()
                batch_cache.hydrate(target, rt/'dispatch', None)
                assert json.loads((target/'build-cache-ready.json').read_text())['seeded_oleans'] == 4
            else:
                assert json.loads((target/'prepared.json').read_text())['seeded_oleans']==4
            if host_only:
                assert not (target/'repo/.lake/build').exists()
                # Owned-stage restart and a new finalizer retain the same trusted
                # donor semantics despite never materializing the outer cache.
                marker = (target/'prepared.json').read_bytes()
                prepare(rt,repo,repo/'trellis.config.json',target,{'in_flight_request':{'id':1}},
                    expected_manifest=source_manifest(repo),seed_packages=False,host_only=True)
                assert (target/'prepared.json').read_bytes() == marker
                finalizer = target/'acceptance/1/finalization/1'
                prepare(rt,repo,repo/'trellis.config.json',finalizer,{'in_flight_request':{'id':2}},
                    expected_manifest=source_manifest(repo),seed_packages=False,host_only=True,
                    host_cache_donor=target)
                assert json.loads((finalizer/'prepared.json').read_text())['seeded_oleans'] == 4
                assert not (finalizer/'repo/.lake/build').exists()
                target = finalizer
            private=target/'repo';authority=private/'.trellis/supervisor/repo'
            assert all(obs.olean_is_content_current(authority,n) for n in ['A','B','C','Preamble'])
            assert all(obs.olean_has_current_kernel_replay(authority,n) for n in ['A','B','C','Preamble'])
            before_calls=len(calls)
            channel=register(rt,target/'runtime',lifetime=300);private_token=channel['TRELLIS_CHECKER_TOKEN']
            for key,value in channel.items():monkeypatch.setenv(key,value)
            seeded=check(private);assert seeded['returncode']==0,seeded
            assert seeded.get('stale_node_count')==0,seeded
            assert ['env','leanchecker','Tablet'] not in [c['args'] for c in calls[before_calls:]],calls
            assert not seeded['kernel_replay']['checked_nodes'],seeded
            assert all(obs.olean_has_current_kernel_replay(authority,n) for n in ['A','B','C','Preamble'])
            assert 'Built Tablet.' not in seeded.get('stdout',''),seeded
            warm=check(private);assert warm['returncode']==0,warm
            if host_only:
                from trellis.supervisor_workspace import propagate_tablet_back_to_worker
                propagate_tablet_back_to_worker(authority, private, mirror_build=False)
                assert not (private/'.lake/build').exists()
            preserved = obs._tablet_olean_path(authority,'C').read_bytes()
            (private/'Tablet/A.lean').write_text('import Tablet.Preamble\ntheorem A : seedZero = 0 := by rfl\n-- independent audit changed input\n')
            changed=check(private)
            assert changed['returncode']==0,changed
            replay=changed.get('kernel_replay',{})
            assert set(replay['checked_nodes']) == {'A','B'},replay
            assert obs._tablet_olean_path(authority,'C').read_bytes()==preserved
            (private/'Tablet/A.lean').write_text('import Tablet.Preamble\ntheorem A : seedZero = 1 := by rfl\n')
            invalid=check(private)
            assert invalid['returncode']!=0,invalid
            assert 'rfl' in invalid.get('stdout','') + invalid.get('stderr',''),invalid
            assert not obs.olean_is_content_current(authority,'A')
            assert not obs.olean_is_content_current(authority,'B')
            assert obs.olean_is_content_current(authority,'C')
            assert obs._tablet_olean_path(authority,'C').read_bytes()==preserved
            if host_only:
                cold = rt/'dispatch/2/acceptance/1'
                prepare(rt,repo,repo/'trellis.config.json',cold,{'in_flight_request':{'id':3}},
                    expected_manifest=source_manifest(repo),seed_packages=False,host_only=True,
                    host_cache_donor=rt/'absent-donor')
                assert json.loads((cold/'prepared.json').read_text())['seeded_oleans'] == 0
                assert not (cold/'repo/.lake/build').exists()
                revoke(rt,private_token)
                channel=register(rt,cold/'runtime',lifetime=300)
                private_token=channel['TRELLIS_CHECKER_TOKEN']
                for key,value in channel.items():monkeypatch.setenv(key,value)
                rebuilt=check(cold/'repo')
                assert rebuilt['returncode']==0,rebuilt
                assert set(rebuilt['kernel_replay']['checked_nodes']) == {'A','B','C','Preamble'}
                assert not (cold/'repo/.lake/build').exists()
            print('PASS: copied replay cache is warm; editing A rebuilds/replays A+B only; invalid A cannot reuse A or B; independent C remains current.',flush=True)
        finally:
            if private_token: revoke(rt,private_token)
            server.shutdown();thread.join(timeout=5)
            assert not thread.is_alive()
            print('Durations:',[round(x['seconds'],3) for x in results],flush=True)

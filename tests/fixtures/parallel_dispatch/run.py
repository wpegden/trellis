"""Opt-in production integration fixture, with only provider answers simulated.

Run from the source root with PYTHONPATH=., a matching prebuilt
TRELLIS_TRELLIS_KERNEL_CMD outside /tmp. Pass a fresh directory
under .work (short enough for Linux UNIX socket paths). Evidence is retained.
"""
import asyncio
import copy
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

from trellis.checking import write_scripts
from trellis.dispatch.__main__ import ensure_checker
from trellis.dispatch.config import DEFAULTS
from trellis.dispatch.coordination import Coordinator
from trellis.dispatch.driver import ProductionDriver
from trellis.dispatch.process import AttemptProcess
from trellis.runtime.bridge import _mint_burst_token, _register_burst_token
from trellis.runtime.kernel_cli import run_kernel_cli

base = Path(sys.argv[1]).resolve()
kernel_command = shlex.split(os.environ['TRELLIS_TRELLIS_KERNEL_CMD'])
if len(kernel_command) == 1 and Path(kernel_command[0]).resolve().is_relative_to('/tmp'):
    raise RuntimeError('fixture kernel binary must be outside /tmp; agent sandboxes use a private /tmp')
resuming = '--resume' in sys.argv[2:]
restart_test = '--restart' in sys.argv[2:]
fact_reuse_test = '--acceptance-fact-reuse' in sys.argv[2:]
ordinary_refresh = ('helpers' if '--ordinary-refresh-helpers' in sys.argv[2:]
                    else 'delete' if '--ordinary-refresh-delete' in sys.argv[2:] else None)
staged_test = ('bad-consumer' if '--staged-bad-consumer' in sys.argv[2:]
               else 'helpers' if '--staged-helpers' in sys.argv[2:]
               else 'shared' if '--staged-shared-consumer' in sys.argv[2:] else None)
shared_root = ordinary_refresh or staged_test
worker_changes = ordinary_refresh or (staged_test if staged_test in ('helpers', 'bad-consumer') else None)
publication_refresh_test = '--publication-refresh' in sys.argv[2:] or bool(ordinary_refresh)
probe_reuse_test = '--acceptance-probe-reuse' in sys.argv[2:] or fact_reuse_test or publication_refresh_test
concurrent_acceptance_test = '--concurrent-acceptance' in sys.argv[2:] or probe_reuse_test
semantic_reservations_test = '--semantic-reservations' in sys.argv[2:]
dependent_acceptance_test = '--dependent-acceptance' in sys.argv[2:]
concurrent_acceptance_test = concurrent_acceptance_test or semantic_reservations_test or bool(staged_test)
retry_test = '--retry' in sys.argv[2:]
migration_test = '--migrate-sidecar' in sys.argv[2:]
coarse_test = '--coarse' in sys.argv[2:]
submission_test = '--worker-submission' in sys.argv[2:]
scoped_test = '--scoped-audit' in sys.argv[2:]
shadow_imports_test = '--shadow-imports' in sys.argv[2:]
imports_test = '--add-imports' in sys.argv[2:] or shadow_imports_test
assert not scoped_test or coarse_test
os.environ['TRELLIS_MANAGED_AGENT_CHECKS'] = '1'
os.environ.pop('TRELLIS_COMPILER_BWRAP', None)
os.environ.pop('TRELLIS_COMPILER_DATA_VIEW', None)
if base.exists() and not resuming:
    raise RuntimeError('use a fresh fixture directory; previous evidence is retained')
repo = base / 'repo'
os.environ['TRELLIS_RUNTIME_CHECKPOINT_HOOK'] = str(Path('scripts/trellis_git_checkpoint_hook.sh').resolve())
if not resuming:
    repo.joinpath('Tablet').mkdir(parents=True)
    os.environ['TRELLIS_RUNTIME_CHECKPOINT_HOOK'] = str(Path('scripts/trellis_git_checkpoint_hook.sh').resolve())
    config = json.loads(Path('examples/trellis.config.json').read_text())
    config.update(repo_path=str(repo), parallel_dispatch={'enabled': True})
    config['workflow'].update(start_phase='proof_formalization', paper_tex_path='paper.tex', main_result_labels=['t'],
        main_result_targets=[dict(start_line=1, end_line=3, tex_label='t')])
    if submission_test:
        config['parallel_dispatch']['worker_submission_policy'] = dict(version=1, max_submissions=3, max_infrastructure_retries=2)
    config['git']['auto_push'] = False
    (repo / 'trellis.config.json').write_text(json.dumps(config))
    (repo / 'trellis.policy.json').write_bytes(Path('examples/trellis.policy.json').read_bytes())
    (repo / 'paper.tex').write_text('\\begin{theorem}\\label{t}\nTrue.\n\\end{theorem}\n')
    (repo / 'GOAL.md').write_text('Prove A and B.\n')
    (repo / 'lean-toolchain').write_text('leanprover/lean4:v4.33.0\n')
    (repo / 'lakefile.toml').write_text('name = "dispatch_test"\nversion = "0.1.0"\n[[lean_lib]]\nname = "Tablet"\n')
    (repo / 'lake-manifest.json').write_text('{"version":"1.2.0","packagesDir":".lake/packages","packages":[],"name":"dispatch_test","lakeDir":".lake"}')
    (repo / 'Tablet/Preamble.lean').write_text('-- [TABLET NODE: Preamble]\ndef fixtureZero : Nat := 0\n')
    (repo / 'Tablet/Preamble.tex').write_text('')
    (repo / 'Tablet/H.lean').write_text('import Tablet.Preamble\n\n-- [TABLET NODE: H]\ntheorem H : fixtureZero = 0 := by rfl\n')
    (repo / 'Tablet/H.tex').write_text('\\begin{theorem}The shared constant is zero.\\end{theorem}\n\\begin{proof}By definition.\\end{proof}\n')
    if shadow_imports_test:
        with (repo / 'Tablet/H.lean').open('a') as stream:
            stream.write('notation "False" => True\n')
    for node in ('A', 'B'):
        statement = 'False' if shadow_imports_test else 'True'
        support = 'Preamble' if imports_test else 'H'
        (repo / f'Tablet/{node}.lean').write_text(f'import Tablet.{support}\n\n-- [TABLET NODE: {node}]\ntheorem {node} : {statement} := by\n-- BODY\n  sorry\n')
        (repo / f'Tablet/{node}.tex').write_text('\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n')
    (repo / 'Tablet.lean').write_text('import Tablet.A\nimport Tablet.B\n')
    if dependent_acceptance_test:
        path = repo / 'Tablet/B.lean'
        path.write_text(path.read_text().replace('import Tablet.H', 'import Tablet.A'))
    if shared_root:
        (repo / 'Tablet/Root.lean').write_text('import Tablet.A\nimport Tablet.B\n\n-- [TABLET NODE: Root]\ntheorem Root : True := by\n-- BODY\n  exact (fun (_ : True) => A) B\n')
        (repo / 'Tablet/Root.tex').write_text('\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n')
        (repo / 'Tablet.lean').write_text('import Tablet.Root\n')
    if staged_test == 'bad-consumer':
        # D is a separate authorized Definition owner. Its body can change
        # under ordinary restructure while B's own proof remains valid.
        (repo / 'Tablet/D.lean').write_text('import Tablet.H\n\n-- [TABLET NODE: D]\ndef D : Nat :=\n-- BODY\n  0\n')
        (repo / 'Tablet/D.tex').write_text('\\begin{definition}The fixture constant is a natural number.\\end{definition}\n')
        path = repo / 'Tablet/B.lean'
        path.write_text('import Tablet.D\n' + path.read_text())
        path = repo / 'Tablet/Root.lean'
        path.write_text('import Tablet.D\n' + path.read_text().replace('  exact (fun',
            '  have zero : D = 0 := by rfl\n  exact (fun'))
    if ordinary_refresh == 'delete':
        (repo / 'Tablet/D.lean').write_text('import Tablet.H\n\n-- [TABLET NODE: D]\ntheorem D : True := by trivial\n')
        (repo / 'Tablet/D.tex').write_text('\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n')
        p = repo / 'Tablet/B.lean'
        p.write_text('import Tablet.D\n' + p.read_text())
    (repo / '.gitignore').write_text('.lake/\n.trellis/\n')
    write_scripts(repo, repo / '.trellis')
    for args in [('init', '-q'), ('config', 'user.name', 'Test'), ('config', 'user.email', 'test@example.invalid'),
                 ('add', '.'), ('commit', '-qm', 'fixture')]:
        subprocess.run(['git', '-C', str(repo), *args], check=True)

    live = dict(present_nodes=['A', 'B', 'H', 'Preamble'], open_nodes=['A', 'B'], coverage={'t': ['A']},
        paper_current_fingerprints={'t': 'A=ta'}, target_fingerprints={'A': 'ta'},
        corr_current_fingerprints={'A': 'ca', 'B': 'cb', 'H': 'ch', 'Preamble': 'cp'},
        sound_current_fingerprints={'A': 'sa', 'B': 'sb', 'H': 'sh'}, substantiveness_current_fingerprints={'A': 'sub-A', 'B': 'sub-B', 'H': 'sub-H'})
    # Synthetic judgments belong only to this toy fixture. Closure evidence is
    # issued by the real kernel and central checker, never fabricated.
    state = dict(sound_assessment_schema_version=1, phase='TheoremStating', stage='Reviewer', cycle=0,
        active_node='A', configured_targets=['t'], proof_nodes=['A', 'B', 'H'],
        node_kinds={'A': 'Proof', 'B': 'Proof', 'H': 'Proof', 'Preamble': 'Preamble'}, target_claims={'A': ['t']},
        live=live, committed=live, committed_proof_nodes=['A', 'B', 'H'], committed_target_claims={'A': ['t']},
        corr_status={n: 'Pass' for n in live['present_nodes']}, corr_approved_fingerprints=live['corr_current_fingerprints'],
        sound_status={'A': 'Pass', 'B': 'Pass', 'H': 'Pass'}, sound_approved_fingerprints=live['sound_current_fingerprints'],
        substantiveness_status={'A': 'Pass', 'B': 'Pass', 'H': 'Pass'},
        substantiveness_approved_fingerprints=live['substantiveness_current_fingerprints'],
        paper_status={'t': 'Pass'}, paper_approved_fingerprints=live['paper_current_fingerprints'])
    if semantic_reservations_test or dependent_acceptance_test:
        # A/B share an immutable imported proof and are independent of the
        # approved target H. The dependent variant additionally imports A in B.
        # These are real declaration/import/target shapes, not scheduler mocks.
        live.update(coverage={'t': ['H']}, target_fingerprints={'H': 'th'},
                    paper_current_fingerprints={'t': 'H=th'})
        state.update(target_claims={'H': ['t']}, committed_target_claims={'H': ['t']},
                     paper_approved_fingerprints=live['paper_current_fingerprints'],
                     deps={'A': ['H'], 'B': ['A' if dependent_acceptance_test else 'H'], 'H': ['Preamble']})
        state['committed_deps'] = copy.deepcopy(state['deps'])
    if shared_root:
        # A and B are independent unclaimed branches of an actual paper root.
        # Complete graph observations make B's helpers reachable and give a
        # deleted imported helper its real restructure authorization envelope.
        live['present_nodes'].append('Root')
        live.update(coverage={'t': ['Root']}, paper_current_fingerprints={'t': 'Root=tr'},
                    target_fingerprints={'Root': 'tr'})
        state.update(target_claims={'Root': ['t']}, committed_target_claims={'Root': ['t']},
                     paper_approved_fingerprints=live['paper_current_fingerprints'],
                     deps={'Root': ['A', 'B'], 'A': ['H'], 'B': ['H'], 'H': ['Preamble']})
        state['proof_nodes'].append('Root')
        state['committed_proof_nodes'].append('Root')
        state['node_kinds']['Root'] = 'Proof'
        for kind, prefix in [('corr', 'c'), ('sound', 's'), ('substantiveness', 'sub-')]:
            state[kind + '_status']['Root'] = 'Pass'
            live[kind + '_current_fingerprints']['Root'] = prefix + 'r'
    if staged_test == 'bad-consumer':
        # Keep Root an actual consumer, but not an approved paper guard. This
        # reaches final scoped observation rather than an earlier guard veto.
        live.update(coverage={'t': ['H']}, paper_current_fingerprints={'t': 'H=th'},
                    target_fingerprints={'H': 'th'})
        state.update(target_claims={'H': ['t']}, committed_target_claims={'H': ['t']},
                     paper_approved_fingerprints=live['paper_current_fingerprints'])
    if staged_test == 'bad-consumer':
        live['present_nodes'].append('D')
        state['node_kinds']['D'] = 'Definition'
        state['deps']['B'].append('D')
        state['deps']['Root'].append('D')
        state['deps']['D'] = ['H']
        for kind, prefix in [('corr', 'c'), ('sound', 's'), ('substantiveness', 'sub-')]:
            state[kind + '_status']['D'] = 'Pass'
            live[kind + '_current_fingerprints']['D'] = prefix + 'd'
    if ordinary_refresh == 'delete':
        live['present_nodes'].append('D')
        state['proof_nodes'].append('D')
        state['committed_proof_nodes'].append('D')
        state['node_kinds']['D'] = 'Proof'
        state['deps']['B'].append('D')
        state['deps']['D'] = ['H']
        for kind, prefix in [('corr', 'c'), ('sound', 's'), ('substantiveness', 'sub-')]:
            state[kind + '_status']['D'] = 'Pass'
            live[kind + '_current_fingerprints']['D'] = prefix + 'd'
    if shared_root:
        state['committed_deps'] = copy.deepcopy(state['deps'])


    def initialize(runtime, state, *, migrate=False):
        run_kernel_cli(dict(action='init', root=str(runtime), state=state,
            metadata=dict(repo_path=str(repo), config_path=str(repo / 'trellis.config.json'))))
        token = _mint_burst_token()
        _register_burst_token(runtime, token=token, burst_id='fixture', kind='supervisor', request_id=0, cycle=state['cycle'])
        os.environ.update(TRELLIS_CHECKER_TOKEN=token, TRELLIS_CHECKER_SOCKET=str(runtime / 'sockets/checker.sock'))
        ensure_checker(runtime, 2)
        command = dict(action='dispatch_enable', root=str(runtime), config=dict(DEFAULTS, **({
            'worker_submission_policy': config['parallel_dispatch']['worker_submission_policy']} if submission_test else {})))
        if migrate:
            spool = runtime / 'sidecar/spool/outcomes'
            spool.mkdir(parents=True, exist_ok=True)
            outcome = spool / 'outcome-final.json'
            outcome.write_text(json.dumps(dict(schema=1, attempt_id='final', node='A', entry_seq=1,
                status='failed', detail='completed legacy attempt', export_cycle=5)))
            slots = runtime / 'sidecar/slots.json'
            slots.write_text('{"slots":[{}]}')
            before = (runtime / 'protocol_state.json').read_bytes()
            try:
                run_kernel_cli(command)
            except RuntimeError as exc:
                assert 'slot journal is not empty' in str(exc), exc
            else:
                raise AssertionError('migration admitted an unreaped sidecar slot')
            assert outcome.exists() and (runtime / 'protocol_state.json').read_bytes() == before
            slots.write_text('{"slots":[]}')
        result = run_kernel_cli(command)
        if migrate:
            actual = json.loads((runtime / 'protocol_state.json').read_text())
            assert not outcome.exists() and not actual.get('sidecar_queue')
            assert actual['stage'] == 'Start' and actual.get('in_flight_request') is None
            assert (runtime / 'sidecar/spool/outcomes_consumed/outcome-final.json').is_file()
            print('reconciled final sidecar outcome without issuing a serial request', flush=True)
        return result


    if submission_test:
        state['worker_submission_policy'] = dict(version=1, max_submissions=3, max_infrastructure_retries=2)
    genesis = base / 'genesis'
    initialize(genesis, state)
    state = json.loads((genesis / 'protocol_state.json').read_text())
    assert state['cycle'] == 1 and len(state['local_closure_records']) == 4 + int(bool(shared_root)) + int(ordinary_refresh == 'delete' or staged_test == 'bad-consumer')
    uses = state['local_closure_records']['H']['node_certificate_evidence']['exact_declaration_uses']
    assert any(use['owner'] == 'Preamble' for use in uses), 'fixture needs a real closure dependency'
    if semantic_reservations_test or dependent_acceptance_test or staged_test:
        # Start from actual source observations. Synthetic placeholder hashes
        # would make the first closure appear to repair the entire tablet.
        observed = run_kernel_cli(dict(action='hydrate_worker_response', input=dict(
            repo_path=str(repo), configured_targets=state['configured_targets'],
            current_target_claims=state['target_claims'], approved_paper_fingerprints={},
            paper_source_path=str(repo / 'paper.tex'), current_node_kinds=state['node_kinds'],
            response=dict(snapshot=state['live']))))['output']['response']['snapshot']
        state.update(live=observed, committed=copy.deepcopy(observed))
        for kind in ('corr', 'sound', 'substantiveness', 'paper'):
            state[kind + '_approved_fingerprints'] = copy.deepcopy(observed[kind + '_current_fingerprints'])
    state.update(phase='ProofFormalization', cycle=5, stage='Reviewer', dispatch=None, request_seq=0,
                 in_flight_request=None, stuck_math_audit={})
    if migration_test:
        config['sidecar'] = dict(enabled=True, auto_dispatch=dict(enabled=False))
        (repo / 'trellis.config.json').write_text(json.dumps(config))
        state.update(stage='Start', sidecar_queue_seq=1,
            sidecar_queue=[dict(node='A', entry_seq=1, queued_at_cycle=5)])
    if coarse_test or submission_test or worker_changes:
        state.update(coarse_dag_nodes=['A', 'B'], active_coarse_node='A', coarse_goals=None)
    runtime = base / 'run'
    initialize(runtime, state, migrate=migration_test)
    if migration_test:
        config['sidecar']['enabled'] = False
        (repo / 'trellis.config.json').write_text(json.dumps(config))
else:
    runtime = base / 'run'
    write_scripts(repo, repo / '.trellis')
    token = _mint_burst_token()
    _register_burst_token(runtime, token=token, burst_id='fixture-resume', kind='supervisor', request_id=0, cycle=1)
    os.environ.update(TRELLIS_CHECKER_TOKEN=token, TRELLIS_CHECKER_SOCKET=str(runtime / 'sockets/checker.sock'))
    ensure_checker(runtime, 2)


class FixtureProcess(AttemptProcess):
    def start_or_adopt(self, command, payload, **kwargs):
        if command[-1] == 'trellis.runtime.bridge_cli':
            command = [sys.executable, str(Path('tests/fixtures/parallel_dispatch/provider.py').resolve())]
            payload = dict(payload, fixture_retry=retry_test, fixture_migration=migration_test, fixture_coarse=coarse_test, fixture_scoped=scoped_test, fixture_imports=imports_test, fixture_submission=submission_test, fixture_ordinary_refresh=worker_changes)
        return super().start_or_adopt(command, payload, **kwargs)


class FixtureDriver(ProductionDriver):
    def pausing(self):
        # This canary ends at two publications. A/B are deliberately outside
        # the approved target, so later native Cleanup may legitimately prune
        # them; exercise the ordinary drain boundary before that next phase.
        state = self.state()['dispatch']
        failed_original = staged_test == 'bad-consumer' and any(
            task['assignment']['scope']['nodes'][0] == 'B' and task['status']['status'] == 'resolved'
            for task in state['tasks'].values())
        return (super().pausing() or failed_original
                or (semantic_reservations_test or dependent_acceptance_test or staged_test)
                and len(state['publications']) >= 2)

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        if publication_refresh_test:
            original = self.native.kernel
            def kernel(payload):
                if payload.get('action') != 'try_refresh_trellis_worker_result':
                    return original(payload)
                logs = [self.runtime / 'checker-state/server.log',
                    self.root / '2/acceptance/2/runtime/checker-state/server.log',
                    Path(payload['repo_path']) / '.trellis/logs/check-ledger.jsonl',
                    Path(payload['baseline_repo_path']) / '.trellis/logs/check-ledger.jsonl']
                before = [path.stat().st_size if path.exists() else 0 for path in logs]
                started = time.monotonic()
                result = original(payload)
                after = [path.stat().st_size if path.exists() else 0 for path in logs]
                assert before == after, 'native locked refresh issued a checker request'
                (base / 'publication-refresh-native.json').write_text(json.dumps(dict(
                    result=result['output'], wall_secs=time.monotonic() - started,
                    current_request=payload['request'],
                    checker_log_bytes_before=before, checker_log_bytes_after=after)))
                return result
            self.native.kernel = kernel

    async def _fixture_wait(self, predicate, description):
        deadline = time.monotonic() + 300
        while not predicate():
            if time.monotonic() >= deadline:
                raise AssertionError(description)
            await asyncio.sleep(.1)

    async def _route(self, event):
        if probe_reuse_test and event.get('kind') == 'verifier_finished':
            if ordinary_refresh:
                job = self.state()['dispatch']['verifiers']['jobs'][str(event['id'])]
                if job['request']['kind'] == 'Corr':
                    # This race changes substantiveness/context only. A new
                    # correspondence approval changes real validation operands
                    # and is correctly outside an exact-read reuse test.
                    await self._fixture_wait(lambda: '2' in self.state()['dispatch']['publications'],
                                             'ordinary B did not publish before deferred correspondence delivery')
                    return await super()._route(event)
            # Delay only delivery of a genuine verifier result. No protocol
            # values, verdicts or source hashes are fabricated for this race.
            await self._fixture_wait(lambda: (self.root / '2/acceptance/2/canonical.json').exists(),
                                     'B did not capture its second acceptance')
            result = await super()._route(event)
            (base / 'probe-reuse-verifier-committed.json').write_text(json.dumps(event))
            return result
        return await super()._route(event)

    async def _wait(self, process, record, **kwargs):
        if concurrent_acceptance_test and process.root.parent.parent.name == 'acceptance':
            # Both real host processes must be permitted before either may
            # publish. The former coordinator-wide integration lock deadlocks
            # here. Later acceptance rounds need no barrier.
            if process.root.parent.name == '1':
                limit = time.monotonic() + 120
                while len(list(self.root.glob('[0-9]*/acceptance/1/execution/permit.json'))) < 2:
                    if time.monotonic() > limit:
                        raise AssertionError('independent host acceptances were serialized')
                    await asyncio.sleep(.1)
                (base / 'concurrent-acceptance-observed.json').write_text(json.dumps(dict(
                    time=time.time(), permits=[str(p) for p in self.root.glob(
                        '[0-9]*/acceptance/1/execution/permit.json')])))
        result = await super()._wait(process, record, **kwargs)
        if (semantic_reservations_test or staged_test) and process.root.parent.parent == self.root / '2/acceptance':
            await self._fixture_wait(lambda: '1' in self.state()['dispatch']['publications'],
                                     'A did not publish before B delivered its independently checked result')
        if probe_reuse_test and process.root.parent.parent == self.root / '2/acceptance':
            if process.root.parent.name == '1':
                await self._fixture_wait(lambda: '1' in self.state()['dispatch']['publications'],
                                         'A did not publish before B first acceptance')
            elif process.root.parent.name == '2':
                await self._fixture_wait(lambda: (base / 'probe-reuse-verifier-committed.json').exists(),
                                         'verifier did not commit after B captured its second acceptance')
        return result


def verify_probe_reuse():
    from trellis.dispatch.candidate import canonical_json, digest
    previous = runtime / 'dispatch/2/acceptance/2'
    if publication_refresh_test:
        assert not (runtime / 'dispatch/2/acceptance/3').exists(), 'context drift started another acceptance round'
        captured = json.loads((previous / 'canonical.json').read_text())
        original = json.loads((previous / 'execution/response.json').read_text())
        saved = [json.loads(path.read_text()) for path in (previous / 'refreshes').glob('*.json')]
        assert len(saved) == 1, saved
        receipt = saved[0]
        assert receipt['donor_completion_sha256'] == digest((previous / 'completed.json').read_bytes())
        assert receipt['current_key']['input_hashes'] == captured['key']['input_hashes']
        assert receipt['current_key']['acceptance_sha256'] != captured['key']['acceptance_sha256']
        fresh = receipt['output']['response']
        assert fresh['outcome'] == 'Valid' and fresh['request_id'] != original['response']['request_id']
        assert receipt['output']['validation_fact_reuse']
        issued = fresh.get('local_closure_revalidation', {}).get('refreshed', [])
        assert any(node == 'B' for node, _record in issued), 'fresh certificate composition omitted B'
        native = json.loads((base / 'publication-refresh-native.json').read_text())
        assert native['result']['status'] == 'refreshed', native['result']
        assert fresh['request_id'] == native['current_request']['id']
        assert fresh['cycle'] == native['current_request']['cycle']
        assert native['checker_log_bytes_before'] == native['checker_log_bytes_after']
        state = json.loads((runtime / 'protocol_state.json').read_text())
        if ordinary_refresh:
            assert state['dispatch']['tasks']['2']['assignment']['task_kind'] == 'ordinary'
            present = set(state['live']['present_nodes'])
            if ordinary_refresh == 'helpers':
                helpers = present - {'A', 'B', 'H', 'Preamble', 'Root'}
                assert len(helpers) == 2 and helpers <= set(state['local_closure_records'])
                assert helpers <= {node for node, _record in issued}
            else:
                assert 'D' not in present and 'D' not in state['local_closure_records']
                assert not (repo / 'Tablet/D.lean').exists() and not (repo / 'Tablet/D.tex').exists()
        (base / 'publication-refresh-passed.json').write_bytes(canonical_json(dict(
            passed=True, acceptance_rounds=2, context_only_retry_rounds=0,
            ordinary_variant=ordinary_refresh,
            old_cycle=original['response']['cycle'], fresh_cycle=fresh['cycle'],
            old_request_id=original['response']['request_id'], fresh_request_id=fresh['request_id'],
            native_refresh_seconds=native['wall_secs'], native_checker_requests=0,
            fresh_certificate_owners=[node for node, _record in issued],
            donor_completion_sha256=receipt['donor_completion_sha256'])))
        print('PASS: source change rechecked; context-only drift published with fresh native certificates and no new check', flush=True)
        return
    current = runtime / 'dispatch/2/acceptance/3'
    old = json.loads((previous / 'canonical.json').read_text())
    new = json.loads((current / 'canonical.json').read_text())
    assert old['source']['input_hashes'] == new['source']['input_hashes']
    assert old['acceptance_sha256'] != new['acceptance_sha256']
    seed = json.loads((current / 'probe-cache-seed.json').read_text())
    assert seed['donor_round'] == '2'
    assert seed['donor_completion_sha256'] == digest((previous / 'completed.json').read_bytes())
    rows = [json.loads(line) for line in (current / 'runtime/checker-state/server.log').read_text().splitlines()]
    probes = [row for row in rows if row.get('op') in ('lean_semantic_payloads', 'print_axioms', 'local_closure_axioms')]
    semantics = [r for r in probes if r['op'] == 'lean_semantic_payloads']
    axioms = [r for r in probes if r['op'] == 'print_axioms']
    full = [r for r in probes if r.get('closure_mode') == 'full']
    scans = [r for r in probes if r.get('closure_mode') == 'scan']
    assert semantics and all(r['cache_misses'] == 0 and r['cache_hits'] > 0 for r in semantics)
    if fact_reuse_test:
        checked = json.loads((current / 'execution/response.json').read_text())
        reuse = checked.get('validation_fact_reuse')
        assert reuse and reuse['donor_round'] == '2', checked.keys()
        assert reuse['donor_completion_sha256'] == seed['donor_completion_sha256']
        donation = checked['replay_donation']
        assert donation['status'] == 'installed', donation
        assert 'B' in donation['covered_nodes'], donation
        # Unrelated materialized owners need not acquire a new attestation.
        # Native acceptance names its actual artifact-reading roots; the
        # transferred environment still binds every transitive imported byte.
        required = set(checked['validation_fact_receipt']['artifact_owners'])
        assert required <= set(donation['covered_nodes']), (required, donation)
        # Real native/checker work ran in different private round directories.
        # A genuine source change forced B's second round; genuine verifier
        # delivery changed only context after that round's capture. Neither
        # race is synthesized by editing a protocol snapshot or an old receipt.
        assert old['config_sha256'] == new['config_sha256']
        prepared = [json.loads((p / 'prepared.json').read_text()) for p in (previous, current)]
        assert prepared[0]['prepared_config_sha256'] != prepared[1]['prepared_config_sha256']
        subprocesses = [entry for row in rows for entry in row.get('measurement', {}).get('subprocesses', [])]
        assert not any('kernel_replay' in entry.get('phases', []) for entry in subprocesses), subprocesses
        assert checked['ok'] and checked['response']['outcome'] == 'Valid'
        summary = dict(passed=True, source_identical=True, native_context_changed=True,
            private_config_relocated=True, validation_fact_reuse=reuse,
            copied_nodes=donation['copied_nodes'], donor_round=donation['donor_round'],
            replay_import_seconds=donation['wall_secs'], kernel_replay_subprocesses=0,
            semantic_hits=sum(r['cache_hits'] for r in semantics),
            remaining_subprocesses=len(subprocesses))
        (base / 'fact-reuse-passed.json').write_bytes(canonical_json(summary))
        print('PASS: current native acceptance reused authenticated validation and replay facts after context drift', flush=True)
        return
    assert axioms and all(r['print_axioms_cache_hit'] for r in axioms)
    assert full and all(r['local_closure_axioms_cache_hit'] for r in full)
    assert all(r['measurement']['subprocess_count'] == 0 for r in semantics + axioms + full)
    assert scans and all(not r['local_closure_axioms_cache_hit'] and r['measurement']['subprocess_count'] > 0 for r in scans)
    summary = dict(passed=True, source_identical=True, native_context_changed=True, seed=seed,
        semantic_hits=sum(r['cache_hits'] for r in semantics),
        axiom_hits=len(axioms), full_closure_hits=len(full), live_scan_checks=len(scans),
        probes=[{k: v for k, v in r.items() if k != 'measurement'} for r in probes])
    (base / 'probe-reuse-passed.json').write_bytes(canonical_json(summary))
    print('PASS: new native context reused host probe facts; scan-only checks stayed live', flush=True)


async def run():
    driver = FixtureDriver(runtime, process_factory=FixtureProcess)
    coordinator = Coordinator(driver)
    runner = asyncio.create_task(coordinator.run())
    deadline = time.monotonic() + (1800 if submission_test else 600)
    restarted = False
    original_processes = {}
    try:
        while time.monotonic() < deadline:
            if runner.done():
                await runner
                if not (semantic_reservations_test or dependent_acceptance_test or staged_test):
                    raise AssertionError('coordinator stopped before fixture completion')
            state = driver.state()
            tasks = list(state['dispatch']['tasks'].values())
            if staged_test == 'bad-consumer':
                # Retain ordinary retry policy, but end this canary at the
                # original resolution. A native-created retry is not a third
                # candidate whose completion this rejection test must await.
                originals = {}
                for task in sorted(tasks, key=lambda task: task['id']):
                    originals.setdefault(task['assignment']['scope']['nodes'][0], task)
                tasks = list(originals.values())
            if ordinary_refresh and len(tasks) < 2:
                assert (state['dispatch'].get('reviewer') or {}).get('request_id', 0) < 10, \
                    'ordinary fixture assignment repeatedly refused; inspect retained reviewer responses'
            parallel_tasks = [t for t in tasks if not t['assignment']['scope']['exclusive']]
            if restart_test and not restarted and len(parallel_tasks) == 2 and all(t['status']['status'] == 'running' for t in parallel_tasks):
                original_processes = {t['id']: t['process'] for t in parallel_tasks}
                await coordinator.shutdown()
                runner.cancel()
                await asyncio.gather(runner, return_exceptions=True)
                driver = FixtureDriver(runtime, process_factory=FixtureProcess)
                coordinator = Coordinator(driver)
                runner = asyncio.create_task(coordinator.run())
                restarted = True
                print('restarted coordinator with both worker identities retained', flush=True)
            if len(tasks) >= (2 + int(retry_test) + int(migration_test)) and all(t['status']['status'] == 'resolved' for t in tasks):
                if shadow_imports_test:
                    assert all(t['status']['outcome']['kind'] == 'rejected' and 'elaborated theorem statement' in t['status']['outcome']['reason'] for t in tasks), tasks
                    assert set(state['live']['open_nodes']) == {'A', 'B'}
                    assert not state['dispatch']['publications']
                    assert all('import Tablet.H' not in (repo / f'Tablet/{n}.lean').read_text() for n in ('A', 'B'))
                    print('PASS: imported notation changing the typed statement rejected before publication', flush=True)
                    return
                if staged_test == 'bad-consumer':
                    by_node = {t['assignment']['scope']['nodes'][0]: t for t in tasks}
                    assert set(by_node) == {'A', 'B'}, tasks
                    assert by_node['A']['status']['outcome']['kind'] == 'accepted', tasks
                    failed = by_node['B']
                    assert failed['assignment']['task_kind'] == 'ordinary'
                    assert failed['status']['outcome']['kind'] == 'rejected', failed
                    rounds = list((runtime / f'dispatch/{failed["id"]}/acceptance').glob('*/canonical.json'))
                    assert len(rounds) == 1, 'bad current consumer repeated owned checking'
                    owned = rounds[0].parent
                    proof = json.loads((owned / 'execution/response.json').read_text())
                    assert proof['stage'] == 'owned' and proof['status'] == 'facts', proof
                    assert proof['facts']['owner_ok'] is True
                    assert {'B', 'D'} <= set(proof['facts']['owner_nodes'])
                    assert 'Root' not in proof['facts']['read_nodes']
                    finals = list((owned / 'finalization').glob('*/staged-completed.json'))
                    assert len(finals) == 1, 'bad current consumer repeated finalization'
                    final = json.loads((finals[0].parent / 'execution/response.json').read_text())
                    assert final['status'] == 'checked', final  # Never Cold/unsupported.
                    checked = final['output']
                    assert checked['final_outcome'] == 'invalid' and not checked['ok'], checked
                    assert checked['response']['outcome'] == 'Invalid'
                    assert checked['response'].get('acceptance_check_passed', False) is False
                    errors = '\n'.join(checked['errors'])
                    assert 'Root' in errors and 'rfl' in errors, errors
                    publications = state['dispatch']['publications']
                    assert str(failed['id']) not in publications and len(publications) == 1, publications
                    assert 'B' in state['live']['open_nodes'] and 'A' not in state['live']['open_nodes']
                    retained = (repo / 'Tablet/B.lean').read_text()
                    assert 'sorry' in retained and 'import Tablet.D' in retained
                    assert '-- BODY\n  0' in (repo / 'Tablet/D.lean').read_text()
                    assert 'have zero : D = 0 := by rfl' in (repo / 'Tablet/Root.lean').read_text()
                    assert not (runtime / 'dispatch/publication/pending.json').exists()
                    assert not list(owned.glob('rejections/*.json')), 'semantic rejection mislabeled freshness failure'
                    (base / 'staged-bad-consumer-passed.json').write_text(json.dumps(dict(
                        passed=True, owner_ok=True, outcome='Invalid', errors=checked['errors'],
                        rejected_task=failed['id'], publications=publications), indent=2))
                    print('PASS: owner proof succeeds; bad current consumer returns native Invalid without publication or another check', flush=True)
                    return
                accepted = [t for t in tasks if t['status']['outcome']['kind'] == 'accepted']
                assert len(accepted) == 2 + int(migration_test), tasks
                if retry_test:
                    assert len(tasks) == 3 + int(migration_test) and any(t.get('retry_of') == 1 for t in tasks)
                assert not state['live']['open_nodes']
                assert len(state['dispatch']['publications']) == 2 + int(migration_test)
                if concurrent_acceptance_test:
                    assert (base / 'concurrent-acceptance-observed.json').is_file()
                    executions = [runtime / f'dispatch/{t["id"]}/acceptance/1/execution' for t in parallel_tasks[:2]]
                    assert max((p / 'permit.json').stat().st_mtime_ns for p in executions) < min(
                        (p / 'result.json').stat().st_mtime_ns for p in executions), 'host processes did not overlap'
                    rounds = [p for p in (runtime / 'dispatch').glob('[0-9]*/acceptance/*/rejections/*.json')]
                    if semantic_reservations_test or staged_test:
                        assert not rounds, 'independent acceptance was invalidated by a sibling publication'
                        assert all(len(list((runtime / f'dispatch/{t["id"]}/acceptance').glob('*/canonical.json'))) == 1
                                   for t in parallel_tasks[:2])
                        owned = runtime / 'dispatch/2/acceptance/1'
                        if not (owned / 'staged-completed.json').exists():
                            rebases = [json.loads(p.read_text()) for p in (owned / 'semantic-rebase').glob('*.json')]
                            assert any(r['status'] == 'rebased' for r in rebases), rebases
                        assert {'A', 'B'} <= set(state['local_closure_records'])
                    else:
                        assert rounds, 'publication must reject acceptance of an obsolete canonical snapshot'
                if dependent_acceptance_test:
                    admissions = [json.loads((runtime / f'dispatch/{t["id"]}/acceptance-admission.json').read_text())
                                  for t in parallel_tasks[:2]]
                    earlier, later = sorted(admissions, key=lambda r: r['admitted_at_ms'])
                    assert earlier['finished_at_ms'] <= later['admitted_at_ms'], admissions
                    assert all(len(list((runtime / f'dispatch/{t["id"]}/acceptance').glob('*/canonical.json'))) == 1
                               for t in parallel_tasks[:2])
                if restart_test:
                    assert restarted
                    assert all(original_processes[t['id']] == t['process'] for t in tasks if t['id'] in original_processes)
                starts = [json.loads((runtime / f'dispatch/{t["id"]}/repo/.trellis/staging/fixture-agent-start.json').read_text()) for t in parallel_tasks[:2]]
                if not scoped_test:
                    assert max(starts) - min(starts) < (90 if submission_test else 10), 'independent worker execution was serialized'
                if coarse_test:
                    assert set(state['coarse_goals']['goals']) == {'A', 'B'}
                if scoped_test:
                    assert state['coarse_goals']['goals']['A']['history_head']
                    from trellis.dispatch.coarse_audits import read_history
                    report, _ = read_history(repo, state['coarse_goals']['goals']['A']['history_head'])
                    assert report['context']['goals'] == ['A']
                    assert 'Tablet/B.lean' not in report['evidence']['input_hashes']
                    assert not state['coarse_goals']['goals']['B'].get('history_head')
                    overlap = [json.loads(p.read_text()) for p in (runtime / 'dispatch').glob('verifier-*/repo/.trellis/staging/fixture-audit-overlap.json')]
                    assert any(row['started'] <= min(starts) <= row['finished'] for row in overlap), 'B did not overlap the scoped audit'
                for node in ('A', 'B'):
                    text = (repo / f'Tablet/{node}.lean').read_text()
                    if imports_test:
                        assert 'import Tablet.H\n' in text and 'True.intro) H' in text
                    elif not submission_test:
                        assert 'trivial' in text
                if staged_test:
                    stages = []
                    for task in parallel_tasks[:2]:
                        owned = runtime / f'dispatch/{task["id"]}/acceptance/1'
                        proof = json.loads((owned / 'execution/response.json').read_text())
                        assert proof['stage'] == 'owned' and proof['status'] == 'facts', proof
                        assert not {'response', 'outcome', 'acceptance_check_passed'} & proof.keys()
                        assert 'Root' not in proof['facts']['read_nodes'], 'shared consumer serialized owned proof work'
                        finalized = list((owned / 'finalization').glob('*/staged-completed.json'))
                        assert len(finalized) == 1, 'one current finalization per candidate is sufficient'
                        final = finalized[0].parent
                        output = json.loads((final / 'execution/response.json').read_text())
                        assert output['stage'] == 'finalize' and output['status'] == 'checked'
                        assert output['output']['response']['outcome'] == 'Valid'
                        stages.append(dict(task_id=task['id'],
                            owned_start=(owned / 'execution/permit.json').stat().st_mtime,
                            owned_end=(owned / 'execution/result.json').stat().st_mtime,
                            finalize_start=(final / 'execution/permit.json').stat().st_mtime,
                            finalize_end=(final / 'execution/result.json').stat().st_mtime))
                    assert max(s['owned_start'] for s in stages) < min(s['owned_end'] for s in stages), stages
                    first, second = sorted(stages, key=lambda s: s['finalize_start'])
                    assert first['finalize_end'] <= second['finalize_start'], stages
                    assert {'A', 'B', 'Root'} <= set(state['local_closure_records'])
                    if staged_test == 'helpers':
                        helpers = set(state['live']['present_nodes']) - {'A', 'B', 'H', 'Preamble', 'Root'}
                        assert len(helpers) == 2 and helpers <= set(state['local_closure_records'])
                    assert not (runtime / 'dispatch/publication/pending.json').exists()
                    (base / 'staged-publication-passed.json').write_text(json.dumps(dict(
                        passed=True, variant=staged_test, stages=stages,
                        publications=state['dispatch']['publications']), indent=2))
                    print('PASS: shared-consumer proof work overlaps; one current finalization each; both publications retained', flush=True)
                    return
                if semantic_reservations_test or dependent_acceptance_test:
                    assert not (runtime / 'dispatch/publication/pending.json').exists()
                    admissions = [json.loads((runtime / f'dispatch/{t["id"]}/acceptance-admission.json').read_text())
                                  for t in parallel_tasks[:2]]
                    (base / 'semantic-publication-passed.json').write_text(json.dumps(dict(
                        passed=True, dependent=dependent_acceptance_test, admissions=admissions,
                        publications=state['dispatch']['publications']), indent=2))
                    print('PASS: semantic input reservations, one check each, both native publications retained', flush=True)
                    return
                if submission_test:
                    by_node = {t['assignment']['scope']['nodes'][0]: t for t in tasks}
                    assert by_node['A']['assignment']['task_kind'] == 'ordinary'
                    assert by_node['B']['assignment']['task_kind'] == 'lean_closure'
                    for name, count in [('A', 2), ('B', 1)]:
                        task = by_node[name]
                        assert task['worker_attempt']['submissions'] == count, task
                        assert task['worker_attempt']['consumed_ms'] < task['worker_attempt']['allowance_ms']
                        turns = runtime / f'dispatch/{task["id"]}/turns'
                        clocks = [json.loads(p.read_text()) for p in sorted(turns.glob('*/execution/clock/finished.json'))]
                        assert len(clocks) == count
                        assert len({r['thread_id'] for r in clocks}) == 1
                        assert sum(r['elapsed_ms'] for r in clocks) == task['worker_attempt']['consumed_ms']
                        assert len(list(turns.glob('*/candidate/candidate.json'))) == count
                    checks = list((runtime/'dispatch').glob('[0-9]*/acceptance/*/response.json'))
                    (base/'submission-canary-passed.json').write_text(json.dumps(dict(
                        task_ids=[t['id'] for t in tasks], checks=[str(p) for p in checks])))
                jobs = list(state['dispatch']['verifiers']['jobs'].values())
                good = [p for p in (runtime / 'dispatch').glob('verifier-*/execution/response.json')
                        if '"Pass"' in p.read_text()]
                if any(j['status']['status'] == 'resolved' for j in jobs) and good:
                    for job in jobs:
                        prepared = runtime / f'dispatch/verifier-{job["id"]}/dispatch-request.json'
                        if prepared.exists():
                            request = json.loads(prepared.read_text())
                            if request.get('scoped_audit'):
                                continue
                            bindings = request[f'{request["kind"].lower()}_verify_lane_bindings']
                            assert bindings, 'ordinary verifier bindings were not resolved'
                    assert not (runtime / 'dispatch/publication/pending.json').exists()
                    if probe_reuse_test:
                        verify_probe_reuse()
                    print('PASS: reviewer -> two confined workers -> native publication -> verifier execution', flush=True)
                    return
            await asyncio.sleep(.5)
        raise AssertionError('production integration fixture timed out')
    finally:
        await coordinator.shutdown()
        runner.cancel()
        await asyncio.gather(runner, return_exceptions=True)


print('fixture:', base, flush=True)
asyncio.run(run())

"""One serial host operation; canonical protocol transitions belong to Rust.

The durable launcher, provider clock, submission helpers and normalizer are
shared with parallel execution. Only the final response reaches the ordinary
serial source/checkpoint transaction.
"""
from __future__ import annotations

from dataclasses import asdict
import json
from pathlib import Path
import secrets
import sys
import time
from contextlib import nullcontext
from types import SimpleNamespace

from trellis.agent_wrapper import provider_clock
from trellis.dispatch import candidate
from trellis.dispatch.process import AttemptProcess, alive
from trellis.dispatch.publication import atomic


def read(path):
    return json.loads(Path(path).read_bytes())


def write(path, value):
    atomic(Path(path), candidate.canonical_json(value))


def operation(request, observation=None, *, allowance=None):
    event = dict(event='worker_attempt_observed', request_id=request['id'], observation=observation)
    if allowance is not None:
        event = dict(event='worker_attempt_started', request_id=request['id'], allowance_ms=allowance)
    return dict(kind='worker_operation', event=event)


def _root(runtime, request, attempt):
    reference = read(runtime / 'worker-attempts/current.json')
    directory = runtime / 'worker-attempts' / reference['directory']
    if (directory.parent != runtime / 'worker-attempts' or reference['request_id'] != request['id']
            or reference['contract_sha256'] != attempt['contract_sha256']):
        raise RuntimeError('serial worker evidence belongs to a different request')
    return directory


def _wait(process, record, *, started=False):
    while True:
        pause_path = process.root / 'provider-pause.json'
        if pause_path.exists():
            pause = read(pause_path)
            permit = dict(ordinal=pause['ordinal'], process=record)
            resumed = process.root / 'provider-resume.json'
            if pause.get('resume_allowed') is False:
                raise RuntimeError('Provider continuation unavailable; retain this attempt: ' + pause['pause']['reason'])
            if not resumed.exists() or read(resumed) != permit:
                from trellis.provider_budget import ProviderBudgetExhausted
                detail = pause['pause']
                raise ProviderBudgetExhausted(provider=detail['provider'], model=detail.get('model'),
                    role=detail['role'], detail=detail['reason'])
        if started:
            receipt = provider_clock.read(process.root / 'clock', 'started')
            if receipt is not None:
                return receipt
        result = process.result()
        if result is None and not alive(record):
            # Completion can be published between the first read and exit.
            result = process.result()
            if result is None:
                raise RuntimeError('serial worker launcher exited without a receipt; retain its files')
        if result is not None:
            data = (process.root / 'response.json').read_bytes()
            if candidate.digest(data) != result['response_sha256']:
                raise RuntimeError('serial worker result differs from its owned receipt')
            return None if started else (result, json.loads(data))
        time.sleep(.2)


def _terminal(directory, request, response, repo, *, enriched=False, observation=None):
    write(directory / 'terminal.json', dict(response=response, source_manifest=candidate.source_manifest(repo), enriched=enriched))
    return operation(request, observation or dict(kind='finished'))


def owned_executions(directory):
    for pattern in ('turns/*/execution', 'turns/*/checks/*/execution'):
        for execution in sorted(Path(directory).glob(pattern)):
            if (execution / 'process.json').exists():
                yield execution, read(execution / 'process.json')


def require_idle(runtime):
    """A rewind/new assignment cannot inherit an uncollected old launcher."""
    for directory in (Path(runtime) / 'worker-attempts').glob('*'):
        if not directory.is_dir():
            continue
        for execution, record in owned_executions(directory):
            cleanup = execution / 'cleanup.json'
            if alive(record) or not cleanup.exists() or read(cleanup) != record:
                raise RuntimeError('stop and reap retained serial workers before rewinding or starting a new assignment')


def stop(runtime):
    """Explicit abandonment before an operator rewind; never a resume permit."""
    for directory in (Path(runtime) / 'worker-attempts').glob('*'):
        if directory.is_dir():
            for execution, record in owned_executions(directory):
                AttemptProcess(execution).stop_and_reap(record)
    require_idle(runtime)


def next_step(config, runtime, request, attempt, *, contract_sha256):
    from trellis.runtime import bridge
    engineering = request.get('kind') == 'Engineering'
    host = None
    if engineering:
        from trellis.modeling import engineering_transport as host
        host.native_owner(request)
    runtime = Path(runtime)
    if not request.get('worker_submission_policy') or request.get('task_execution'):
        raise RuntimeError('serial worker operation requires its native submission contract')
    if attempt is None:
        require_idle(runtime)
        if engineering:
            from trellis.adapters import ProviderConfig
            profile = request['engineering']['profile']
            provider, decision = ProviderConfig(provider=profile['provider'], model=profile['model'], effort=profile['reasoning_effort']), None
        else:
            provider, decision = bridge._worker_execution_provider(request, config.repo_path)
        if provider.provider not in {'codex', 'deepseek', 'kimi'} or not provider.model or not provider.effort:
            raise RuntimeError('serial submissions require an explicit Codex, DeepSeek, or Kimi model and effort')
        # A new ordinary attempt gets a new owned launcher namespace, including
        # after rewind. An existing native attempt always adopts current.json.
        directory = runtime / 'worker-attempts' / (str(request['id']) + '-' + secrets.token_hex(8))
        directory.mkdir(parents=True, mode=0o700)
        allowance = request['engineering']['profile']['allowance_ms'] if engineering else int(bridge._policy(config).timing.burst_timeout_seconds * 1000)
        write(directory / 'profile.json', dict(provider=asdict(provider), allowance_ms=allowance,
                                               model_ab_decision=decision))
        if not engineering:
            write(directory / 'base-manifest.json', candidate.source_manifest(config.repo_path))
        write(runtime / 'worker-attempts/current.json', dict(directory=directory.name,
            request_id=request['id'], contract_sha256=contract_sha256))
        return operation(request, allowance=allowance)
    directory = _root(runtime, request, attempt)
    turn = directory / 'turns' / str(attempt['turn'])
    execution = turn / 'execution'
    phase = attempt['phase']['phase']
    process = host.native_process(request, execution) if engineering else AttemptProcess(execution)
    if phase == 'ready':
        profile = read(directory / 'profile.json')
        feedback_hash = attempt['phase'].get('feedback_sha256')
        feedback = read(runtime / 'worker-feedback' / (feedback_hash + '.json')) if feedback_hash else None
        submission = dict(turn=attempt['turn'], remaining_ms=attempt['allowance_ms'] - attempt['consumed_ms'],
            session=attempt.get('session'), execution_record=str(execution / 'clock'),
            session_runtime=str(directory / 'runtime'), provider=profile['provider'],
            model_ab_decision=profile.get('model_ab_decision'),
            feedback=feedback, feedback_sha256=feedback_hash)
        payload = dict(config_path=str(config.source_path), runtime_root=str(runtime), request=request,
                       worker_turn=submission)
        checker = None
        if engineering:
            submission.update(host.native_turn(request, runtime))
        else:
            from trellis.checker.jobs import enabled
            if not enabled():
                raise RuntimeError('serial submissions require the central managed compiler service')
            checker = dict(serial=True, runtime=str(runtime), task_runtime=str(directory / 'runtime'),
                           repo=str(config.repo_path), compiler='managed', lifetime=None,
                           scope=str(execution.relative_to(runtime)))
        with host.native_start(request) if engineering else nullcontext():
            record = process.start_or_adopt([sys.executable, '-m', 'trellis.runtime.bridge_cli'], payload,
                checker=checker)
        if not (execution / "permit.json").exists():
            process.permit(record)
        receipt = _wait(process, record, started=True)
        if receipt is None:
            result, response = _wait(process, record)
            process.stop_and_reap(record)
            if engineering or result['returncode'] != 0 or response.get('kind') != 'worker':
                raise RuntimeError('serial worker failed before provider start; retain its launch evidence')
            return _terminal(directory, request, response, config.repo_path)
        return operation(request, dict(kind='launched', process=record, started_at_ms=receipt['started_at_ms']))
    if phase == 'running':
        record = attempt['phase']['process']
        result, response = _wait(process, record)
        process.stop_and_reap(record)
        if result['returncode'] != 0 or response.get('kind') != 'worker_submission' or response.get('turn') != attempt['turn']:
            raise RuntimeError('serial worker has no owned submission; retain its execution evidence')
        timing = provider_clock.read(execution / 'clock', 'finished')
        if not timing or timing != response.get('provider_execution') or timing['disposition'] not in ('completed', 'expired'):
            raise RuntimeError('serial worker delivery has no completed or exhausted owned clock')
        exhausted = timing['disposition'] == 'expired'
        if not exhausted and not response.get('provider_turn_completed'):
            raise RuntimeError('serial provider history did not finish closing; retain the submission')
        write(turn / 'delivery.json', response)
        if engineering and exhausted:
            return operation(request, dict(kind='expired', process=record))
        if exhausted and response.get('raw') is None:
            terminal = bridge._save_malformed_worker_result(runtime_root=runtime, request=request,
                raw_payload=None, acceptance_context_path=None, authoritative_repo_path=config.repo_path,
                errors=['Worker execution allowance exhausted before a result was available'], transport_failure=True)
            return _terminal(directory, request, terminal, config.repo_path,
                             observation=dict(kind='expired', process=record))
        session = read(directory / 'runtime/worker-session.json')
        if session['thread_id'] != timing.get('thread_id'):
            raise RuntimeError('serial worker delivery differs from its retained provider conversation')
        completion = dict(process=record, turn=attempt['turn'], exhausted=exhausted, session=session,
            response_sha256=candidate.digest(candidate.canonical_json(response)),
            elapsed_ms=attempt['allowance_ms'] - attempt['consumed_ms'] if exhausted else timing['elapsed_ms'])
        return operation(request, dict(kind='turn_completed', completion=completion))
    if phase == 'submitted':
        delivery = read(turn / 'delivery.json')
        if candidate.digest(candidate.canonical_json(delivery)) != attempt['phase']['completion']['response_sha256']:
            raise RuntimeError('serial delivery no longer matches its native completion')
        if engineering:
            identity = host.native_seal(request, attempt, turn, delivery)
            return operation(request, dict(kind='sealed', candidate_sha256=identity))
        identity = candidate.seal(config.repo_path, turn / 'candidate', task_id=request['id'],
            base_manifest=read(directory / 'base-manifest.json'), inputs={}, response=delivery.get('raw'),
            submission=dict(turn=attempt['turn'], contract_sha256=attempt['contract_sha256'],
                            delivery_sha256=attempt['phase']['completion']['response_sha256']))
        return operation(request, dict(kind='sealed', candidate_sha256=identity))
    if phase == 'checking':
        if engineering:
            return host.native_check(request, attempt, turn)
        sealed, identity = candidate.read(turn / 'candidate')
        if identity != attempt['phase']['candidate_sha256']:
            raise RuntimeError('serial checker candidate differs from native authorization')
        check_root = turn / 'checks' / str(attempt['infrastructure_retries'])
        checking = AttemptProcess(check_root / 'execution')
        payload = dict(config_path=str(config.source_path), runtime_root=str(runtime), request=request,
                       directory=str(directory), turn=attempt['turn'], candidate_sha256=identity,
                       check_round=attempt['infrastructure_retries'])
        record = checking.start_or_adopt([sys.executable, '-m', __name__, 'check'], payload)
        if not (checking.root / "permit.json").exists():
            checking.permit(record)
        result, checked = _wait(checking, record)
        checking.stop_and_reap(record)
        if result['returncode'] != 0:
            if checked.get('source_manifest') != candidate.source_manifest(config.repo_path):
                raise RuntimeError('serial failed check has no matching source receipt; retain its evidence')
            write(check_root / 'failed.json', checked)
            if attempt['infrastructure_retries'] >= attempt['policy']['max_infrastructure_retries']:
                raise RuntimeError('serial host-check retry limit reached; retain the candidate and diagnostics')
            return operation(request, dict(kind='infrastructure_retry', candidate_sha256=identity))
        if candidate.source_manifest(config.repo_path) != checked['source_manifest']:
            raise RuntimeError('serial sources changed after host checking; retain the checked candidate')
        # Native decides whether correction is authorized. The adapter persists
        # Finished and this exact terminal receipt only after that decision.
        return checked
    if phase == 'finished':
        terminal = read(directory / 'terminal.json')
        current = candidate.source_manifest(config.repo_path)
        response = terminal['response']
        rejected = response.get('status') == 'Malformed' or response.get('outcome') in (
            'Invalid', 'Stuck', 'NeedsRestructure', 'TargetFalseUnderModel', 'UncertifiableUnderCertificateContext')
        if (current != terminal['source_manifest'] and not
                (rejected and current == read(directory / 'base-manifest.json'))):
            raise RuntimeError('serial sources changed after the terminal host result; retain its evidence')
        return dict(kind='worker_terminal', response=response, enriched=terminal.get('enriched', False))
    raise RuntimeError('unknown serial worker phase')


def check(payload):
    from trellis.config import load_config
    from trellis.runtime import bridge
    config = load_config(Path(payload['config_path']))
    runtime, directory = Path(payload['runtime_root']), Path(payload['directory'])
    turn = directory / 'turns' / str(payload['turn'])
    record, identity = candidate.read(turn / 'candidate')
    if identity != payload['candidate_sha256']:
        raise RuntimeError('serial checker candidate identity changed')
    expected = dict(record['base_manifest'])
    for path, delta in record['changes'].items():
        if delta['after'] is None:
            expected.pop(path, None)
        else:
            expected[path] = delta['after']
    if payload.get('check_round', 0):
        previous = turn / 'checks' / str(payload['check_round'] - 1) / 'failed.json'
        failure = read(previous)
        if failure.get('candidate_sha256') != identity:
            raise RuntimeError('serial retry belongs to a different candidate')
        expected = failure['source_manifest']
    if candidate.source_manifest(config.repo_path) != expected:
        raise RuntimeError('serial sources changed after sealing')
    request = payload['request']
    baseline = bridge._bridge_private_state_dir(config.repo_path, runtime) / bridge._artifact_name(
        'worker', request['id'], 'result').replace('.json', '.acceptance.json')
    if isinstance(record['response'], dict):
        response = bridge.check_completed_worker(config=config, runtime_root=runtime, request=request,
            response=SimpleNamespace(payload=record['response'], raw_path=None), acceptance_context=read(baseline),
            acceptance_context_path=baseline, authoritative_repo=config.repo_path)
    else:
        response = bridge._save_malformed_worker_result(runtime_root=runtime, request=request,
            raw_payload=None, acceptance_context_path=baseline, authoritative_repo_path=config.repo_path,
            errors=['Submission must contain a JSON object'])
    if response.get('transport_failure') or any('kind=kernel_failure' in str(reason)
            for reason in response.get('deterministic_rejection_reasons', [])):
        raise RuntimeError('serial host validation failed before adjudicating the candidate: ' + response.get('comments', ''))
    raw = record['response']
    declared = raw.get('outcome') if isinstance(raw, dict) else None
    outcomes = {'valid': 'Valid', 'invalid': 'Invalid', 'stuck': 'Stuck',
                'needs_restructure': 'NeedsRestructure', 'target_false_under_model': 'TargetFalseUnderModel'}
    return dict(kind='worker_checked', candidate_sha256=identity,
                submitted_outcome=outcomes.get(str(declared).lower()), response=response,
                source_manifest=candidate.source_manifest(config.repo_path))


def finish(payload):
    from trellis.config import load_config
    config = load_config(Path(payload['config_path']))
    directory = _root(Path(payload['runtime_root']), payload['request'], payload['attempt'])
    response = payload['response']
    if candidate.source_manifest(config.repo_path) != payload['source_manifest']:
        raise RuntimeError('serial source changed before terminal delivery')
    return _terminal(directory, payload['request'], response, config.repo_path, enriched=True)


def configure(runtime):
    """Apply the explicit launch policy at a stopped native boundary."""
    from trellis.config import load_config
    from trellis.runtime.kernel_cli import run_kernel_cli
    runtime = Path(runtime).resolve()
    require_idle(runtime)
    metadata = read(runtime / 'runtime_metadata.json')
    config_path = Path(metadata['config_path'])
    raw = read(config_path)
    if 'worker_submission_policy' not in raw:
        raise RuntimeError('set worker_submission_policy explicitly in the launch config, or null to disable it')
    config = load_config(config_path)
    if config.parallel_dispatch:
        raise RuntimeError('use trellis.dispatch configure-submissions for parallel runs')
    return run_kernel_cli(dict(action='configure_worker_submissions', root=str(runtime),
                               policy=config.worker_submission_policy))


def resume(runtime):
    """Explicit operator resume grants existing exact launcher permits only."""
    runtime = Path(runtime).resolve()
    if not (runtime / 'protocol_state.json').exists():
        return
    state = read(runtime / 'protocol_state.json')
    record = state.get('worker_attempt')
    if record is None:
        return
    directory = _root(runtime, state['in_flight_request'], record['attempt'])
    from trellis.dispatch.__main__ import provider_resume_permits
    for execution, pause in provider_resume_permits(directory / 'turns'):
        write(execution / 'provider-resume.json', dict(ordinal=pause['ordinal'], process=pause['process']))


if __name__ == '__main__':
    if sys.argv[1] == 'resume':
        resume(sys.argv[2])
    elif sys.argv[1] == 'stop':
        stop(sys.argv[2])
    elif sys.argv[1] == 'configure':
        print(json.dumps(configure(sys.argv[2])))
    elif sys.argv[1] == 'finish':
        print(json.dumps(finish(json.load(sys.stdin))))
    else:
        payload = json.load(sys.stdin)
        try:
            print(json.dumps(check(payload)))
        except Exception as exc:
            from trellis.config import load_config
            repo = load_config(Path(payload['config_path'])).repo_path
            print(json.dumps(dict(error=str(exc), candidate_sha256=payload['candidate_sha256'],
                                  source_manifest=candidate.source_manifest(repo))))
            raise SystemExit(1)

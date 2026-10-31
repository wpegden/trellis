"""Read the display fields without sending full task requests to Node.

Protocol snapshots can exceed JavaScript's string limit. This read-only helper
also keeps those snapshots off the viewer's event loop. Nothing here authorizes
dispatch or acceptance.
"""
import json
from pathlib import Path
import sys


def coarse_goals_view(state, state_path):
    registry = state.get('coarse_goals')
    if registry is None:
        return None
    # Project before Node parses anything; reports are bounded and read only.
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from trellis.dispatch.coarse_audits import read_history
    try:
        metadata = json.loads((Path(state_path).parent / 'runtime_metadata.json').read_text())
        repo = Path(metadata['repo_path'])
    except (OSError, ValueError, KeyError):
        repo = None
    rows = []
    goals = sorted(registry['goals'].items(), key=lambda item: (
        item[1]['status'] != 'active', -item[1]['started_cycle'], item[0]))
    inactive = 0
    for node, goal in goals:
        if goal['status'] != 'active':
            inactive += 1
            if inactive > 10:
                continue
        row = dict(node=node, **{key: goal.get(key) for key in (
            'status', 'started_cycle', 'completed_attempts', 'attempts_since_progress',
            'remaining_open', 'remaining_blockers', 'history_head')})
        row['plan_tasks'] = (goal.get('plan') or {}).get('tasks', [])
        row['history'] = []
        reference = goal.get('history_head')
        if reference and repo is None:
            row['history_error'] = 'Audit history repository is unavailable'
        elif reference:
            try:
                for _ in range(3):
                    if not reference:
                        break
                    report, _ = read_history(repo, reference)
                    if node not in report['context']['goals']:
                        raise ValueError('audit report belongs to a different coarse goal')
                    plan = report['plan']
                    row['history'].append(dict(reference=reference,
                        cycle=plan['written_at_cycle'], request_id=plan['written_by_request'],
                        reason=report['context']['reason'], report=plan['report']))
                    reference = report['context']['previous'].get(node)
                row['older_history'] = reference
            except (OSError, ValueError, KeyError) as error:
                row['history_error'] = str(error)
        elif goal.get('plan'):
            # Migrated serial advice predates the per-goal history store.
            plan = goal['plan']
            row['history'].append(dict(reference=None, cycle=plan['written_at_cycle'],
                request_id=plan['written_by_request'], reason=plan['trigger_at_write'], report=plan['report']))
        rows.append(row)
    return dict(limit=registry['limit'], goals=rows, pending_audits=registry.get('pending_audits', []))


def retired_display(path):
    """A small transcript index, never a protocol state or acceptance input."""
    path = Path(path)
    if path.name != 'protocol_state.json':
        raise FileNotFoundError(path)
    with path.with_name('display-state.json').open('rb') as stream:
        raw = stream.read(65537)
    if len(raw) > 65536:
        raise ValueError('oversized retired task display record')
    value = json.loads(raw)
    task = next((p for p in path.parents if p.parent.name == 'dispatch'), None)
    valid_owner = False
    if isinstance(value, dict) and task is not None:
        if value.get('format') == 'trellis-private-display-state/1':
            valid_owner = (task.name.isdigit() and type(value.get('task_id')) is int
                           and value['task_id'] == int(task.name))
        elif value.get('format') == 'trellis-private-display-state/2':
            owner = value.get('owner')
            valid_owner = (isinstance(owner, dict) and owner.get('kind') in ('review', 'verifier')
                           and type(owner.get('id')) is int and owner['id'] >= 0
                           and task.name == f"{owner['kind']}-{owner['id']}")
    if (not isinstance(value, dict) or not valid_owner
            or not isinstance(value.get('checkpoint_commit'), str)
            or len(value['checkpoint_commit']) not in (40, 64)
            or any(c not in '0123456789abcdef' for c in value['checkpoint_commit'])
            or type(value.get('cycle')) is not int or value['cycle'] < 0):
        raise ValueError('invalid retired task display record')
    request = value.get('in_flight_request')
    if (not isinstance(request, dict) or type(request.get('id')) is not int or request['id'] < 0
            or type(request.get('cycle')) is not int or request['cycle'] < 0
            or not isinstance(request.get('kind'), str) or not request['kind']
            or any(request.get(k) is not None and not isinstance(request[k], str) for k in ('active_node', 'mode'))
            or any(value.get(k) is not None and not isinstance(value[k], str) for k in ('phase', 'stage'))):
        raise ValueError('invalid retired task transcript request')
    return {**{k: value.get(k) for k in ('phase', 'stage', 'cycle')},
            'sidecar_queue': None, 'closure_provenance': None,
            'in_flight_request': {k: request.get(k) for k in ('id', 'kind', 'cycle', 'active_node', 'mode')}}


def project(path):
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    from trellis.private_state import decode_private_state
    try:
        try:
            with Path(path).open() as stream:
                state = json.load(stream)
        except FileNotFoundError:
            try:
                return retired_display(path)
            except FileNotFoundError:
                pass
            sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
            from trellis.dispatch.retained_files import read_bytes
            state = json.loads(read_bytes(path))
    except FileNotFoundError:
        return None
    state = decode_private_state(state)
    result = {key: state.get(key) for key in
              ('phase', 'stage', 'cycle', 'sidecar_queue', 'closure_provenance')}
    if state.get('coarse_goals') is not None:
        result['coarse_goals'] = coarse_goals_view(state, path)
    request = state.get('in_flight_request')
    result['in_flight_request'] = ({key: request.get(key) for key in
                                   ('id', 'kind', 'cycle', 'active_node', 'mode')}
                                  if request else None)
    ledger = state.get('dispatch')
    if not ledger:
        from trellis.dispatch import worker_transport
        from trellis.runtime.bridge import any_halt_marker_present
        runtime = Path(path).parent
        try:
            retry = worker_transport.serial_status(runtime, state)
            if retry is not None and not any_halt_marker_present(runtime) and not (runtime / 'pause_request.json').exists():
                result['provider_retry'] = worker_transport.public_status(retry)
        except (OSError, ValueError, KeyError, TypeError, RuntimeError):
            pass
        return result
    result['dispatch'] = {key: ledger.get(key) for key in
                          ('config', 'draining', 'reviewer')}
    # Rejected assignments never become tasks. Retain recent decisions so a
    # reviewer transcript cannot be mistaken for evidence of worker admission.
    receipts = sorted(ledger.get('receipts', {}).values(), key=lambda receipt: receipt['review_id'])[-10:]
    result['dispatch']['receipts'] = [
        dict(review_id=receipt['review_id'], decisions=[
            {key: decision.get(key) for key in ('assignment_id', 'task_id', 'rejection')}
            for decision in receipt['decisions']]) for receipt in receipts]
    jobs = {}
    for identity, job in (ledger.get('verifiers') or {}).get('jobs', {}).items():
        row = {key: job.get(key) for key in
               ('id', 'status', 'required', 'reviewer_requested')}
        row['identity'] = {key: job['identity'].get(key) for key in ('lane', 'compiler')}
        scope = (job.get('request') or {}).get('scoped_audit')
        if scope:
            row['coarse_nodes'] = scope['goals']
        elif job.get('request') is None:
            goals = ((job.get('retired_request') or {}).get('summary') or {}).get('audit_goals')
            if goals is not None:
                row['coarse_nodes'] = goals
        jobs[identity] = row
    result['dispatch']['verifiers'] = {'jobs': jobs}
    tasks = {}
    from trellis.dispatch.liveness import coordinator_pid
    retry_owner_active = coordinator_pid(Path(path).parent) is not None
    for identity, task in ledger.get('tasks', {}).items():
        row = {key: task.get(key) for key in ('id', 'binding', 'status', 'retry_of',
               'report', 'retry_refusal', 'launched_at_ms', 'execution_capacity_released')}
        row['assignment'] = {key: task['assignment'].get(key) for key in
                             ('profile', 'compiler', 'task_kind', 'scope')}
        from trellis.dispatch import worker_transport
        from trellis.runtime.bridge import any_halt_marker_present
        runtime = Path(path).parent
        try:
            retry = worker_transport.parallel_status(runtime / 'dispatch', task)
            if (retry is not None and not any_halt_marker_present(runtime)
                    and not (runtime / 'pause_request.json').exists()
                    and not (runtime / 'dispatch/publication/pending.json').exists()):
                row['provider_retry'] = worker_transport.public_status(retry, resume_allowed=retry_owner_active)
        except (OSError, ValueError, KeyError, TypeError, RuntimeError):
            pass
        tasks[identity] = row
    result['dispatch']['tasks'] = tasks
    return result


if __name__ == '__main__':
    json.dump(project(sys.argv[1]), sys.stdout)

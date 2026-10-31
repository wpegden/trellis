"""Discard redundant private inputs after terminal task history is committed.

Git checkpoints retain the authoritative history. This policy intentionally
does not preserve enough private input to re-authenticate old host executions.
Candidates, source trees, handoffs, execution records and transcripts stay put.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
from trellis.private_state import parse_private_state
from pathlib import Path
import subprocess
import sys
import threading

from trellis.history_artifacts import decode_shared_state_text
from .candidate import canonical_json, _sync_dir
from .publication import atomic
from . import retained_files as retained

FORMAT = 'trellis-terminal-private-retirement/1'
DISPLAY_FORMAT = 'trellis-private-display-state/1'
OWNER_DISPLAY_FORMAT = 'trellis-private-display-state/2'
OWNER_FORMAT = 'trellis-terminal-private-retirement/2'
HISTORY = '.trellis-history/supervisor_state.json'
RECEIPT = 'private-inputs-retired.json'


def task_identity(task):
    """Stable native identity, excluding differently encoded frozen requests."""
    if task['status']['status'] != 'resolved':
        return None
    value = dict(id=task['id'], review_id=task.get('review_id'),
        reserved_at_ms=task['reserved_at_ms'], assignment_id=task['assignment']['assignment_id'],
        status=task['status'])
    return dict(sha256=hashlib.sha256(canonical_json(value)).hexdigest(), status=task['status'])


def terminal_tasks(state):
    return {id: task_identity(task) for id, task in state['dispatch']['tasks'].items()
            if task['status']['status'] == 'resolved'}


class _DeferredVerifierIdentity:
    """Reference one native observation; never cache across routing snapshots."""
    def __init__(self, job):
        self.value = {key: job[key] for key in ('id', 'identity', 'status')}

    def resolve(self):
        return dict(sha256=hashlib.sha256(canonical_json(self.value)).hexdigest())


def terminal_owners(state, *, defer_verifiers=False):
    """Small stable identities shared by full checkpoints and native routing.

    Scheduling must not serialize all historical verifier input maps under the
    GIL. Defer those hashes to the daemon's selected-directory retirement turn;
    retain references only, not copies or a cross-snapshot identity cache.
    """
    result = terminal_tasks(state)
    ledger = state['dispatch']
    for key, job in ledger['verifiers']['jobs'].items():
        if job['status']['status'] == 'resolved':
            value = _DeferredVerifierIdentity(job)
            result[f'verifier-{key}'] = value if defer_verifiers else value.resolve()
    active = (ledger.get('reviewer') or {}).get('request_id')
    for key, receipt in ledger.get('receipts', {}).items():
        if int(key) != active and receipt['review_id'] == int(key):
            result[f'review-{key}'] = dict(sha256=hashlib.sha256(canonical_json(receipt)).hexdigest())
    return result


def _git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.PIPE)


def _json(path):
    return json.loads(retained.regular_path(path).read_bytes())


def accepted_source_candidates(state):
    """Only published, accepted workers with no retry custody can shed sources.

    Keep this separate from terminal identity: existing receipt identities and
    native scheduling projections retain their original compatibility contract.
    """
    ledger = state['dispatch']
    retried = {task.get('retry_of') for task in ledger['tasks'].values()
               if task.get('retry_of') is not None}
    result = {}
    for key, task in ledger['tasks'].items():
        status = task['status']
        outcome = status.get('outcome')
        publication = ledger.get('publications', {}).get(key)
        if (type(task.get('id')) is not int or task['id'] < 1 or key != str(task['id'])
                or status.get('status') != 'resolved' or not isinstance(outcome, dict)
                or outcome.get('kind') != 'accepted' or task.get('pending_retry') is not None
                or task['id'] in retried or not isinstance(publication, dict)):
            continue
        sha = publication.get('candidate_sha256')
        if (type(outcome.get('event_id')) is not int or outcome['event_id'] < 0
                or type(publication.get('event_id')) is not int
                or publication['event_id'] != outcome['event_id']
                or not isinstance(sha, str) or len(sha) != 64
                or any(char not in '0123456789abcdef' for char in sha)):
            continue
        result[key] = dict(task_id=task['id'], event_id=outcome['event_id'], candidate_sha256=sha)
    return result


def publication_cache_candidates(state):
    """Exact native receipts for resolved accepted tasks without retry custody."""
    result = {}
    for key, accepted in accepted_source_candidates(state).items():
        receipt = state['dispatch']['publications'][key]
        sha = receipt.get('payload_sha256')
        if (set(receipt) == {'candidate_sha256', 'payload_sha256', 'event_id'}
                and isinstance(sha, str) and len(sha) == 64
                and all(c in '0123456789abcdef' for c in sha)):
            result[key] = dict(task_id=accepted['task_id'], receipt=dict(receipt))
    return result


def checkpoint(repo, commit):
    if len(commit) not in (40, 64) or any(c not in '0123456789abcdef' for c in commit):
        raise ValueError('invalid checkpoint commit')
    raw = _git(repo, 'show', f'{commit}:{HISTORY}')
    document = decode_shared_state_text(raw.decode())
    from .terminal_batches import observations as terminal_batches
    from .retry_retention import observations as retry_preparations
    return dict(commit=commit, path=HISTORY, blob_sha256=hashlib.sha256(raw).hexdigest(),
                tasks=terminal_tasks(document['state']), owners=terminal_owners(document['state']),
                accepted_source_candidates=accepted_source_candidates(document['state']),
                publication_cache_candidates=publication_cache_candidates(document['state']),
                terminal_batches=terminal_batches(document['state']),
                retry_preparations=retry_preparations(document['state']))


def display_source(path):
    """Legacy fallback runs out of process so large JSON never blocks dispatch."""
    state = parse_private_state(retained.read_bytes(path))
    request = state.get('in_flight_request')
    if not isinstance(request, dict):
        raise ValueError('private state has no historical request for the viewer')
    return dict(phase=state.get('phase'), stage=state.get('stage'), cycle=state.get('cycle'),
                in_flight_request={key: request.get(key) for key in ('id', 'kind', 'cycle', 'active_node', 'mode')})


def _valid_display(value):
    request = value.get('in_flight_request') if isinstance(value, dict) else None
    return (isinstance(request, dict) and type(request.get('id')) is int
        and type(request.get('cycle')) is int and type(value.get('cycle')) is int
        and request['id'] >= 0 and request['cycle'] >= 0 and value['cycle'] >= 0
        and isinstance(request.get('kind'), str) and bool(request['kind'])
        and all(request.get(key) is None or isinstance(request[key], str) for key in ('active_node', 'mode'))
        and all(value.get(key) is None or isinstance(value[key], str) for key in ('phase', 'stage')))


class Collector:
    def __init__(self, root, repo, *, wait_for_lock=False, stop_event=None):
        self.wait_for_lock = wait_for_lock
        self.stop_event = stop_event
        self.root, self.repo = Path(root).absolute(), Path(repo).resolve()
        self._checkpoint = None
        self._checkpoint_guard = threading.Lock()
        self.batch = None

    def cancelled(self):
        return self.stop_event is not None and self.stop_event.is_set()

    @contextmanager
    def retirement_lock(self):
        if self.batch is not None:
            with self.batch.critical() as acquired:
                yield acquired
        else:
            with self.direct_retirement_lock() as acquired:
                yield acquired

    @contextmanager
    def direct_retirement_lock(self):
        # Only the daemon collector queues behind protocol work. A nonblocking
        # probe can starve forever while protocol waiters hand off this lock.
        # Preparing receipts, parsing snapshots and deleting trees stay outside.
        if self.cancelled():
            yield False
            return
        with retained.regular_path(self.root / 'coordinator.lock').open('rb') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | (0 if self.wait_for_lock else fcntl.LOCK_NB))
            except BlockingIOError:
                acquired = False
            else:
                acquired = not self.cancelled()
            yield acquired

    def checkpoint_current(self, witness):
        head = _git(self.repo, 'rev-parse', 'HEAD').decode().strip()
        if head == witness['commit']:
            return True
        # Resolved task IDs never reopen. A queued cleanup may use its already
        # validated terminal checkpoint after newer history is appended, but
        # cannot use it after a rewind or replacement of that history.
        try:
            _git(self.repo, 'merge-base', '--is-ancestor', witness['commit'], head)
        except subprocess.CalledProcessError as error:
            if error.returncode == 1:
                return False
            raise
        return True

    def witness(self):
        # Preparation-only mutex: never held while queuing a final fence.
        with self._checkpoint_guard:
            head = _git(self.repo, 'rev-parse', 'HEAD').decode().strip()
            if self._checkpoint is None or self._checkpoint['commit'] != head:
                # A checkpoint can be tens of MB even in its shared-state encoding.
                # Decode outside the coordinator process/GIL, retaining small IDs.
                result = subprocess.check_output([sys.executable, '-m', 'trellis.dispatch.terminal_retention',
                    '--checkpoint', str(self.repo), '--commit', head])
                self._checkpoint = json.loads(result)
            return self._checkpoint

    def _same_task_pending(self, task_id):
        path = self.root / 'publication/pending.json'
        if not path.exists() and not path.is_symlink():
            return False
        identity = _json(path)['identity']
        if not isinstance(identity, str) or len(identity) != 64 or any(c not in '0123456789abcdef' for c in identity):
            raise ValueError('invalid pending publication identity')
        return _json(path.parent / identity / 'record.json')['task_id'] == task_id

    def _owner_pending(self, owner):
        # Publication journals belong to worker IDs, not numerically equal
        # verifier/reviewer IDs. Committed review batch recovery uses its saved
        # capture/workers and preserved handoffs, never these private inputs.
        return owner['kind'] == 'worker' and self._same_task_pending(owner['id'])

    def _display(self, path, task_id, commit, *, owner=None):
        binding = dict(task_id=task_id) if owner is None else dict(owner=owner)
        format = DISPLAY_FORMAT if owner is None else OWNER_DISPLAY_FORMAT
        target = path.with_name('display-state.json')
        if target.exists() or target.is_symlink():
            old = _json(target)
            if (not _valid_display(old) or old.get('format') != format
                    or any(old.get(key) != value for key, value in binding.items()) or old.get('checkpoint_commit') != commit):
                raise ValueError('existing private display record differs')
            return target
        if not retained._untracked(target):
            raise ValueError('private display record is Git tracked')
        prepared = _json(path.parent.parent / 'prepared.json')
        history = prepared.get('history')
        value = dict(phase=None, stage=None, cycle=history.get('cycle') if isinstance(history, dict) else None,
            in_flight_request=dict(id=prepared.get('request_id'), **{
                key: history.get(key) if isinstance(history, dict) else None
                for key in ('kind', 'cycle', 'active_node', 'mode')}))
        if not _valid_display(value):
            value = json.loads(subprocess.check_output([sys.executable, '-m', 'trellis.dispatch.terminal_retention',
                '--display-source', str(path)]))
        if not _valid_display(value):
            raise ValueError('cannot preserve meaningful private viewer history')
        atomic(target, canonical_json(dict(format=format, **binding,
            checkpoint_commit=commit, **value)))
        return target

    def retire(self, directory, observed, *, apply=False, budget=None):
        return self._retire(directory, observed, apply=apply, budget=budget)

    def retire_execution(self, directory, observed, *, apply=False, budget=None):
        """Independent receipt lets already-cleaned owners shed redundant inputs."""
        from . import execution_retention
        return self._retire(directory, observed, apply=apply, budget=budget, scope=execution_retention)

    def retire_history(self, directory, observed, *, apply=False, budget=None):
        """Copied indexes have an independent receipt from launch inputs."""
        from . import history_retention
        return self._retire(directory, observed, apply=apply, budget=budget, scope=history_retention)

    def retire_compiled_probes(self, directory, observed, *, apply=False, budget=None):
        """Independent scope for exact generated checker program files."""
        from . import compiled_probe_retention
        return self._retire(directory, observed, apply=apply, budget=budget, scope=compiled_probe_retention)

    def retire_checker_caches(self, directory, observed, *, apply=False, budget=None):
        """Derived host observations have their own restartable scope."""
        from . import checker_cache_retention
        return self._retire(directory, observed, apply=apply, budget=budget, scope=checker_cache_retention)

    def _retire(self, directory, observed, *, apply=False, budget=None, scope=None):
        if scope is not None:
            eligible_paths = scope.eligible_paths
            formats, receipt = (scope.FORMAT, scope.OWNER_FORMAT), scope.RECEIPT
        else:
            eligible_paths = retained.eligible_paths
            formats, receipt = (FORMAT, OWNER_FORMAT), RECEIPT

        def representations(path):
            return (path,) if scope is not None else (path, *retained._paths(path), retained._skip_path(path))

        if self.cancelled():
            return dict(status='cancelled', removed=0)
        root, directory = retained.owner_directory(self.root, directory)
        task_id = directory.name
        owner = retained.owner_identity(directory)
        worker = owner['kind'] == 'worker'
        binding = dict(task_id=owner['id']) if worker else dict(owner=owner)
        format = formats[0] if worker else formats[1]
        identity_field = 'task' if worker else 'identity'
        if task_id not in observed or self._owner_pending(owner):
            return dict(**binding, status='not_terminal_or_pending', removed=0)
        # Completed and never-prepared owners need no identity hashing or Git
        # decoding. Discovery preserves the same explicit input allowlist.
        if not any(eligible_paths(directory)):
            return dict(**binding, status='absent', removed=0)
        guard = retained.terminal_owner_guard(root, directory, observed)
        identity = observed[task_id]
        if isinstance(identity, _DeferredVerifierIdentity):
            identity = identity.resolve()
        witness = self.witness()
        if witness.get('owners', witness['tasks']).get(task_id) != identity:
            return dict(**binding, status='not_committed', removed=0)
        prepared = _json(directory / 'prepared.json')
        if Path(prepared['source_repo']).resolve() != self.repo:
            raise ValueError('task preparation belongs to another project')
        if not worker:
            # Verifier routing deduplicates by job identity and replaces the
            # job's ordinary request even while the original execution runs.
            # Its final request ID is not the prepared execution's identity.
            # The committed job identity/result plus process cleanup above
            # establish retirement; prepared history supplies the display ID.
            if owner['kind'] == 'review' and prepared['request_id'] != owner['id']:
                raise ValueError('private preparation belongs to another owner request')
            _git(self.repo, 'merge-base', '--is-ancestor', prepared['source_head'], witness['commit'])
        record_path = directory / receipt
        if not retained._untracked(record_path):
            raise ValueError('private retirement receipt is Git tracked')
        if record_path.exists() or record_path.is_symlink():
            record = _json(record_path)
            if (not isinstance(record, dict) or record.get('format') != format
                    or any(record.get(key) != value for key, value in binding.items()) or record.get('repo') != str(self.repo)
                    or record.get(identity_field) != identity):
                raise ValueError('private retirement receipt changed identity')
            _git(self.repo, 'merge-base', '--is-ancestor', record['checkpoint']['commit'], witness['commit'])
        else:
            entries = []
            for path in eligible_paths(directory):
                prepared = _json(path.parents[1] / 'prepared.json') if path.parent.name == 'runtime' else None
                if prepared is not None and Path(prepared['source_repo']).resolve() != self.repo:
                    raise ValueError('private preparation belongs to another project')
                # Repo replay manifests also belong to this task's prepared root.
                if Path(_json(directory / 'prepared.json')['source_repo']).resolve() != self.repo:
                    raise ValueError('task preparation belongs to another project')
                if not retained._untracked(path):
                    continue
                for selected in representations(path):
                    if not selected.exists() and not selected.is_symlink():
                        continue
                    info = retained.regular_path(selected).stat()
                    if not retained._untracked(selected):
                        continue
                    entries.append(dict(path=str(selected.relative_to(directory)),
                        signature=list(retained._signature(info)), bytes=info.st_size,
                        allocated_bytes=info.st_blocks * 512,
                        display_source=str(path.relative_to(directory)) if path.name == 'protocol_state.json' else None))
            record = dict(format=format, **binding, repo=str(self.repo), **{identity_field: identity},
                checkpoint={key: witness[key] for key in ('commit', 'path', 'blob_sha256')}, files=sorted(entries, key=lambda item: bool(item['display_source'])))
        result = dict(**binding, status='eligible', removed=0, removed_bytes=0, removed_allocated_bytes=0, skipped=[], pending=False,
            bytes=sum(item['bytes'] for item in record['files']), files=len(record['files']), allocated_bytes=sum(item['allocated_bytes'] for item in record['files']))
        if not apply or not record['files']:
            return result
        if self.cancelled():
            return dict(result, status='cancelled')
        # The recoverable intent and display records precede every unlink.
        if not record_path.exists():
            atomic(record_path, canonical_json(record))
        allowed = {str(selected.relative_to(directory)): (str(path.relative_to(directory)) if path.name == 'protocol_state.json' else None)
                   for path in eligible_paths(directory)
                   for selected in representations(path)}
        ready = []
        for item in record['files']:
            if self.cancelled():
                return dict(result, status='cancelled')
            if budget is not None and budget['files'] <= 0:
                result['pending'] = True
                break
            relative = Path(item['path'])
            if relative.is_absolute() or '..' in relative.parts:
                raise ValueError('retirement receipt path escapes its task')
            if item['path'] not in allowed:
                # Already removed paths naturally disappear from discovery.
                path = directory / item['path']
                if path.exists() or path.is_symlink():
                    raise ValueError('retirement receipt points outside its allowlist')
                continue
            path = directory / item['path']
            if not path.exists() and not path.is_symlink():
                continue
            if item['display_source'] != allowed[item['path']]:
                raise ValueError('retirement display path differs from allowlist')
            if item['display_source']:
                try:
                    self._display(directory / item['display_source'], owner['id'], record['checkpoint']['commit'],
                        **({} if worker else {'owner': owner}))
                except (ValueError, OSError, KeyError, TypeError, subprocess.CalledProcessError) as error:
                    result['skipped'].append(dict(path=item['path'], reason=str(error)))
                    continue
            ready.append((path, item))
        if not ready:
            return dict(result, status='unchanged')
        guard()
        # Queue once per terminal owner, not once per file behind long native
        # publications. Inventory and all display creation above stay off-lock;
        # retain the same final checks for every unlink, including cancellation.
        # The durable receipt makes an interrupted partial batch restartable.
        dirty_directories = set()
        try:
            with self.retirement_lock() as acquired:
                if not acquired:
                    result['status'] = 'cancelled' if self.cancelled() else 'coordinator_busy'
                    return result
                for path, item in ready:
                    if self.cancelled():
                        return dict(result, status='cancelled')
                    if budget is not None and budget['files'] <= 0:
                        result['pending'] = True
                        break
                    if not self.checkpoint_current(witness):
                        raise ValueError('Git checkpoint changed during retirement; retry against the new head')
                    if self._owner_pending(owner):
                        raise ValueError('task publication still needs recovery')
                    guard()
                    if list(retained._signature(retained.regular_path(path).stat())) != item['signature']:
                        raise ValueError('private retirement input changed')
                    if not retained._untracked(path):
                        continue
                    if self.cancelled():
                        return dict(result, status='cancelled')
                    path.unlink()
                    dirty_directories.add(path.parent)
                    if budget is not None:
                        budget['files'] -= 1
                    result['removed'] += 1
                    result['removed_bytes'] += item['bytes']
                    result['removed_allocated_bytes'] += item['allocated_bytes']
        finally:
            # Flush successful removals even if a later file fails or cleanup
            # is cancelled. Directory fsync must not extend the protocol lock.
            sync_error = None
            for parent in sorted(dirty_directories):
                try:
                    _sync_dir(parent)
                except OSError as error:
                    sync_error = sync_error or error
            if sync_error is not None:
                raise sync_error
        result['status'] = 'retired' if result['removed'] else 'unchanged'
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', type=Path)
    parser.add_argument('--repo', type=Path)
    parser.add_argument('--task', action='append', type=int)
    parser.add_argument('--owner', action='append', help='review-N or verifier-N')
    parser.add_argument('--apply', action='store_true')
    parser.add_argument('--execution-inputs', action='store_true', help='retire redundant launch and bridge inputs')
    parser.add_argument('--checkpoint', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--commit', help=argparse.SUPPRESS)
    parser.add_argument('--display-source', type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.checkpoint:
        print(json.dumps(checkpoint(args.checkpoint, args.commit)))
        return
    if args.display_source:
        print(json.dumps(display_source(args.display_source)))
        return
    if args.runtime is None:
        parser.error('--runtime is required')
    root = args.runtime / 'dispatch'
    repo = args.repo or Path(_json(args.runtime / 'runtime_metadata.json')['repo_path'])
    observed = terminal_owners(_json(args.runtime / 'protocol_state.json'))
    collector = Collector(root, repo)
    ids = ([str(id) for id in (args.task or [])] + (args.owner or [])) if args.task or args.owner else sorted(observed)
    with (root / 'build-cleanup.lock').open('a+b') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        for task_id in ids:
            try:
                operation = collector.retire_execution if args.execution_inputs else collector.retire
                value = operation(root / str(task_id), observed, apply=args.apply)
            except Exception as error:
                value = dict(task_id=task_id, status='retained', reason=str(error))
            print(json.dumps(value), flush=True)


if __name__ == '__main__':
    main()

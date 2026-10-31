"""Production bindings for the native parallel coordinator.

Providers and compilers run only in durable, isolated attempts. The coordinator
does filesystem preparation and records host facts; mathematical acceptance is
always a native ordinary transition plus source-backed checkpoint publication.
"""
from __future__ import annotations

import asyncio
import copy
from contextlib import asynccontextmanager, nullcontext
import json
import os
from pathlib import Path
import sys
import subprocess
import time

from trellis.private_state import read_private_state
from . import candidate, workspace, acceptance_receipts
from .acceptance import GENERATED
from .acceptance_queue import AcceptanceQueue, Footprint
from .coordination import Completion
from .native import NativeLedger
from .process import AttemptProcess, alive
from .publication import Publisher, PublicationChanged, atomic
from .preparation_capacity import PreparationCapacity, PreparationDeferred


class HostCheckFailure(RuntimeError):
    """Owned check process failed; its sealed input remains available to retry."""


def read(path):
    return read_private_state(path) if Path(path).name == 'protocol_state.json' else json.loads(Path(path).read_text())


async def _run_blocking(function, *args, **kwargs):
    """Join an owned thread before propagating cancellation to its caller."""
    operation = asyncio.create_task(asyncio.to_thread(function, *args, **kwargs))
    cancelled = False
    while not operation.done():
        try:
            await asyncio.shield(operation)
        except asyncio.CancelledError:
            # Cancellation cannot stop the thread. In particular, it may still
            # create a launcher, grant compiler access or commit publication.
            cancelled = True
    result = operation.result()  # A real operation failure remains visible.
    if cancelled:
        raise asyncio.CancelledError
    return result


def acceptance_identity(private):
    """Ignore request numbering, retaining every ordinary acceptance input.

    Independent launches/reviews advance sequence numbers without changing the
    reconstructed worker contract. Native publication still binds/replays the
    entire current canonical state and the freshly numbered worker response.
    """
    # Only these two headers are normalized. The remaining evidence is read
    # unchanged by canonical_json; copying it would duplicate a large state.
    context = dict(private, request_seq=0)
    if context.get('in_flight_request'):
        context['in_flight_request'] = dict(context['in_flight_request'], id=0)
    return candidate.digest(candidate.canonical_json(context))


class ProductionDriver:
    def __init__(self, runtime, *, kernel=None, process_factory=AttemptProcess, prepare_workspace=workspace.prepare):
        self.runtime = Path(runtime).resolve()
        self.metadata = read(self.runtime / 'runtime_metadata.json')
        self.repo = Path(self.metadata['repo_path']).resolve()
        self.config_path = Path(self.metadata['config_path']).resolve()
        self.preparation_capacity = PreparationCapacity(self.runtime, self.repo)
        self.native = NativeLedger(self.runtime, **({'kernel': kernel} if kernel else {}),
            materialize=self._materialize, preparation_capacity=self.preparation_capacity,
            materialize_batch=self._materialize_batch)
        self.root = self.native.root
        self.publisher = Publisher(self.runtime, self.repo)
        self.process_factory = process_factory
        self.prepare_workspace = prepare_workspace
        self.view = self.root / 'compiler-view'
        self.environment = dict(os.environ, TRELLIS_CHECK_JOBS='1')
        self._notify = lambda: None
        from .telemetry import Measurements
        self.measurements = Measurements(self.runtime)
        self.acceptance_queue = AcceptanceQueue()
        self.finalization_gate = asyncio.Lock()

    def set_notifier(self, notify):
        self._notify = notify

    def now_ms(self):
        return int(time.time() * 1000)

    def preparation_ready(self):
        capacity = getattr(self, 'preparation_capacity', None)
        return capacity is None or capacity.ready()

    def _require_preparation_capacity(self, count=1):
        capacity = getattr(self, 'preparation_capacity', None)
        if capacity is not None:
            capacity.require(count)

    def prepared_target_ready(self, directory):
        # This is scheduling eligibility only; _prepare/process adoption still
        # authenticate the original inputs and recipe before granting work.
        return (directory / 'prepared.json').exists() or self.preparation_ready()

    def worker_preparation_ready(self, task):
        directory = self.root / str(task['id'])
        marker = directory / 'prepared.json'
        if marker.exists() and read(marker).get('deferred_build_cache'):
            return (directory / 'build-cache-ready.json').exists() or self.preparation_ready()
        return self.prepared_target_ready(directory)

    def verifier_preparation_ready(self, status):
        if self.preparation_ready() or (status.get('stage') == 'Start' and status.get('request') is None):
            return True  # Starting a native cycle does not allocate a workspace.
        if any(j['status']['status'] in ('running', 'checking')
               or self.prepared_target_ready(self.root / f'verifier-{j["id"]}')
               for j in status.get('verifiers', {}).get('jobs', {}).values()
               if j['status']['status'] != 'resolved'):
            return True
        request = status.get('request')
        return (request is not None
                and self.prepared_target_ready(self.root / f'exclusive-{request["id"]}'))

    def review_preparation_ready(self, cursor):
        if self.preparation_ready():
            return True
        return (cursor is not None
            and cursor['request_id'] not in getattr(self, '_deferred_reviews', set())
            and (self.root / f'review-{cursor["request_id"]}' / 'prepared.json').exists())

    def _defer_review(self, cursor):
        self._deferred_reviews = {cursor['request_id']}

    def _prepare_fresh(self, destination, private, **kwargs):
        if not (destination / 'prepared.json').exists():
            self._require_preparation_capacity()
        return self._prepare(destination, private, **kwargs)

    def state(self):
        return read(self.runtime / 'protocol_state.json')

    def pausing(self):
        return (self.repo / '.trellis-stop-after-checkpoint').exists() or (self.runtime / 'pause_request.json').exists()

    def require_no_halt(self):
        from trellis.runtime.bridge import any_halt_marker_present
        if any_halt_marker_present(self.runtime):
            raise RuntimeError(f'halt marker present in {self.runtime}; inspect and resolve it before resuming parallel dispatch')

    def _permit(self, process, record):
        self.require_no_halt()
        process.permit(record)

    def _record_transport(self, directory, response):
        """Carry ordinary bridge retry budgets across isolated request roots.

        A completion is counted once, even if the coordinator crashes before
        the native response commit. This file records transport health only.
        """
        request = read(directory / 'runtime/protocol_state.json')['in_flight_request']
        kind = request['kind'].lower()
        sentinel = directory / 'repo/.trellis-stop-after-checkpoint'
        if sentinel.exists():
            self._pause('bridge_circuit_breaker', sentinel.read_text())
        if kind not in ('review', 'paper', 'corr', 'sound'):
            return
        private = directory / 'runtime/bridge/verifier_artifact_retry.json'
        failed = bool(read(private).get(kind)) if private.exists() else response.get('status') == 'Malformed'
        identity = str(directory.relative_to(self.root))
        from trellis.runtime.bridge import _verifier_artifact_retry_budget
        with self.native.locked():
            path = self.root / 'transport.json'
            saved = read(path) if path.exists() else dict(streaks={}, receipts={})
            digest = candidate.digest(candidate.canonical_json(response))
            if identity in saved['receipts']:
                if saved['receipts'][identity] != digest:
                    raise RuntimeError('transport completion changed after being recorded')
            else:
                saved['receipts'][identity] = digest
                streak = saved['streaks'].get(kind, 0) + 1 if failed else 0
                saved['streaks'][kind] = streak
                if streak > _verifier_artifact_retry_budget():
                    saved['pause'] = dict(kind='verifier_artifact_failure',
                        reason=f'{kind} produced no usable artifact for {streak} consecutive attempts')
                atomic(path, candidate.canonical_json(saved))
            if saved.get('pause'):
                self._pause(**saved['pause'])

    def _pause(self, kind, reason, **detail):
        from datetime import datetime, timezone
        path = self.runtime / 'pause_request.json'
        if not path.exists():
            atomic(path, candidate.canonical_json(dict(kind=kind, reason=reason,
                armed_by='parallel coordinator', armed_at=datetime.now(timezone.utc).isoformat(),
                runtime_root=str(self.runtime), detail=detail)))
        atomic(self.repo / '.trellis-stop-after-checkpoint', (reason + '\n').encode())

    def _routing(self, event):
        self.require_no_halt()
        return self.native._call(dict(action='dispatch_routing', root=str(self.runtime), event=event))

    async def observe(self, event):
        return await _run_blocking(self.native.observe, event)

    async def complete_delivery(self, task, completion):
        return await _run_blocking(self.native.complete_delivery, task, completion, self.now_ms)

    def _prepare(self, destination, private, *, previous=None, handoff_repo=None, host_cache_donor=None,
                 deferred_build_cache=None, host_only=False):
        from .telemetry import phase
        measurements = getattr(getattr(self, 'native', None), 'measurements', None)
        with phase(measurements, 'preparation.manifest'):
            marker = destination / 'base-manifest.json'
            manifest = read(marker) if marker.exists() else candidate.source_manifest(self.repo)
        # Custom preparation adapters retain their existing call contract.
        instrumentation = ({'measurements': measurements, **({'host_only': True} if host_only else {})}
                           if self.prepare_workspace is workspace.prepare else {})
        try:
            return self.prepare_workspace(self.runtime, self.repo, self.config_path, destination,
                private, expected_manifest=manifest, previous=previous, handoff_repo=handoff_repo, **instrumentation,
                **({'host_cache_donor': host_cache_donor} if host_cache_donor is not None else {}),
                **({'deferred_build_cache': deferred_build_cache} if deferred_build_cache is not None else {}))
        except workspace.PreparedInputsChanged:
            # Only an unlaunched, receipted preparation can be displaced when
            # a batch is revalidated after a crash. Never overwrite task work.
            if not (destination / 'prepared.json').exists() or any(destination.rglob('recipe.json')):
                raise
            archive = self.root / 'unused-preparations' / f'{destination.name}-{candidate.digest((destination / "prepared.json").read_bytes())}'
            archive.parent.mkdir(parents=True, exist_ok=True)
            if archive.exists():
                raise RuntimeError('unused preparation already archived; preserve both trees for recovery')
            destination.rename(archive)
            candidate._sync_dir(archive.parent)
            return self.prepare_workspace(self.runtime, self.repo, self.config_path, destination,
                private, expected_manifest=candidate.source_manifest(self.repo), previous=previous,
                handoff_repo=handoff_repo, **instrumentation,
                **({'host_cache_donor': host_cache_donor} if host_cache_donor is not None else {}),
                **({'deferred_build_cache': deferred_build_cache} if deferred_build_cache is not None else {}))

    def _materialize_batch(self, workers, *, path, committed):
        from . import batch_cache, batch_storage
        if committed:
            # Original sources/config/assignment were all captured before the
            # native event; cache hydration does not reconstruct those inputs.
            for id, worker in workers.items():
                batch_storage.adopt_capture(self.root / str(id), worker)
                del worker
            return
        if not workers:
            return
        if self.repo.stat().st_dev != self.runtime.stat().st_dev:
            return self._materialize(workers)  # Preserve ordinary cross-device package copying.
        complete = True
        identities = {}
        manifest = candidate.source_manifest(self.repo)
        config_hash = candidate.digest(self.config_path.read_bytes())
        for id in workers:
            marker = self.root / str(id) / 'prepared.json'
            if not marker.exists():
                # StoredWorkers hydrates a full immutable private state on
                # lookup. A missing capture cannot match it: defer hydration
                # to the ordinary materialization and adoption checks below.
                identities[id] = False
                complete = False
                continue
            worker = workers[id]
            expected = candidate.digest(candidate.canonical_json(dict(
                state=worker['private_state'], sources=manifest, config_sha256=config_hash)))
            identities[id] = marker.exists() and read(marker)['identity'] == expected
            records = all((marker.parent / name).exists() for name in batch_storage.CAPTURE_RECORDS)
            sealed = not marker.exists() or not read(marker).get('deferred_build_cache') or (marker.parent / 'carrier-ready.json').exists()
            if not identities[id] or not records or not sealed:
                complete = False
            del worker
        if complete:
            # Includes exact same-base batches captured by an older release.
            # Never create a new donor for an already complete preparation.
            from .batch_cache import validate, read as cache_read
            for id, worker in workers.items():
                directory = self.root / str(id)
                batch_storage.adopt_capture(directory, worker)
                ref = read(directory / 'prepared.json').get('deferred_build_cache')
                if ref and not (directory / 'build-cache-ready.json').exists():
                    cache_root = self.root / ref['path']
                    captured = cache_read(cache_root / 'capture.json')
                    if candidate.digest(candidate.canonical_json(captured)) != ref['sha256']:
                        raise candidate.CandidateError('saved batch cache receipt changed')
                    validate(cache_root, captured)
                del worker
            return
        saved = read(path)
        root = self.root / 'batch-caches' / path.parent.name / saved['base_sha256']
        binding = dict(base_sha256=saved['base_sha256'], review_id=int(path.parent.name),
                       receipt=saved['prepared']['receipt'], workers=workers.references)
        expected_path = str(root.relative_to(self.root))
        pending = []
        auxiliary_bytes = auxiliary_inodes = 0
        shared_reference = False
        for id in workers:
            marker = self.root / str(id) / 'prepared.json'
            prior = read(marker).get('deferred_build_cache') if marker.exists() else None
            shared_reference |= prior is not None
            if identities[id] and prior and prior['path'] == expected_path:
                if not (root / 'capture.json').exists():
                    raise candidate.CandidateError('saved worker lost its original shared cache')
                # Preparation publishes before these final carrier records.
                # Resume their bounded remaining allocation, not a new workspace.
                directory = marker.parent
                if not (directory / 'assignment.json').exists():
                    auxiliary_bytes += workers.references[id]['bytes'] + 4096
                    auxiliary_inodes += 1
                if not (directory / 'worker-handoffs.json').exists():
                    auxiliary_bytes += 64 << 20
                    auxiliary_inodes += 64
                if not (directory / 'carrier-ready.json').exists():
                    auxiliary_bytes += 4096
                    auxiliary_inodes += 1
            else:
                pending.append(id)
        capacity = self.preparation_capacity
        if pending:
            estimate = batch_cache.allocation(self.repo, workers, pending,
                new_capture=not (root / 'capture.json').exists(),
                workspace_bytes=capacity.workspace_bytes, workspace_inodes=capacity.workspace_inodes)
            auxiliary_bytes += estimate['bytes']
            auxiliary_inodes += estimate['inodes']
        if auxiliary_bytes or auxiliary_inodes:
            capacity.require_allocation(auxiliary_bytes, auxiliary_inodes)
        if len(workers) == 1 and not shared_reference and not root.exists() and not root.is_symlink():
            # A single new workspace can receive its optional caches directly.
            # Keep the same measured allocation admission above. Existing or
            # interrupted shared captures must retain their original donor and
            # carrier-seal recovery path, even when only one worker remains.
            self._materialize(workers)
        else:
            receipt = batch_cache.capture(root, self.repo, binding)
            ref = batch_cache.reference(root, self.root, receipt)
            self._materialize(workers, deferred_build_cache=ref)
        for id, worker in workers.items():
            batch_storage.adopt_capture(self.root / str(id), worker)
            del worker

    def _materialize(self, workers, *, committed=False, deferred_build_cache=None):
        if not committed and deferred_build_cache is None:
            self._require_preparation_capacity(sum(
                not (self.root / str(id) / 'prepared.json').exists() for id in workers))
        state = None
        for id, prepared in workers.items():
            if prepared is None:
                continue
            if committed:
                from .batch_storage import adopt_capture
                adopt_capture(self.root / str(id), prepared)
                del prepared
                continue
            # The caller holds coordinator.lock throughout this batch; copying
            # private workspaces cannot change the canonical protocol state.
            if state is None:
                state = self.state()
            task = state.get('dispatch', {}).get('tasks', {}).get(str(id), {})
            previous = task.get('retry_of')
            # During retry preparation the predecessor is in the immutable
            # prepared request's context; native reserve has not committed yet.
            if previous is None:
                prior = [t['id'] for t in state.get('dispatch', {}).get('tasks', {}).values()
                    if t.get('pending_retry') is not None and t['assignment']['scope'] == prepared['assignment']['scope']]
                previous = prior[0] if len(prior) == 1 else None
            review = prepared['assignment'].get('review')
            origin = task or state.get('dispatch', {}).get('tasks', {}).get(str(previous), {})
            review_id = review['request_id'] if review is not None else origin.get('review_id') or None
            if review_id is not None and (type(review_id) is not int or review_id < 0):
                raise ValueError('prepared reviewer request ID must be a nonnegative integer')
            self._prepare(self.root / str(id), prepared['private_state'],
                          previous=self.root / str(previous) if previous is not None else None,
                          handoff_repo=self.root / f'review-{review_id}' / 'repo' if review_id is not None else None,
                          **({'deferred_build_cache': deferred_build_cache} if deferred_build_cache else {}))
            directory = self.root / str(id)
            if deferred_build_cache and (directory / 'carrier-ready.json').exists():
                from .batch_storage import adopt_capture
                adopt_capture(directory, prepared)
                del prepared
                continue
            handoff_path = directory / 'worker-handoffs.json'
            if not handoff_path.exists() and not any(directory.rglob('recipe.json')):
                from .handoffs import worker_handoffs, relevant_tasks
                prior_tasks = relevant_tasks(state.get('dispatch', {}).get('tasks', {}).values(),
                                             prepared['assignment']['scope'].get('nodes', []), limit=8)
                atomic(handoff_path, candidate.canonical_json(
                    worker_handoffs(self.root, directory / 'repo', prior_tasks)))
            assignment_bytes = candidate.canonical_json(prepared['assignment'])
            if deferred_build_cache and (directory / 'assignment.json').exists():
                if acceptance_receipts.regular_digest(directory / 'assignment.json') != candidate.digest(assignment_bytes):
                    raise candidate.CandidateError('saved batch assignment changed before carrier seal')
            else:
                atomic(directory / 'assignment.json', assignment_bytes)
            if deferred_build_cache:
                from .batch_storage import seal_capture
                seal_capture(directory)
            # Stored batch mappings hydrate on demand. Release this carrier
            # before requesting the next, while retaining one canonical view.
            del prepared

    def _prepare_verifiers(self, status):
        changed = False
        state = None
        for id, job in status['verifiers']['jobs'].items():
            if job['status']['status'] != 'queued' or (self.root / f'verifier-{id}' / 'prepared.json').exists():
                continue
            # The caller owns the coordinator lock for this pass. Preparation
            # reads canonical state; supersession below is its only mutation.
            if state is None:
                state = self.state()
            private = self.native._call(dict(action='dispatch_prepare_verifier',
                state=state, id=int(id), repo_path=str(self.repo)))
            if private is None:
                self._supersede_verifier(int(id))
                state = None
                changed = True
                continue
            try:
                self._prepare_fresh(self.root / f'verifier-{id}', private)
            except PreparationDeferred:
                break  # Keep any earlier supersession in the returned boolean.
        return changed

    def _supersede_verifier(self, id):
        # A crash can leave a launcher awaiting permission before Launched was
        # committed. Reap it before retiring the queued job; never grant it work.
        execution = self.root / f'verifier-{id}' / 'execution'
        if (execution / 'permit.json').exists():
            raise RuntimeError('queued verifier already has provider permission')
        if (execution / 'process.json').exists():
            self.process_factory(execution).stop_and_reap(read(execution / 'process.json'))
        self._routing(dict(kind='supersede_queued', id=id,
            reason='verifier inputs or panel policy changed before launch'))

    def _snapshot(self):
        self.require_no_halt()
        with self.native.locked():
            self.native._require_recovered_publication()
            from .scheduling_status import SchedulingStatus
            if not hasattr(self, '_scheduling_status'):
                self._scheduling_status = SchedulingStatus()
            status = self._scheduling_status.read(
                lambda: self.native._call(dict(action='dispatch_control', root=str(self.runtime), event=None)),
                [self.runtime / 'protocol_state.json', self.runtime / 'runtime_metadata.json',
                 getattr(self, 'config_path', self.repo / 'trellis.config.json')],
                inputs=self._scheduling_inputs)
            if status.get('pv_revision_ready'):
                return status  # The completed generation is frozen for its revision controller.
            # Draining stops fresh work, not native retention of resolved
            # requests. Keep the exact compressed carriers and checkpoint
            # event while releasing their expanded copies before more private
            # acceptance workspaces are prepared. Native eligibility excludes
            # active/retrying tasks and frozen generations.
            if any(t.get('request_archivable', False) for t in status['tasks'].values()):
                status = self.native._call(dict(action='dispatch_archive_requests', root=str(self.runtime)))
            elif status.get('resolved_verifier_requests_retirable'):
                # Scheduling hint only: native verifies exact terminal state and
                # the committed request again. Never parse Git history per poll.
                # A tail absent from this checkpoint waits for the next one.
                head = subprocess.check_output(['git', '--no-replace-objects', '-C', str(self.repo),
                    'rev-parse', 'HEAD'], text=True).strip()
                if head != getattr(self, '_verifier_retirement_attempted_checkpoint', None):
                    status = self.native._call(dict(action='dispatch_retire_verifier_requests', root=str(self.runtime)))
                    self._verifier_retirement_attempted_checkpoint = head

            if (status.get('stage') == 'HumanGate' and not self.pausing()
                    and (self.runtime / 'human_gate_response.json').exists()):
                # Step explicitly supports HumanGate with parallel dispatch.
                # The ordinary bridge reads, validates and consumes the user's
                # stamped viewer signal; never construct a choice here.
                from trellis.runtime.kernel_cli import KernelCliError
                try:
                    result = self.native.kernel(dict(action='step', root=str(self.runtime)))
                except KernelCliError as error:
                    # Match the serial runtime's missing/stale-signal polling
                    # behavior. Keep all other native failures visible.
                    if 'missing human gate response file' not in str(error):
                        raise
                else:
                    if result.get('status') != 'ok':
                        raise RuntimeError('native human gate step returned an unexpected response')
                    status = self.native._call(dict(action='dispatch_control', root=str(self.runtime), event=None))
            if (status.get('routing_pending') or status.get('coarse_audits_pending')) and not self.pausing():
                status = self._routing(dict(kind='route', inputs={}, verifier_context_version=5,
                                            suppress_sound_review_self_wakeup=True))
            if status.get('pv_revision_ready'):
                return status
            if status.get('stage') == 'Worker' and not self.pausing():
                status = self._routing(dict(kind='adopt_worker', graph={'edges': {}}, inputs={}, base_snapshot='', now_ms=self.now_ms()))
            # Recover the reserve/materialize window for engine-routed workers.
            reserved = [id for id, task in status['tasks'].items()
                        if task['status']['status'] == 'reserved' and task['assignment']['scope']['exclusive']]
            if reserved:
                state = self.state()
                for id in reserved:
                    seed = state['dispatch'].get('exclusive_workers', {}).get(id)
                    if seed is not None:
                        try:
                            self._materialize({id: dict(private_state=seed['state'],
                                assignment=state['dispatch']['tasks'][id]['assignment'])})
                        except PreparationDeferred:
                            break  # The exact native Reserved seed remains for another tick.
            if not self.pausing():
                try:
                    superseded = self._prepare_verifiers(status)
                except PreparationDeferred:
                    superseded = False
                if superseded:
                    # Superseding the last queued job must expose its replacement
                    # before the coordinator can go idle awaiting another event.
                    status = self._routing(dict(kind='route', inputs={}, verifier_context_version=5,
                                                suppress_sound_review_self_wakeup=True))
            # Routing already returned the resulting native status. Under this
            # lock, preparing workspaces cannot change the ledger; only a
            # superseded verifier requires another authoritative load.
            if self.pausing():
                status['pausing'] = True
                status['reviewer_runnable'] = False
                status['verifiers_runnable'] = any(j['status']['status'] in ('running', 'checking')
                    for j in status['verifiers']['jobs'].values())
            try:
                self.measurements.sample(status['tasks'], native=self.native.measurements,
                    cleanup=getattr(self, '_build_cleanup', None),
                    scheduling=dict(cache_hits=self._scheduling_status.hits,
                                    native_refreshes=self._scheduling_status.misses,
                                    max_age_seconds=self._scheduling_status.seconds))
            except Exception:
                pass  # Telemetry cannot approve, reject or stop protocol work.
            from .parking import summary, retire_replaced
            # Publish the same current status view before retiring a marker;
            # parked pollers read this under this same lock, avoiding an old
            # Checking view racing a just-observed native correction/reap.
            self._host_park_tasks = status['tasks']
            for task in status['tasks'].values():
                retire_replaced(self.root, task)
                blocked = summary(self.root, task)
                if blocked is not None:
                    task['host_check_blocked'] = blocked
            return status

    def _scheduling_inputs(self):
        from .scheduling_status import source_inputs
        inputs = source_inputs(self.repo)
        if inputs is None:
            return None
        # goal_file may name a referent outside the repository source walk.
        try:
            config = read(self.config_path)
        except (OSError, ValueError):
            return None  # Cache invalidation cannot replace native diagnostics.
        if not isinstance(config, dict):
            return None
        configured = config.get('goal_file')
        goal = self.repo / ((configured.strip() if isinstance(configured, str) else '') or 'GOAL.md')
        try:
            referent = candidate.digest(goal.read_bytes())
        except FileNotFoundError:
            referent = None  # Native control diagnoses a required absent referent.
        except OSError:
            return None
        return inputs, referent

    async def snapshot(self):
        status = await _run_blocking(self._snapshot)
        self._host_park_tasks = status.get('tasks', {})
        for pulse in getattr(self, '_host_park_waiters', {}).values():
            pulse.set()
        if not status.get('pv_revision_ready'):
            # Snapshot/native lock work has finished. Never wait for private
            # package unlinking before exposing runnable work to the scheduler.
            from .storage import BackgroundCollector
            if not hasattr(self, '_build_cleanup'):
                self._build_cleanup = BackgroundCollector(self.root, repo=self.repo)
            self._build_cleanup.schedule(dict(dispatch=status))
        return status

    def set_handoff_notifier(self, notify):
        self._handoff_notifier = notify

    def _handoff(self, task_id, kind):
        notify = getattr(self, '_handoff_notifier', None)
        if notify is not None:
            notify(task_id, kind)

    async def prepare_one_retry(self):
        def proceed():
            self.require_no_halt()
            return not self.pausing()
        try:
            return bool(await _run_blocking(self.native.prepare_retries, self.repo, proceed=proceed))
        except PreparationDeferred:
            return False

    def _process(self, directory, *, compiler, seconds, command=None, payload=None, execution=None, worker_turn=None, capacity_recovery=False):
        self.require_no_halt()
        prepared_path = directory / 'dispatch-request.json'
        if prepared_path.exists():
            request = read(prepared_path)
        else:
            request = self.native._call(dict(action='dispatch_prepared_request', root=str(directory / 'runtime')))
            # Host checks supply their complete launch payload. Keep native
            # validation and transcript mount selection, without persisting a
            # second full request that their launcher never consumes.
            if not payload:
                atomic(prepared_path, candidate.canonical_json(request))
                request = read(prepared_path)
        if (directory / 'assignment.json').exists():
            request['parallel_scope'] = read(directory / 'assignment.json')['scope']
        if (directory / 'review-context.json').exists():
            request['parallel_dispatch'] = read(directory / 'review-context.json')
        if (directory / 'worker-handoffs.json').exists():
            request['worker_handoffs'] = read(directory / 'worker-handoffs.json')
        payload = payload or dict(config_path=str(directory / 'repo/trellis.config.json'),
                                 runtime_root=str(directory / 'runtime'), request=request)
        if worker_turn is not None:
            payload['worker_turn'] = worker_turn
        checker_scope = str((execution if execution is not None else directory).relative_to(self.root))
        execution = execution or directory / 'execution'
        process = self.process_factory(execution)
        environment = self.environment
        if str(request.get('kind', '')).lower() in ('worker', 'review', 'stuckmathaudit', 'stuck_math_audit'):
            from .transcripts import readonly_paths
            paths = readonly_paths(self.repo)
            if paths:
                old = environment.get('TRELLIS_SANDBOX_EXTRA_RO_PATHS', '')
                environment = dict(environment, TRELLIS_SANDBOX_EXTRA_RO_PATHS=
                    os.pathsep.join(filter(None, [old, *(str(path) for path in paths)])))
        self.require_no_halt()
        lifetime = None if seconds is None else seconds + 600
        recipe = execution / 'recipe.json'
        if seconds is None and recipe.exists():
            # Adopt legacy acceptance without changing its immutable recipe.
            # Fresh acceptance rounds use an owner-bound grant.
            lifetime = read(recipe)['checker']['lifetime']
        checker = dict(runtime=str(self.runtime), task_runtime=str(directory / 'runtime'),
                       compiler=compiler, lifetime=lifetime, scope=checker_scope)
        if capacity_recovery and (not recipe.exists() or read(recipe)['checker'].get('capacity_recovery') is True):
            # Existing launch recipes remain immutable and conservative. Only
            # new supported host checks receive task-local capacity recovery.
            checker['capacity_recovery'] = True
        if worker_turn and worker_turn.get('transport_continuation') and self.pause_requested():
            raise PreparationDeferred('worker transport continuation is paused')
        record = process.start_or_adopt(command or [sys.executable, '-m', 'trellis.runtime.bridge_cli'], payload,
            environment=environment, confinement=self.view, checker=checker)
        return process, record

    async def _wait(self, process, record, *, poll=None, host_check=False):
        while True:
            if poll is not None:
                await poll()
            pause_path = process.root / 'provider-pause.json'
            if pause_path.exists():
                paused = read(pause_path)
                if paused.get('resume_allowed') is False:
                    reason = paused['pause']['reason']
                    self._pause('parallel_infrastructure', reason, process=str(process.root))
                    raise RuntimeError(f'provider continuation unavailable; attempt and sources retained: {reason}')
                permit = dict(ordinal=paused['ordinal'], process=record)
                resumed = process.root / 'provider-resume.json'
                if not resumed.exists() or read(resumed) != permit:
                    self._pause('provider_budget', paused['pause']['reason'],
                        process=str(process.root), provider=paused['pause'].get('provider'),
                        model=paused['pause'].get('model'))
                    raise RuntimeError('provider budget exhausted; attempt retained; replenish the allowance and use trellis.dispatch resume')
            result = process.result()
            if result is None and not alive(record):
                # The launcher publishes its receipt before exit. It may have
                # completed between the first read and the liveness check.
                result = process.result()
                if result is None:
                    raise RuntimeError(f'attempt launcher exited without a result; retain its workspace: {process.root}')
            if result is not None:
                output = (acceptance_receipts.regular(process.root / 'response.json') if host_check
                          else (process.root / 'response.json').read_bytes())
                if candidate.digest(output) != result['response_sha256']:
                    raise RuntimeError('attempt response no longer matches its completion receipt')
                if result['returncode'] != 0:
                    if host_check:
                        await _run_blocking(process.stop_and_reap, record)
                        raise HostCheckFailure(f'host checker exited {result["returncode"]}; evidence: {process.root}')
                    # Provider pauses and infrastructure failures are visible
                    # operational stops, never mathematical worker failures.
                    atomic(self.root / 'operational-stop.json', candidate.canonical_json(dict(
                        process=str(process.root), returncode=result['returncode'])))
                    await _run_blocking(process.stop_and_reap, record)
                    self._pause('parallel_infrastructure', f'attempt process failed ({result["returncode"]}); inspect {process.root}',
                                process=str(process.root))
                    raise RuntimeError(f'attempt process failed ({result["returncode"]}); inspect {process.root}')
                response = json.loads(output)
                return response
            await asyncio.sleep(1)

    def _hydrate_worker_cache(self, task):
        from .batch_cache import hydrate
        directory = self.root / str(task['id'])
        marker = directory / 'prepared.json'
        if not marker.exists():
            return
        prepared = read(marker)
        if prepared.get('host_only'):
            raise candidate.CandidateError('host-only preparation cannot launch a provider')
        if not prepared.get('deferred_build_cache'):
            return  # Legacy/retry/exclusive work keeps its existing path.
        with self.native.locked():
            self.require_no_halt()
            self.native._require_recovered_publication()
            hydrate(directory, self.root, self.preparation_capacity)

    @asynccontextmanager
    async def _worker_bootstrap(self):
        # Hydration already serializes on coordinator.lock. Let its worker
        # finish the durable launch/permit handoff before another worker queues
        # a long hydration ahead of that launch event. Provider execution and
        # completion polling remain concurrent, outside this gate.
        # Lazy initialization also supports stopped/recovery driver fixtures;
        # this synchronous setup runs on the coordinator's single event loop.
        if not hasattr(self, '_worker_bootstrap_lock'):
            self._worker_bootstrap_lock = asyncio.Lock()
        async with self._worker_bootstrap_lock:
            yield

    async def launch_or_adopt(self, task):
        if task.get('worker_attempt') is not None:
            return await self._launch_submission(task)
        async with self._worker_bootstrap():
            await _run_blocking(self._hydrate_worker_cache, task)
            directory = self.root / str(task['id'])
            if not (directory / 'prepared.json').exists():
                raise RuntimeError(f'reserved task {task["id"]} has no durable workspace preparation')
            managed = task.get('execution_capacity_managed', False)
            process, record = await _run_blocking(self._start_pipelined_worker if managed else self._process,
                task if managed else directory,
                compiler=task['assignment']['compiler'], seconds=task['binding']['timeout_seconds'])
            if task['status']['status'] == 'reserved' and not managed:
                await self.observe(dict(kind='launched', task_id=task['id'], process=record, now_ms=self.now_ms()))
            elif task['status']['status'] != 'reserved' and record != task.get('process'):
                raise RuntimeError('adopted process differs from the task launch identity')
            if not (process.root / 'permit.json').exists():
                await _run_blocking(self._permit, process, record)
            self._handoff(task['id'], 'worker')
        response = await self._wait(process, record)
        await _run_blocking(process.stop_and_reap, record)
        await _run_blocking(self._record_transport, directory, response)
        bridge = read(directory / 'runtime/bridge/latest_worker.json')
        if bridge.get('response') != response:
            raise RuntimeError('worker completion and retained authoritative bridge response disagree')
        digest = await _run_blocking(candidate.seal, directory / 'repo', directory / 'candidate',
            task_id=task['id'], base_manifest=read(directory / 'base-manifest.json'),
            inputs=task['assignment']['input_hashes'], response=bridge.get('raw'),
            bridge_response=response, generated_paths=GENERATED)
        return Completion(task['id'], digest)

    def _submission_directory(self, task):
        root = self.root / str(task['id'])
        attempt = task.get('worker_attempt')
        return root / 'turns' / str(attempt['turn']) if attempt is not None else root

    def _start_pipelined_worker(self, task, **kwargs):
        # Capacity may have changed since the scheduling projection, including
        # a checked task returning for repair. Keep eligibility, launcher
        # creation and its native ownership event in one critical section;
        # no provider permit exists until after that event has committed.
        with self.native.locked():
            self.native._require_recovered_publication()
            status = self.native._call(dict(action='dispatch_control', root=str(self.runtime), event=None))
            current = status['tasks'][str(task['id'])]
            if current['status']['status'] == 'reserved' and not current.get('execution_runnable', True):
                raise PreparationDeferred('worker execution capacity is occupied; reservation retained')
            # A stale coroutine cannot create the next turn's process or debit
            # another transport's capacity. Existing adoption checks remain.
            if (current['status'] != task['status'] or current.get('worker_attempt') != task.get('worker_attempt')):
                raise PreparationDeferred('worker scheduling view changed; refresh before launch')
            process, record = self._process(self.root / str(task['id']), **kwargs)
            if current['status']['status'] == 'reserved':
                self.native._call(dict(action='dispatch_control', root=str(self.runtime),
                    event=dict(kind='launched', task_id=task['id'], process=record, now_ms=self.now_ms())))
            return process, record

    async def _launch_submission(self, task):
        from trellis.agent_wrapper import provider_clock
        from . import worker_transport
        attempt = task['worker_attempt']
        if attempt['phase']['phase'] == 'transport_ready' and task['status']['status'] == 'reserved':
            submission_root = self._submission_directory(task)
            previous = worker_transport.execution_directory(submission_root, attempt['transport_retries'] - 1)
            request_id = read(previous / 'recipe.json')['payload']['request']['id']
            status = await _run_blocking(worker_transport.prepare_wait, submission_root, attempt, request_id)
            while not worker_transport.ready(status, now_ms=self.now_ms(),
                    permit_path=previous / 'transport-resume.json'):
                self.require_no_halt()
                if self.pause_requested():
                    raise PreparationDeferred('worker transport continuation is paused')
                await asyncio.sleep(.2)
            await _run_blocking(worker_transport.wait_status, submission_root, attempt, request_id)
            self.require_no_halt()
            if self.pause_requested():
                raise PreparationDeferred('worker transport continuation is paused')
        async with self._worker_bootstrap():
            await _run_blocking(self._hydrate_worker_cache, task)
            directory = self.root / str(task['id'])
            attempt = task['worker_attempt']
            submission_root = self._submission_directory(task)
            ordinal = attempt.get('transport_retries', 0)
            execution = worker_transport.execution_directory(submission_root, ordinal)
            remaining = attempt['allowance_ms'] - attempt['consumed_ms']
            # A Submitted recovery adopts the same recipe, whose original turn
            # allowance preceded its debit. It never starts another provider.
            recipe = execution / 'recipe.json'
            if recipe.exists():
                turn = read(recipe)['payload']['worker_turn']
            else:
                turn = dict(turn=attempt['turn'], remaining_ms=remaining,
                    session=attempt.get('session'), execution_record=str(execution / 'clock'))
                if ordinal:
                    turn.update(transport_ordinal=ordinal, retain_baseline=True,
                        transport_continuation=(attempt.get('last_interruption') or {}).get('turn') == attempt['turn'])
                if attempt['turn'] > 1:
                    feedback = read(directory / 'turns' / str(attempt['turn'] - 1) / 'feedback.json')
                    if attempt['phase']['phase'] == 'transport_ready':
                        previous = worker_transport.execution_directory(submission_root, ordinal - 1)
                        feedback_hash = read(previous / 'recipe.json')['payload']['worker_turn']['feedback_sha256']
                    else:
                        feedback_hash = attempt['phase']['feedback_sha256']
                    turn.update(feedback=feedback, feedback_sha256=feedback_hash)
            managed = task.get('execution_capacity_managed', False)
            process, record = await _run_blocking(self._start_pipelined_worker if managed else self._process,
                task if managed else directory,
                compiler=task['assignment']['compiler'], seconds=None, execution=execution, worker_turn=turn)
            if task['status']['status'] == 'reserved' and not managed:
                await self.observe(dict(kind='launched', task_id=task['id'], process=record, now_ms=self.now_ms()))
            elif task['status']['status'] != 'reserved' and record != task.get('process'):
                raise RuntimeError('adopted worker turn differs from its native launcher')
            if not (execution / 'permit.json').exists():
                if attempt['phase']['phase'] == 'transport_ready' and self.pause_requested():
                    raise PreparationDeferred('worker transport continuation is paused')
                await _run_blocking(self._permit, process, record)
            self._handoff(task['id'], 'worker')
        started = attempt['phase']['phase'] not in ('ready', 'transport_ready')
        async def observe_start():
            nonlocal started
            receipt = provider_clock.read(execution / 'clock', 'started')
            if not started and receipt is not None:
                config = read(execution / 'clock/config.json')
                if config['allowance_ms'] != turn['remaining_ms']:
                    raise RuntimeError('provider clock allowance differs from its authorized turn')
                await self.observe(dict(kind='provider_started', task_id=task['id'],
                    process=record, started_at_ms=receipt['started_at_ms']))
                started = True
        response = await self._wait(process, record, poll=observe_start)
        await _run_blocking(process.stop_and_reap, record)
        if response.get('kind') != 'worker_submission' or response.get('turn') != attempt['turn']:
            raise RuntimeError('worker turn returned an unexpected delivery; retained for investigation')
        timing = provider_clock.read(execution / 'clock', 'finished')
        if not timing or response.get('provider_execution') != timing or not started:
            raise RuntimeError('worker delivery has no matching owned provider timing receipt')
        if timing['disposition'] not in ('completed', 'expired'):
            interrupted = await _run_blocking(worker_transport.interruption, execution,
                dict(task, process=record), response)
            await self.observe(dict(kind='worker_interrupted', task_id=task['id'], interruption=interrupted))
            return Completion(task['id'])
        if timing['disposition'] == 'completed' and not response.get('provider_turn_completed'):
            raise RuntimeError('provider closing/history flush failed after completion; retain its source and context')
        if timing['disposition'] == 'expired' and response.get('raw') is None:
            await self.observe(dict(kind='worker_expired', task_id=task['id'], process=record,
                turn=attempt['turn'], elapsed_ms=timing['elapsed_ms']))
            return Completion(task['id'])
        if not timing.get('thread_id'):
            raise RuntimeError('worker submission has no retained conversation; retain its source for investigation')
        session = read(directory / 'runtime/worker-session.json')
        if session['thread_id'] != timing['thread_id']:
            raise RuntimeError('worker delivery differs from its retained conversation')
        # Native acknowledgement wins before expensive source capture, regardless
        # of coordinator lock delay. The monitor already chose expiry/completion.
        if task['status']['status'] != 'submitted':
            await self.observe(dict(kind='turn_completed', task_id=task['id'], completion=dict(
                process=record, turn=attempt['turn'],
                elapsed_ms=(turn['remaining_ms'] if timing['disposition'] == 'expired' else timing['elapsed_ms']),
                exhausted=timing['disposition'] == 'expired',
                response_sha256=candidate.digest(candidate.canonical_json(response)), session=session)))
        bridge_response = dict(kind='worker', request_id=response['request_id'], cycle=response['cycle'],
            status='Malformed', outcome='Invalid', comments=response.get('delivery_error', ''),
            transport_failure=False, deterministic_rejection_reasons=[response.get('delivery_error', '')])
        identity = await _run_blocking(candidate.seal, directory / 'repo', submission_root / 'candidate',
            task_id=task['id'], base_manifest=read(directory / 'base-manifest.json'),
            inputs=task['assignment']['input_hashes'], response=response.get('raw'),
            bridge_response=bridge_response, generated_paths=GENERATED,
            submission=dict(turn=attempt['turn'], contract_sha256=attempt['contract_sha256'],
                delivery_sha256=candidate.digest(candidate.canonical_json(response))))
        return Completion(task['id'], identity)

    async def stop_and_reap(self, task):
        directory = self.root / str(task['id'])
        # Acceptance has its own launcher and channels. Reap both before the
        # native ProcessesReaped event releases the reservation.
        for path in sorted(directory.rglob('execution/process.json')):
            process = self.process_factory(path.parent)
            await _run_blocking(process.stop_and_reap, read(path))

    def _reject_candidate(self, task_id, identity, reason):
        """A deterministic scope violation uses the ordinary failure budget."""
        self.require_no_halt()
        with self.native.locked():
            self.native._require_recovered_publication()
            state = self.state()
            task = state['dispatch']['tasks'][str(task_id)]
            if task['status'] != dict(status='checking', candidate_sha256=identity):
                return
            self._reject_candidate_locked(state, task, identity, reason)

    def _stop_stale_negative(self, task, identity, source, reason):
        """Dispose an obsolete negative result through ordinary native Stale.

        Caller holds the canonical lock. This never grants positive freshness,
        rewrites a candidate, or converts malformed evidence into a retry.
        """
        from .acceptance import StaleInput, validate_overlay
        if task['status'] != dict(status='checking', candidate_sha256=identity):
            raise candidate.CandidateError('negative disposition no longer owns this submission')
        sealed = self._submission_directory(task) / 'candidate'
        record, actual = candidate.read(sealed)
        if actual != identity or record['task_id'] != task['id']:
            raise candidate.CandidateError('negative disposition candidate changed')
        try:
            validate_overlay(record, self.repo, source['input_hashes'],
                task['assignment'].get('proof_body_rebase_inputs', {}))
        except StaleInput as error:
            self.require_no_halt()
            self.native._call(dict(action='dispatch_control', root=str(self.runtime), event=dict(
                kind='attempt_stopping', task_id=task['id'], outcome=dict(kind='stale',
                reason=f'{error}; retained candidate feedback: {reason}'))))
            return True
        return False

    def _reject_candidate_locked(self, state, task, identity, reason, *,
                                 summary='Candidate violates its assigned scope'):
        """Current native negative delivery; caller owns coordinator.lock."""
        task_id = task['id']
        source = self.native.observe_sources(state, self.repo)
        if self._stop_stale_negative(task, identity, source, reason):
            return
        private = self.native._call(dict(action='dispatch_acceptance_state', state=state,
            task_id=task_id, graph=source['graph']))
        request = private['in_flight_request']
        response = dict(kind='worker', status='Ok', outcome='Invalid', request_id=request['id'],
            cycle=request['cycle'], summary=summary,
            comments=reason, deterministic_rejection_reasons=[reason])
        payload = dict(task_id=task_id, candidate_sha256=identity, base_state_sha256='',
            decided_at_ms=self.now_ms(), inputs=source['input_hashes'], proof_body_rebase_inputs=source.get('proof_body_rebase_inputs', {}), actual_scope=task['assignment']['scope'],
            graph_before=source['graph'], graph_after=source['graph'], response=response)
        if task.get('worker_attempt') is not None:
            raw = candidate.read(self._submission_directory(task) / 'candidate')[0].get('response')
            payload['submitted_outcome'] = self._submitted_outcome(raw)
        payload = self.native.prepare_publication(payload)
        self.require_no_halt()
        if task.get('worker_attempt') is not None:
            atomic(self._submission_directory(task) / 'feedback.json', candidate.canonical_json(payload))
        self.native.commit_failure(payload)

    def _prepare_acceptance(self, task_id, round_id, identity, excluded=None):
        from . import failure_report
        from .telemetry import phase
        measurements = getattr(getattr(self, 'native', None), 'measurements', None)
        with self.native.locked():
            self.native._require_recovered_publication()
            with phase(measurements, 'acceptance.state_read'):
                state = self.state()
            task = state['dispatch']['tasks'][str(task_id)]
            if task['status']['status'] != 'checking':
                return None
            with phase(measurements, 'acceptance.source_observation'):
                source = self.native.observe_sources(state, self.repo)
            private = self.native._call(dict(action='dispatch_acceptance_state', state=state,
                task_id=task_id, graph=source['graph']))
            excluded = excluded or {}
            attempts = self.root / str(task_id) / 'acceptance'
            current_key = acceptance_receipts.key(task_id, identity, task, acceptance_identity(private), source)
            previous_rounds = sorted((p for p in attempts.iterdir() if p.name.isdigit()),
                                     key=lambda p: int(p.name), reverse=True) if attempts.exists() else []
            for previous in previous_rounds:
                if excluded.get(previous.name, 0) >= 2:
                    continue
                checked = acceptance_receipts.reusable(previous, current_key)
                if checked is not None:
                    return previous, task, source['input_hashes'], checked
            directory = attempts / str(round_id)
            donor = acceptance_receipts.retry_cache_donor(directory, current_key,
                candidate.digest(self.config_path.read_bytes()), previous_rounds)
            self._prepare(directory, private, host_only=True, **({'host_cache_donor': donor} if donor is not None else {}))
            captured = dict(source['input_hashes'])
            atomic(directory / 'canonical.json', candidate.canonical_json(dict(
                acceptance_sha256=acceptance_identity(private), source=source, key=current_key,
                report_reconciliation=failure_report.reconciliation_capture(private),
                config_sha256=candidate.digest(self.config_path.read_bytes()))))
        # These are private immutable inputs; optional fact transfer need not
        # hold the canonical publication lock or delay another ready task.
        acceptance_receipts.seed_probe_caches(directory, current_key, previous_rounds)
        acceptance_receipts.seed_retry_replay(directory, current_key, donor)
        return directory, task, captured, None

    def _check_candidate_scope(self, task_id, candidate_sha256):
        # Read the current task, immutable submission and canonical statements
        # together, never amid a publication or against a different repair turn.
        with self.native.locked():
            self.native._require_recovered_publication()
            task = self.state()['dispatch']['tasks'][str(task_id)]
            if task['status'] != dict(status='checking', candidate_sha256=candidate_sha256):
                return None
            sealed = self._submission_directory(task) / 'candidate'
            record, identity = candidate.read(sealed)
            if identity != candidate_sha256:
                raise RuntimeError('candidate does not match the native Checking task')
            scope = task['assignment']['scope']
            scope_error = None
            try:
                candidate.check_scope(record, nodes=scope['nodes'], helper_prefixes=scope['helper_prefixes'],
                    shared_paths=scope['shared_writes'], exclusive=scope['exclusive'], compiler=task['assignment']['compiler'])
                if task['assignment']['task_kind'] == 'lean_closure' and record['response'] is not None:
                    # The assigned base is still present in canonical unless a
                    # conflicting external writer violated the reservation.
                    candidate.check_lean_closure(sealed, self.repo, next(iter(scope['nodes'])))
            except candidate.CandidateError as exc:
                scope_error = str(exc)
            return task, sealed, scope_error

    def _acceptance_footprint(self, task_id, identity):
        """Capture native reads before admission, then recapture after waiting."""
        from . import staged_driver
        planned = staged_driver.plan(self, task_id, identity)
        if planned is None:
            return None
        if planned['status'] == 'planned':
            return Footprint(frozenset(planned['read_nodes']), frozenset(planned['write_nodes']))
        with self.native.locked():
            self.native._require_recovered_publication()
            state = self.state()
            task = state['dispatch']['tasks'][str(task_id)]
            if task['status'] != dict(status='checking', candidate_sha256=identity):
                return None
            scope = task['assignment']['scope']
            if scope['exclusive'] or task['assignment']['task_kind'] != 'lean_closure':
                return Footprint(exclusive=True)
            sealed = self._submission_directory(task) / 'candidate'
            record, actual_identity = candidate.read(sealed)
            if actual_identity != identity:
                raise candidate.CandidateError('host reservation candidate changed')
            from .acceptance import proof_prefix_digest
            for path, change in record['changes'].items():
                if not path.startswith('Tablet/') or Path(path).suffix != '.lean':
                    return Footprint(exclusive=True)
                # Import additions remain allowed worker work. Until the
                # compiled new graph exists, reserve their host check alone.
                before = proof_prefix_digest(candidate.regular_bytes(self.repo, path))
                after = proof_prefix_digest(candidate.regular_bytes(sealed / 'files', path))
                if before is None or before != after or change['after'] is None:
                    return Footprint(exclusive=True)
            source = self.native.observe_sources(state, self.repo)
            private = self.native._call(dict(action='dispatch_acceptance_state', state=state,
                task_id=task_id, graph=source['graph']))
            observed = self.native._call(dict(action='dispatch_acceptance_footprint',
                state=private, graph=source['graph'], write_nodes=scope['nodes']))
            if not observed['supported'] or observed['exclusive']:
                return Footprint(exclusive=True)
            footprint = observed['footprint']
            return Footprint(frozenset(footprint['read_nodes']), frozenset(footprint['write_nodes']))

    @staticmethod
    def _footprint_record(footprint):
        return dict(reads=sorted(footprint.reads), writes=sorted(footprint.writes), exclusive=footprint.exclusive)

    def _restore_acceptance_reservations(self):
        """All surviving preparations become visible before any new admission."""
        with self.native.locked():
            reservations = {}
            self.native._require_recovered_publication()
            for task in (self.state().get('dispatch') or {}).get('tasks', {}).values():
                from . import parking
                parking.retire_replaced(self.root, task)
                if task['status']['status'] != 'checking':
                    continue
                path = self.root / str(task['id']) / 'acceptance-admission.json'
                saved = read(path) if path.exists() else {}
                from . import parking
                marker = path.parent / parking.MARKER
                if marker.exists() and parking._read(marker).get('state') == 'parked':
                    record = parking._read(marker)
                    if (record.get('candidate_sha256') != task['status']['candidate_sha256']
                            or saved.get('finished_at_ms') is not None
                            or record.get('admission_sha256') != candidate.digest(candidate.canonical_json(saved))):
                        raise candidate.CandidateError('parked task lost its unfinished semantic admission')
                if saved.get('version') == 1 and saved.get('candidate_sha256') == task['status']['candidate_sha256']:
                    if saved.get('finished_at_ms') is None:
                        row = saved['footprint']
                        reservations[task['id']] = Footprint(frozenset(row['reads']), frozenset(row['writes']), row['exclusive'])
                elif any((self.root / str(task['id']) / 'acceptance').glob('*/canonical.json')):
                    # Prior versions did not record semantic reservations.
                    # Preserve their running checks while blocking new work.
                    reservations[task['id']] = Footprint(exclusive=True)
            return reservations

    def _latest_acceptance_is_legacy(self, task_id, identity):
        rounds = self.root / str(task_id) / 'acceptance'
        paths = sorted((p for p in rounds.glob('*/canonical.json') if p.parent.name.isdigit()),
                       key=lambda p: int(p.parent.name), reverse=True)
        for path in paths:
            captured = read(path)
            if captured.get('key', {}).get('candidate_sha256') == identity:
                return 'staged_plan' not in captured
        return False

    async def integrate(self, task_id, candidate_sha256):
        queued_at = self.now_ms()
        adopted = task_id in self.acceptance_queue.active
        if not adopted:
            from . import failure_report
            if await _run_blocking(failure_report.eligible, self, task_id, candidate_sha256):
                # A failure report has no source/artifact writes. Queue its
                # short current transition without blocking independent proof
                # admission; feedback still cannot invalidate a live finalizer.
                self._handoff(task_id, 'checking')
                if await _run_blocking(failure_report.deliver, self, task_id, candidate_sha256,
                                       concurrent=True):
                    return
                async with self.finalization_gate:
                    if await _run_blocking(failure_report.deliver, self, task_id, candidate_sha256):
                        return
        while True:
            footprint = (self.acceptance_queue.active[task_id] if adopted else
                         await _run_blocking(self._acceptance_footprint, task_id, candidate_sha256))
            if footprint is None:
                return
            # The next await enters/restores the semantic queue. It need not
            # wait for a conflicting proof to finish before retry preparation.
            self._handoff(task_id, 'checking')
            async with self.acceptance_queue.reserve(task_id, footprint, adopt=adopted):
                if not adopted and footprint != await _run_blocking(self._acceptance_footprint, task_id, candidate_sha256):
                    continue  # Sources may change while waiting; prepare only after readmission.
                admission = dict(version=1, task_id=task_id, candidate_sha256=candidate_sha256,
                    footprint=self._footprint_record(footprint), queued_at_ms=queued_at,
                    admitted_at_ms=self.now_ms(), adopted=adopted)
                path = self.root / str(task_id) / 'acceptance-admission.json'
                from . import parking
                marker = path.parent / parking.MARKER
                if marker.exists() and read(marker).get('state') == 'parked':
                    # Restore the exact admission before authenticating the park;
                    # timestamps/footprints must not be replaced across restart.
                    admission = await _run_blocking(parking._read, path)
                else:
                    await _run_blocking(atomic, path, candidate.canonical_json(admission))
                if (admission.get('candidate_sha256') != candidate_sha256
                        or admission.get('footprint') != self._footprint_record(footprint)
                        or admission.get('finished_at_ms') is not None):
                    raise candidate.CandidateError('parked acceptance reservation changed')
                parked = await _run_blocking(parking.load, self, task_id, candidate_sha256, admission)
                # Cancellation retains the durable admission for restart. The
                # in-memory semantic lease ends after all publication threads
                # join; an isolated check child cannot publish on its own.
                # Native process/resource ownership still lasts until reaping.
                legacy_adoption = adopted and await _run_blocking(
                    self._latest_acceptance_is_legacy, task_id, candidate_sha256)
                while True:
                    if parked is not None:
                        action = await parking.wait(self, parked)
                        if action == 'retired':
                            result = None
                            break
                        parked = None
                    try:
                        from .resource_outcome import MemoryRequeue
                        result = await self._integrate_reserved(task_id, candidate_sha256,
                            **({'legacy_adoption': True} if legacy_adoption else {}))
                        break
                    except MemoryRequeue as retry:
                        # All finalization locks have unwound. Keep only the
                        # task's semantic lease while independent work proceeds.
                        await asyncio.sleep(retry.seconds)
                    except parking.HostCheckBlocked as blocked:
                        # staged.run has released its finalization mutex here;
                        # this coroutine still owns its semantic reservation.
                        parked = await _run_blocking(parking.persist, self, blocked, admission)
                admission['finished_at_ms'] = self.now_ms()
                if result == 'readmit':
                    admission['readmitting'] = True
                await _run_blocking(atomic, path, candidate.canonical_json(admission))
                if result == 'readmit':
                    # Historical checks may already overlap. They can finish
                    # under restored reservations, but a new owner stage must
                    # release that old lease and join current conflict admission.
                    adopted = False
                    continue
                return

    async def _integrate_reserved(self, task_id, candidate_sha256, *, legacy_adoption=False):
        checked = await _run_blocking(self._check_candidate_scope, task_id, candidate_sha256)
        if checked is None:
            return
        task, sealed, scope_error = checked
        identity = candidate_sha256
        if scope_error is not None:
            await _run_blocking(self._reject_candidate, task_id, identity, scope_error)
            return
        from . import staged_driver
        if not legacy_adoption:
            staged = await staged_driver.run(self, task_id, identity)
            if staged == 'readmit':
                return 'readmit'
            if staged:
                return
        # Exclusive legacy checks consume verifier approvals throughout the
        # full normalizer. Queue verifier delivery before capturing that state,
        # while verifier providers may continue computing independently.
        admitted = self.acceptance_queue.active.get(task_id)
        exclusive = task['assignment']['scope']['exclusive'] or bool(admitted and admitted.exclusive)
        async with self.finalization_gate if exclusive else nullcontext():
            return await self._integrate_legacy(task_id, identity, task, sealed,
                                                finalization_locked=exclusive)

    async def _integrate_legacy(self, task_id, identity, task, sealed, *, finalization_locked=False):
        from . import staged_driver, freshness_recheck
        attempts = self.root / str(task_id) / 'acceptance'
        round_id = max((int(p.name) for p in attempts.iterdir() if p.name.isdigit()), default=1) if attempts.exists() else 1
        # Resume a surviving or completed acceptance process before creating
        # another. A changed canonical base gets a new retained round.
        rejected = {}
        while True:
            result = None
            directory = attempts / str(round_id)
            if ((directory / 'canonical.json').exists()
                    and read(directory / 'canonical.json').get('key', {}).get('candidate_sha256', identity) != identity):
                round_id += 1
                continue
            if freshness_recheck.superseded(directory, identity):
                round_id += 1
                continue
            if (directory / 'retry-authorized.json').exists():
                round_id += 1
                continue
            if ((directory / 'canonical.json').exists()
                    and staged_driver.completed_modeling_fallback(directory)):
                # This round only observed the pre-overlay baseline. Keep its
                # evidence and create a normal gate with its own immutable recipe.
                round_id += 1
                continue
            if not (directory / 'canonical.json').exists():
                try:
                    prepared = await _run_blocking(self._prepare_acceptance, task_id, round_id, identity, rejected)
                except candidate.CandidateError as exc:
                    await self.observe(dict(kind='attempt_stopping', task_id=task_id, outcome=dict(kind='stale', reason=str(exc))))
                    return
                if prepared is None:
                    return
                directory, task, captured, result = prepared
            else:
                captured = read(directory / 'canonical.json')['source']['input_hashes']
            if result is None:
                payload = dict(config_path=str(directory / 'repo/trellis.config.json'), runtime_root=str(directory / 'runtime'),
                               candidate=str(sealed), captured_inputs=captured,
                               prepared_config_sha256=read(directory / 'prepared.json').get('prepared_config_sha256'),
                               proof_body_rebase_inputs=task['assignment'].get('proof_body_rebase_inputs', {}))
                process, process_id = await _run_blocking(self._process, directory, compiler='managed',
                    seconds=None, command=[sys.executable, '-m', 'trellis.dispatch.acceptance'], payload=payload,
                    capacity_recovery=task.get('worker_attempt') is not None)
                if not (process.root / 'permit.json').exists():
                    await _run_blocking(self._permit, process, process_id)
                try:
                    result = await self._wait(process, process_id,
                        **({'host_check': True} if task.get('worker_attempt') is not None else {}))
                except HostCheckFailure as exc:
                    if await self._handle_check_failure(task_id, identity, directory, str(exc)):
                        return
                    round_id += 1
                    continue
                await _run_blocking(process.stop_and_reap, process_id)
            if result.get('stale_reason'):
                await self.observe(dict(kind='attempt_stopping', task_id=task_id,
                    outcome=dict(kind='stale', reason=result['stale_reason'])))
                return
            await _run_blocking(acceptance_receipts.complete, directory, result)
            plan = await _run_blocking(staged_driver.plan, self, task_id, identity)
            if plan is None:
                return
            if plan['status'] == 'planned':
                return 'readmit'  # Reuse this completion only after fresh semantic admission.
            async with nullcontext() if finalization_locked else self.finalization_gate:
                finished = await _run_blocking(self._publish, task_id, identity, directory, result)
                if finished is None:
                    from . import publication_refresh
                    refresh = await _run_blocking(publication_refresh.prepare, self.runtime, directory, result)
                    finished = await _run_blocking(self._publish, task_id, identity, directory, result,
                                                  refresh=refresh)
                if not finished and await _run_blocking(
                        freshness_recheck.authorize, self, task_id, identity, directory, result):
                    round_id += 1
                    continue
            if finished:
                return
            # Unauthenticated, source-changing or repeated conflicts still stop.
            # The bounded context-only recovery above retains the original check.
            await _run_blocking(self._pause, 'parallel_infrastructure',
                'Host acceptance inputs changed despite its reservation; checked work retained',
                task_id=task_id, evidence=str(directory))
            raise RuntimeError('host acceptance freshness conflict; retained completion requires reconciliation')

    async def _handle_check_failure(self, task_id, identity, directory, reason):
        outcome = await _run_blocking(self._retry_check, task_id, identity, directory, reason)
        if outcome and outcome.get('rejected'):
            return True
        if outcome and outcome.get('retry_after_seconds'):
            from .resource_outcome import MemoryRequeue
            raise MemoryRequeue(outcome['retry_after_seconds'])
        return False

    def _retry_check(self, task_id, identity, directory, reason):
        with self.native.locked():
            self.native._require_recovered_publication()
            state = self.state()
            task = state['dispatch']['tasks'][str(task_id)]
            attempt = task.get('worker_attempt')
            if not attempt or task['status'] != dict(status='checking', candidate_sha256=identity):
                raise RuntimeError('host check interruption no longer matches the current submission')
            from trellis.checker.resource_failure import CAPACITY_MARKER, MEMORY_MARKER
            memory_failure = False
            if any((directory / 'runtime' / name).exists() for name in (CAPACITY_MARKER, MEMORY_MARKER)):
                from . import resource_outcome
                outcome = resource_outcome.handle(self, state, task, directory)
                if 'infrastructure_reason' not in outcome:
                    return outcome
                reason = outcome['infrastructure_reason']
                memory_failure = True
            path = directory / 'infrastructure-failure.json'
            event = (read(path)['event'] if path.exists() else dict(kind='check_retry', task_id=task_id,
                candidate_sha256=identity, retry=attempt['infrastructure_retries'] + 1))
            if not path.exists():
                atomic(path, candidate.canonical_json(dict(event=event, reason=reason)))
            if event['retry'] > attempt['policy']['max_infrastructure_retries']:
                if memory_failure:
                    # A drained compiler failure has no successful staged
                    # completion for parking/recovery grants to authenticate.
                    self._pause('parallel_infrastructure', reason + '; host retry allowance exhausted; submission retained',
                                task_id=task_id, evidence=str(directory))
                    raise RuntimeError(reason + '; host retry allowance exhausted; submission retained')
                grant = attempt.get('infrastructure_recovery_grant')
                if grant is None or (attempt['infrastructure_retries'] >= grant['retry'] and event['retry'] > grant['retry']):
                    from .parking import block
                    block(self, task, directory, str(reason) + '; host retry allowance exhausted', event)
                from .recovery import retry_event
                try:
                    event = retry_event(self, task, directory, event)
                except (OSError, ValueError, KeyError, TypeError, candidate.CandidateError) as error:
                    reason = (f'host recovery grant unavailable: {error}; submission retained'
                              if attempt.get('infrastructure_recovery_grant') is not None else
                              'host check retry allowance exhausted; submission retained')
                    self._pause('parallel_infrastructure', reason, task_id=task_id, evidence=str(directory))
                    raise RuntimeError(reason) from None
            self.native._call(dict(action='dispatch_control', root=str(self.runtime), event=event))
            atomic(directory / 'retry-authorized.json', candidate.canonical_json(dict(
                kind='bounded_infrastructure_retry', event=event, reason=reason)))

    @staticmethod
    def _completed_output(directory, key, *, staged=False):
        if staged:
            from . import staged_acceptance, staged_driver
            completed = staged_acceptance.reusable_finalizer(directory, key)
            if completed is None or completed.get('status') != 'checked':
                return None
            return staged_driver._flatten(completed)
        return acceptance_receipts.reusable(directory, key)

    def _rebase_acceptance(self, directory, captured, current_key, private, source, checked, task, *, staged=False):
        """Reuse authenticated computation; native code owns semantic freshness."""
        if 'key' not in captured or not private.get('worker_execution'):
            return None
        if candidate.digest(self.config_path.read_bytes()) != captured.get('config_sha256'):
            return None
        # Original receipts remain exact. A reviewed producer can donate its
        # checked result, but only the current native gates authorize adoption.
        previous_key = dict(captured['key'], acceptance_sha256=current_key['acceptance_sha256'],
                            input_hashes=current_key['input_hashes'])
        original_private = read(directory / 'runtime/protocol_state.json')
        original_source = captured['source']
        compatibility = None
        if previous_key != current_key:
            if staged:
                from . import staged_publication
                compatibility = staged_publication.compatible(directory, captured, current_key,
                    private, checked, source)
            else:
                from . import acceptance_compatibility
                compatibility = acceptance_compatibility.prepare(directory, captured, current_key,
                    original_private, private, checked, source)
            if compatibility is None:
                return None
            original_source = compatibility['original_source']
        elif self._completed_output(directory, captured['key'], staged=staged) != checked:
            raise candidate.CandidateError('host completion changed before semantic rebase')
        result = self.native._call(dict(action=('dispatch_rebase_staged_finalization' if staged
                                               else 'dispatch_rebase_checked_response'),
            original_private=original_private, current_private=private,
            original_source=original_source, current_source=source,
            checked_response=checked['response'], write_nodes=task['assignment']['scope']['nodes']))
        diagnostic = dict(version=1, current_key_sha256=candidate.digest(candidate.canonical_json(current_key)),
            status=result['status'], **({'reason': result['reason']} if result.get('reason') else {}))
        attempts = result.get('attempts')
        if (isinstance(attempts, list) and len(attempts) <= 2 and all(
                isinstance(row, dict) and row.get('lane') in ('ordinary_context', 'scoped_response_checks', 'selected_response_checks')
                and isinstance(row.get('reason'), str) and len(row['reason']) <= 1024
                and isinstance(row.get('truncated'), bool) for row in attempts)):
            diagnostic['attempts'] = [{key: row[key] for key in ('lane', 'reason', 'truncated')}
                                      for row in attempts]
        if result['status'] == 'integrity_error':
            diagnostic['operand'] = result.get('operand')
            diagnostic['integrity'] = result.get('integrity')
        if compatibility is not None:
            diagnostic['producer_compatibility'] = compatibility['decision']
        acceptance_receipts.write_once(directory / 'semantic-rebase' /
            (candidate.digest(candidate.canonical_json(diagnostic)) + '.json'), diagnostic)
        if result['status'] == 'integrity_error':
            raise candidate.CandidateError('native certificate integrity failure; retain original completion without another finalizer: '
                                           + str(result.get('reason', 'inconsistent retained certificate')))
        if result['status'] == 'cold':
            return None
        if result['status'] != 'rebased':
            raise RuntimeError('native semantic rebase returned an unexpected result')
        return dict(checked, response=result['response'])

    def _publication_basis(self):
        """Optional exact-byte shortcut; no state/result authority survives drift."""
        try:
            # These are streamed hashes, never mtime/inode cache identities.
            state = acceptance_receipts.regular_digest(self.runtime / 'protocol_state.json')
            config = acceptance_receipts.regular_digest(self.config_path)
        except (AttributeError, OSError, candidate.CandidateError):
            return None  # Nonstandard/legacy carriers retain full reconstruction.
        # Capture the native filesystem inventory itself, including absences
        # via full membership, global inputs and host authority files. Do not
        # reconstruct it from scoped inputs or synthetic missing-TeX rows.
        inputs = self.native._call(dict(action='dispatch_verifier_inputs', repo_path=str(self.repo)))
        return dict(state=state, config=config, inputs=inputs)

    def _publication_basis_current(self, basis):
        if basis is None:
            return False
        try:
            if (acceptance_receipts.regular_digest(self.runtime / 'protocol_state.json') != basis['state']
                    or acceptance_receipts.regular_digest(self.config_path) != basis['config']):
                return False
        except (OSError, candidate.CandidateError):
            return False
        return self.native._call(dict(action='dispatch_verifier_inputs', repo_path=str(self.repo))) == basis['inputs']

    def _publish(self, task_id, identity, directory, checked, *, refresh=None, staged=False):
        # finalization_gate remains owned by the caller throughout. Only the
        # immutable packet work releases coordinator.lock, never roll-forward.
        packet = None
        basis = None
        prepared = 0
        try:
            while True:
                result = self._publish_locked(task_id, identity, directory, checked,
                    refresh=refresh, staged=staged, packet=packet, basis=basis)
                if not isinstance(result, dict):
                    if prepared and result is not True:
                        raise RuntimeError('publication inputs changed during packet preparation; checked work retained')
                    return result
                if prepared >= 3:
                    raise RuntimeError('publication preparation did not stabilize; checked work retained')
                if packet is not None:
                    self.publisher.discard(packet)
                packet = None
                prepared += 1
                basis = result.pop('canonical_basis')
                try:
                    packet = self.publisher.stage(task_id, identity, directory / 'repo', **result)
                except PublicationChanged:
                    # No pointer/source was written. Recapture current native
                    # inputs, preserving the original completed proof check.
                    continue
        finally:
            if packet is not None:
                self.publisher.discard(packet)

    def _publish_locked(self, task_id, identity, directory, checked, *, refresh=None, staged=False, packet=None, basis=None):
        with self.publisher.locked():
            self.require_no_halt()
            self.native._require_recovered_publication()
            if packet is not None and self._publication_basis_current(basis):
                # Exact canonical bytes are unchanged since the first native
                # preparation. Avoid reparsing/authenticating the full state a
                # second time; ordinary native commit still owns acceptance.
                self.publisher.activate(packet, kernel_event=packet.record['kernel_event'])
                self.publisher.recover(self.native.commit_publication,
                    publication_receipt=self.native.publication_receipt)
                return True
            state = self.state()
            task = state['dispatch']['tasks'][str(task_id)]
            if task['status']['status'] != 'checking':
                return True
            captured = read(directory / 'canonical.json')
            source = self.native.observe_sources(state, self.repo)
            original_response = checked.get('response') or {}
            if ('key' in captured and original_response.get('status') == 'Ok'
                    and original_response.get('outcome') in ('Invalid', 'Stuck')
                    and captured['source']['input_hashes'] != source['input_hashes']):
                # Obsolete negative work needs its native stale disposition,
                # not a current acceptance context for its old source packet.
                # Authenticate the completed check before using any of it.
                if self._completed_output(directory, captured['key'], staged=staged) != checked:
                    raise candidate.CandidateError('negative completion changed before stale disposition')
                if self._stop_stale_negative(task, identity, source,
                        original_response.get('comments') or original_response.get('summary')
                        or 'host negative result retained'):
                    return True
            private = self.native._call(dict(action='dispatch_acceptance_state', state=state,
                task_id=task_id, graph=source['graph']))
            current_key = acceptance_receipts.key(task_id, identity, task, acceptance_identity(private), source)
            original_checked = checked
            rebased = False
            if (captured.get('acceptance_sha256') != acceptance_identity(private)
                    or captured['source']['input_hashes'] != source['input_hashes']
                    or ('key' in captured and captured['key'] != current_key)):
                replacement = self._rebase_acceptance(directory, captured, current_key, private, source, checked, task,
                                                      **({'staged': True} if staged else {}))
                if replacement is not None:
                    checked, rebased = replacement, True
            if not rebased and (captured.get('acceptance_sha256') != acceptance_identity(private)
                    or captured['source']['input_hashes'] != source['input_hashes']
                    or ('key' in captured and captured['key'] != current_key)):
                if staged:
                    # The caller retains immutable owner facts and refreshes
                    # only the current finalization. A stage must never enter
                    # the old full-acceptance refresh or start a new proof round.
                    return False
                context_only = ('key' in captured and dict(captured['key'],
                    acceptance_sha256=current_key['acceptance_sha256']) == current_key)
                if (refresh is None and context_only and checked.get('final_environment')
                        and checked.get('acceptance_context_sha256')
                        and checked.get('response', {}).get('outcome') == 'Valid'):
                    # Capture current compiler environments outside this lock.
                    # The second entry reconstructs state and inputs afresh.
                    return None
                refreshed = None
                if checked.get('response', {}).get('outcome') == 'Stuck':
                    # This reconstructs only a current negative report. Its
                    # own exact input/receipt guards also permit producer drift.
                    from . import publication_refresh
                    refreshed = publication_refresh.refresh_stuck(directory, checked,
                        current_key=current_key, private=private, repo=self.repo, kernel=self.native.kernel)
                if refresh is None and context_only and checked.get('response', {}).get('outcome') == 'Valid':
                    refreshed = dict(status='cold', reason=(
                        'original final artifact environment unavailable' if not checked.get('final_environment')
                        else 'original acceptance context binding unavailable'))
                if refresh is not None and context_only:
                    from . import publication_refresh
                    refreshed = publication_refresh.apply(directory, refresh, current_key=current_key,
                        request=private['in_flight_request'], repo=self.repo, kernel=self.native.kernel)
                if refreshed is not None and refreshed.get('status') == 'refreshed':
                    checked = refreshed['output']
                else:
                    if refreshed is not None:
                        diagnostic = dict(version=1, current_key_sha256=candidate.digest(
                            candidate.canonical_json(current_key)), reason=refreshed.get('reason', 'refresh unavailable'))
                        acceptance_receipts.write_once(directory / 'refresh-misses' /
                            (candidate.digest(candidate.canonical_json(diagnostic)) + '.json'), diagnostic)
                    acceptance_receipts.reject(directory, task_id=task_id, identity=identity, task=task,
                        captured=captured, source=source, private=private, native=self.native, now_ms=self.now_ms(), state=state)
                    return False
            if 'key' in captured and self._completed_output(directory, captured['key'], staged=staged) != original_checked:
                raise candidate.CandidateError('acceptance completion or checked workspace changed before publication')
            response = dict(checked['response'], request_id=private['in_flight_request']['id'],
                            cycle=private['in_flight_request']['cycle'])
            valid = response.get('status') == 'Ok' and response.get('outcome') == 'Valid'
            proposed = self.native.observe_sources(
                read(directory / 'runtime/protocol_state.json') if rebased else state,
                directory / 'repo', response=original_checked['response'] if rebased else response) if valid else source
            # Deleted owned nodes keep their old edges for conservative
            # reservation checks; the ordinary engine validates actual deletion.
            graph_after = copy.deepcopy(proposed['graph'])
            # Exact-source administrative reuse retains the checked structural
            # delta, including helpers, against the identical canonical base.
            # The older scoped proof rebase merges independent publications.
            exact_base = (captured['source']['input_hashes'] == source['input_hashes']
                          and captured['source']['graph'] == source['graph']
                          and captured['source'].get('authority_inputs') == source.get('authority_inputs'))
            if rebased and valid and not staged and private['phase'] != 'Modeling' and not exact_base:
                writes = set(task['assignment']['scope']['nodes'])
                original_edges = captured['source']['graph']['edges']
                proposed_edges = proposed['graph']['edges']
                if any(original_edges.get(node) != proposed_edges.get(node)
                       for node in set(original_edges) | set(proposed_edges) if node not in writes):
                    raise candidate.CandidateError('checked graph changed outside its publication scope')
                graph_after = copy.deepcopy(source['graph'])
                for node in writes:
                    graph_after['edges'][node] = proposed_edges[node]
            for node, edges in source['graph']['edges'].items():
                graph_after['edges'].setdefault(node, edges)
            actual = copy.deepcopy(task['assignment']['scope'])
            changes = candidate.read(self._submission_directory(task) / 'candidate')[0]['changes']
            actual['nodes'] = sorted(set(actual['nodes']) | {Path(p).stem for p in changes
                if p.startswith('Tablet/') and Path(p).suffix in ('.lean', '.tex') and p not in GENERATED})
            payload = dict(task_id=task_id, candidate_sha256=identity, base_state_sha256='', decided_at_ms=self.now_ms(),
                inputs=source['input_hashes'], proof_body_rebase_inputs=source.get('proof_body_rebase_inputs', {}), actual_scope=actual, graph_before=source['graph'],
                graph_after=graph_after, response=response)
            if task.get('worker_attempt') is not None:
                raw = candidate.read(self._submission_directory(task) / 'candidate')[0].get('response')
                payload['submitted_outcome'] = self._submitted_outcome(raw)
            if (task['assignment'].get('task_kind') == 'lean_closure'
                    and response.get('outcome') == 'Stuck'):
                from .failure_report import request_review
                raw = candidate.read(self._submission_directory(task) / 'candidate')[0].get('response')
                request_review(payload, task, raw)
            if staged and valid:
                from . import staged_publication
                fresh_inputs = staged_publication.fresh_basis(directory, captured, current_key, private,
                    source, original_checked, task)
                if fresh_inputs is not None:
                    payload['fresh_checked_inputs'] = fresh_inputs
            if not valid and self._stop_stale_negative(task, identity, source,
                    response.get('comments') or response.get('summary') or 'host negative result retained'):
                return True
            payload = self.native.prepare_publication(payload)
            self.require_no_halt()
            if valid:
                preparation = dict(expected_manifest=source['manifest'], kernel_event=payload,
                    artifact_source=directory if 'key' in captured else None,
                    **({'checked_baseline': read(directory / 'base-manifest.json')} if rebased else {}))
                if packet is None or not self.publisher.matches(packet, task_id, identity, directory / 'repo',
                        expected_manifest=source['manifest'], kernel_event=payload):
                    preparation['canonical_basis'] = self._publication_basis()
                    return preparation
                self.publisher.activate(packet, kernel_event=payload)
                self.publisher.recover(self.native.commit_publication,
                    publication_receipt=self.native.publication_receipt)
            else:
                if task.get('worker_attempt') is not None:
                    atomic(self._submission_directory(task) / 'feedback.json', candidate.canonical_json(payload))
                self.native.commit_failure(payload)
            return True

    @staticmethod
    def _submitted_outcome(raw):
        return {'valid': 'Valid', 'invalid': 'Invalid', 'stuck': 'Stuck',
                'needs_restructure': 'NeedsRestructure', 'target_false_under_model': 'TargetFalseUnderModel'}.get(
                    (raw or {}).get('outcome'))

    def _reviewer_handoff_repo(self, state):
        origin = (state.get('dispatch') or {}).get('worker_handoff')
        if origin is None:
            return None
        # Bind advisory files to the native producer before stripping dispatch.
        # Neither the newest reviewer nor a canonical namesake is a substitute.
        comments = state['in_flight_request'].get('reviewer_comments') or ''
        digest = candidate.digest(json.dumps(comments, ensure_ascii=False,
            separators=(',', ':')).encode())
        review_id = origin['review_id']
        if (type(review_id) is not int or review_id < 0
                or digest != origin['comments_sha256']):
            raise RuntimeError('advisory handoff differs from its native reviewer origin')
        return self.root / f'review-{review_id}' / 'repo'

    def _start_review(self):
        self.require_no_halt()
        with self.native.locked():
            # An ordinary drain can arm after the coordinator's snapshot.
            # Its pause and reviewer admission share this native lock.
            if self.pausing():
                return None
            status = self.native._start_review_locked()
            cursor = status['reviewer']
            if cursor is None:
                return None
            from .review_snapshot_files import read as read_review_snapshot
            private = read_review_snapshot(self.root / 'reviews' / str(cursor['request_id']) / 'state.json')
            directory = self.root / f'review-{cursor["request_id"]}'
            handoff_repo = self._reviewer_handoff_repo(private)
            private['dispatch'] = None
            try:
                self._prepare_fresh(directory, private,
                    **({'handoff_repo': handoff_repo} if handoff_repo is not None else {}))
            except PreparationDeferred:
                self._defer_review(cursor)
                return None
            context = dict(profiles=status['config']['profiles'], available_slots=status['available_slots'],
                deferred_verification_advisory=(private.get('in_flight_request') or {}).get(
                    'deferred_verification_advisory', []),
                lightweight_attempts_per_node=status.get('lightweight_attempts_per_node'),
                lightweight_attempts_by_node=status.get('lightweight_attempts_by_node', {}),
                sol_worker_slots=status['config'].get('sol_worker_slots'),
                available_sol_slots=status.get('available_sol_slots', status['available_slots']),
                compiler_slots=status['config']['compiler_slots'], verifier_slots=status['config']['verifier_slots'],
                draining=status['draining'], helper_prefix=f'P{cursor["request_id"]}_T<assignment-number>_',
                dispatch_receipts=sorted(status.get('receipts', {}).values(), key=lambda receipt: receipt['review_id'])[-10:],
                tasks=[dict(id=t['id'], profile=t['assignment']['profile'], task_kind=t['assignment']['task_kind'], scope=t['assignment']['scope'],
                    status=t['status'], retry_of=t.get('retry_of'), report=t.get('report'), retry_refusal=t.get('retry_refusal'))
                    for t in status['tasks'].values() if t['status']['status'] != 'resolved'
                    or t['id'] >= max((j['id'] for j in status['tasks'].values()), default=0) - 20])
            if status['config'].get('heavy_worker_slots') is not None:
                context.pop('sol_worker_slots', None)
                context.pop('available_sol_slots', None)
                context.update(heavy_worker_slots=status['config']['heavy_worker_slots'],
                    available_heavy_slots=status['available_heavy_slots'])
            jobs = sorted(status.get('verifiers', {}).get('jobs', {}).values(), key=lambda job: job['id'])
            recent_resolved = {job['id'] for job in
                               [job for job in jobs if job['status']['status'] == 'resolved'][-10:]}
            context['verifier_jobs'] = []
            for job in jobs:
                state = job['status']['status']
                if state == 'resolved' and job['id'] not in recent_resolved:
                    continue
                summary = dict(id=job['id'], lane=job['identity']['lane'], status=state,
                    required=job['required'], reviewer_requested=job['reviewer_requested'])
                for field in ('nodes', 'targets', 'audit_goals'):
                    if field in job:
                        summary[field] = job[field]
                if state == 'resolved':
                    summary['stale_reason'] = job['status'].get('stale_reason')
                context['verifier_jobs'].append(summary)
            from .parking import summary
            context['host_check_blocked'] = {str(t['id']): blocked for t in status['tasks'].values()
                if (blocked := summary(self.root, t)) is not None}
            context['checker_measurements'] = self.measurements.value
            path = directory / 'review-context.json'
            if not path.exists():
                from .closure_history import by_node, untried_candidates
                context['closure_attempts_by_node'] = by_node(status['tasks'])
                context['closure_candidates_without_recorded_launches'] = untried_candidates(
                    (private.get('in_flight_request') or {}).get('lean_closure_candidates', []),
                    context['closure_attempts_by_node'])
                from .handoffs import worker_handoffs, relevant_tasks, reviewer_rationales
                context['recent_reviewer_rationales'] = reviewer_rationales(
                    self.root, context['dispatch_receipts'])
                handoffs = worker_handoffs(self.root, directory / 'repo',
                    relevant_tasks(status['tasks'].values(), private.get('live', {}).get('open_nodes', []), recent=20))
                if handoffs:
                    context['worker_handoffs'] = handoffs
                atomic(path, candidate.canonical_json(context))
            return cursor

    async def start_review(self):
        return await _run_blocking(self._start_review)

    async def review(self, cursor, *, prepared=False):
        # Only the fresh start in this coordinator invocation has already
        # prepared. Adoption still checks the receipt and completes any
        # preparation interrupted before launch.
        if not prepared:
            if await _run_blocking(self._start_review) is None:
                return
        directory = self.root / f'review-{cursor["request_id"]}'
        try:
            process, record = await _run_blocking(self._process, directory, compiler='managed', seconds=14400)
        except PreparationDeferred:
            self._defer_review(cursor)
            return
        if not (process.root / 'permit.json').exists():
            await _run_blocking(self._permit, process, record)
        response = await self._wait(process, record)
        await _run_blocking(process.stop_and_reap, record)
        await _run_blocking(self._record_transport, directory, response)
        try:
            # Batch admission can add coarse goals to acceptance state. It must
            # respect the same gate as verifier delivery and canonical publication.
            async with self.finalization_gate:
                await _run_blocking(self._dispatch_review, cursor, response)
        except PreparationDeferred:
            self._defer_review(cursor)
            return  # Retained response and saved uncommitted batch are revalidated later.

    def _dispatch_review(self, cursor, response):
        self.require_no_halt()
        if self.pausing():
            return  # Retain the finished response for revalidation after resume.
        review_id = cursor['request_id']
        if response.get('status') == 'Malformed':
            with self.native.locked():
                return self._routing(dict(kind='review_action', review_id=review_id,
                    response_sha256=candidate.digest(candidate.canonical_json(response)), response=response))
        if response.get('kind') != 'parallel_review':
            raise RuntimeError('reviewer produced an unrecognized host response; preserve its diagnostic')
        if response.get('action') is not None:
            with self.native.locked():
                self.native._require_recovered_publication()
                return self._routing(dict(kind='review_action', review_id=review_id,
                    response_sha256=candidate.digest(candidate.canonical_json(response)), response=response['action']))
        # Capture current observations before batch preparation; both actions
        # are under one shared lock in NativeLedger.dispatch_current_batch.
        return self.native.dispatch_current_batch(review_id, response, self.repo)

    async def run_verifiers(self):
        runners = {}
        parked = {}
        try:
            while True:
                status = await self.snapshot()
                exclusive = (status.get('pv_revision_ready') or
                    status.get('stage') not in ('Reviewer', 'Worker', 'HumanGate', 'Complete'))
                if exclusive:
                    jobs = status.get('verifiers', {}).get('jobs', {})
                    if (any(j['status']['status'] in ('running', 'checking') for j in jobs.values())
                            or any(jobs.get(str(id), {}).get('status', {}).get('status') != 'resolved'
                                   for id in runners)):
                        raise RuntimeError('exclusive routing crossed live verifier attempts')
                    if runners:
                        # Native routing can resolve a finished job or supersede
                        # a queued one before its Python waiter returns. Join
                        # those waiters without cancelling them or hiding errors.
                        # _start_verifier rechecks the job under the native lock
                        # before creating a process or granting permission.
                        await asyncio.gather(*runners.values())
                        runners.clear()
                        self._notify()
                        continue  # Re-read the boundary after the wait.
                    if status.get('pv_revision_ready'):
                        return
                    if status.get('pausing'):
                        return parked
                    await self._exclusive(status)
                    return
                jobs = status['verifiers']['jobs'].values()
                parked = {id: sealed for id, sealed in parked.items()
                    if status.get('pausing') and
                    status['verifiers']['jobs'].get(str(id), {}).get('status') == sealed}
                occupied = {j['id'] for j in jobs if j['status']['status'] in ('running', 'checking')}
                # A coroutine may have started but not committed Launched yet.
                # Count it too; otherwise another queue pass could overfill.
                slots = status['config']['verifier_slots'] - len(occupied | runners.keys())
                launch = [j for j in jobs if j['id'] in occupied and j['id'] not in runners
                          and j['id'] not in parked]
                launch += sorted((j for j in jobs if not status.get('pausing') and j['status']['status'] == 'queued' and j['id'] not in runners and (self.preparation_ready() or self.prepared_target_ready(self.root / f'verifier-{j["id"]}'))),
                    key=lambda j: (not j['required'], not j['reviewer_requested'], j['id']))[:max(0, slots)]
                for job in launch:
                    runners[job['id']] = asyncio.create_task(self._verifier(job))
                if not runners:
                    return parked
                done, _ = await asyncio.wait(runners.values(), timeout=30, return_when=asyncio.FIRST_COMPLETED)
                for id in [id for id, task in runners.items() if task in done]:
                    sealed = runners.pop(id).result()
                    if sealed is not None:
                        parked[id] = sealed
                if done:
                    self._notify()
        finally:
            # Stopping the coordinator parks durable attempts for adoption.
            # Cancelling a waiter neither approves a result nor frees a slot.
            for task in runners.values():
                task.cancel()
            await asyncio.gather(*runners.values(), return_exceptions=True)

    async def _route(self, event):
        def commit():
            with self.native.locked():
                self.native._require_recovered_publication()
                status = self._routing(event)
                if not status.get('pv_revision_ready') and not self.pausing():
                    try:
                        self._prepare_verifiers(status)
                    except PreparationDeferred:
                        pass  # Event is committed; reconcile only preparation on a later snapshot.
                return status
        return await _run_blocking(commit)

    def _start_verifier(self, id):
        with self.native.locked():
            self.native._require_recovered_publication()
            state = self.state()
            job = state['dispatch']['verifiers']['jobs'][str(id)]
            if job['status']['status'] == 'resolved':
                return None
            if job['status']['status'] == 'queued' and self.pausing():
                return None
            directory = self.root / f'verifier-{id}'
            if job['status']['status'] == 'queued':
                private = self.native._call(dict(action='dispatch_prepare_verifier',
                    state=state, id=id, repo_path=str(self.repo)))
                if private is None:
                    self._supersede_verifier(id)
                    return None
                if not (directory / 'prepared.json').exists():
                    try:
                        self._prepare_fresh(directory, private)
                    except PreparationDeferred:
                        return None
            try:
                process, record = self._process(directory, compiler=job['identity']['compiler'], seconds=14400)
            except PreparationDeferred:
                return None
            if job['status']['status'] == 'queued':
                self._routing(dict(kind='verifier_launched', id=id, process=record, now_ms=self.now_ms()))
            elif job['status']['status'] == 'running' and job['status']['process'] != record:
                raise RuntimeError('verifier process differs from its durable launch identity')
            if not (process.root / 'permit.json').exists():
                self._permit(process, record)
            return job, process, record

    async def _verifier(self, job):
        started = await _run_blocking(self._start_verifier, job['id'])
        if started is None:
            return
        job, process, record = started
        directory = self.root / f'verifier-{job["id"]}'
        response = await self._wait(process, record)
        await _run_blocking(self._record_transport, directory, response)
        # Candidate sealing is idempotent and also authenticates a retained
        # Checking result against its native digest on restart.
        status = await self._route(dict(kind='verifier_candidate', id=job['id'], response=response))
        sealed = status['verifiers']['jobs'][str(job['id'])]['status']
        await _run_blocking(process.stop_and_reap, record)
        while True:
            if self.pausing():
                # This is only a host drain acknowledgment. Leave the native
                # result Checking for ordinary publication after resume.
                return sealed
            # Approval changes are inputs to current consumer/guard checks.
            # Keep provider work concurrent, but publish its verdict between
            # finalizations so a new approval cannot invalidate one in flight.
            async with self.finalization_gate:
                if self.pausing():
                    return sealed
                status = await self._route(dict(kind='verifier_finished', id=job['id'], inputs={}, response=response))
            if status['verifiers']['jobs'][str(job['id'])]['status']['status'] == 'resolved':
                return
            await asyncio.sleep(1)  # A global transition is waiting for worker cleanup.

    def _prepare_exclusive(self, status):
        request = status.get('request')
        id = request['id'] if request else self.state()['request_seq'] + 1
        directory = self.root / f'exclusive-{id}'
        with self.native.locked():
            inputs = self.native._call(dict(action='dispatch_verifier_inputs', repo_path=str(self.repo)))
            if request:
                state = self.state()
                handoff_repo = (self._reviewer_handoff_repo(state)
                    if request['kind'] == 'StuckMathAudit' else None)
                state['dispatch'] = None
                self._prepare_fresh(directory, state, handoff_repo=handoff_repo)
                marker = directory / 'inputs.json'
                if not marker.exists():
                    atomic(marker, candidate.canonical_json(inputs))
                inputs = read(marker)
                handoff_path = directory / 'worker-handoffs.json'
                if (request['kind'] == 'StuckMathAudit' and not handoff_path.exists()
                        and not (directory / 'execution/recipe.json').exists()):
                    from .handoffs import worker_handoffs
                    tasks = list(status['tasks'].values())
                    cutoff = max((task['id'] for task in tasks), default=0) - 20
                    handoffs = worker_handoffs(self.root, directory / 'repo',
                        [task for task in tasks if task['id'] >= cutoff])
                    atomic(handoff_path, candidate.canonical_json(handoffs))
        return directory, request, inputs

    async def _exclusive(self, status):
        try:
            directory, request, inputs = await _run_blocking(self._prepare_exclusive, status)
        except PreparationDeferred:
            return
        response = None
        if request:
            try:
                process, record = await _run_blocking(self._process, directory, compiler='managed', seconds=14400)
            except PreparationDeferred:
                return
            if not (process.root / 'permit.json').exists():
                await _run_blocking(self._permit, process, record)
            response = await self._wait(process, record)
            await _run_blocking(process.stop_and_reap, record)
            await _run_blocking(self._record_transport, directory, response)
            if response.get('kind') == 'stuck_math_audit':
                from .enrichment import enrich
                response = await _run_blocking(enrich, directory / 'runtime', response)
        def publish():
            from .artifacts import publish_probes
            with self.native.locked():
                self.native._require_recovered_publication()
                current = self.native._call(dict(action='dispatch_verifier_inputs', repo_path=str(self.repo)))
                if current != inputs:
                    raise RuntimeError('exclusive operation inputs changed; retain its response for review')
                publish_probes(directory, self.repo, response)
                return self._routing(dict(kind='exclusive_step', inputs={}, expected_inputs=inputs, response=response))
        await _run_blocking(publish)
        self._notify()

    async def recover_publication(self):
        def recover():
            with self.publisher.locked():
                result = self.publisher.recover(self.native.commit_publication,
                    publication_receipt=self.native.publication_receipt)
                # Coordinator.run calls this before exposing any integrations.
                self.publisher.discard_unused_preparations()
                return result
        result = await _run_blocking(recover)
        self.acceptance_queue.restore(await _run_blocking(self._restore_acceptance_reservations))
        return result

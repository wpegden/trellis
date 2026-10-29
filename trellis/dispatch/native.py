"""Native kernel adapter for the asynchronous coordinator's durable facts."""
from contextlib import contextmanager, nullcontext
import fcntl
import json
import os
from pathlib import Path
import time

from trellis.runtime.kernel_cli import run_kernel_cli
from .candidate import CandidateError, canonical_json, digest, regular_bytes
from .review import evidence_paths
from .publication import atomic
from . import batch_storage, review_snapshot_files
from .preparation_capacity import PreparationDeferred
from .telemetry import NativeMeasurements, phase


class NativeLedger:
    def __init__(self, runtime, *, kernel=run_kernel_cli, materialize=None, preparation_capacity=None, materialize_batch=None):
        self.runtime = Path(runtime).resolve()
        self.root = self.runtime / 'dispatch'
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.kernel = kernel
        self.materialize = materialize
        self.materialize_batch = materialize_batch
        self.preparation_capacity = preparation_capacity
        self._source_cache = {}
        self._no_retry_snapshot = None
        self.measurements = NativeMeasurements()

    @contextmanager
    def locked(self):
        with (self.root / 'coordinator.lock').open('a+b') as stream:
            wait = self.measurements.begin('lock_wait')
            acquired = False
            try:
                fcntl.flock(stream, fcntl.LOCK_EX)
                acquired = True
            finally:
                self.measurements.end(wait, ok=acquired)
            held = self.measurements.begin('lock_held')
            try:
                yield
            finally:
                try:
                    fcntl.flock(stream, fcntl.LOCK_UN)
                finally:
                    self.measurements.end(held)

    @contextmanager
    def own_coordinator(self):
        """One orchestration loop per run; unrelated task launchers survive it."""
        with (self.root / 'owner.lock').open('a+b') as stream:
            try:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise RuntimeError('this run already has a parallel coordinator') from None
            try:
                yield
            finally:
                fcntl.flock(stream, fcntl.LOCK_UN)

    def _call(self, payload):
        ticket = self.measurements.begin('rpc', payload.get('action'))
        control = None
        if payload.get('action') == 'dispatch_control':
            event = payload.get('event')
            kind = 'read' if event is None else event.get('kind') if isinstance(event, dict) else None
            control = self.measurements.begin('control', kind)
        ok = False
        try:
            response = self.kernel(payload)
            if response.get('status') != 'dispatch_ok':
                raise RuntimeError('native dispatch returned an unexpected response')
            output = response['output']
            ok = True
            return output
        finally:
            self.measurements.end(control, ok=ok)
            self.measurements.end(ticket, ok=ok)

    def _require_recovered_publication(self):
        # A pending pointer is already a durable decision to publish. Neither
        # a timeout nor a new reviewer may race its roll-forward after a failed
        # callback released the file lock. Recovery owns this boundary until
        # both canonical sources and the kernel receipt agree again.
        if (self.root / 'publication/pending.json').exists():
            raise RuntimeError('finish the pending candidate publication before reading or changing dispatch state')

    def commit_publication(self, task_id, candidate_sha256, payload):
        """Publisher callback; its caller already holds coordinator.lock.

        The kernel verifies the pending journal and canonical source hashes,
        performs source-backed closure issuance, and commits through the usual
        checkpoint transaction. An exact replay returns the durable receipt.
        """
        if payload.get('task_id') != task_id or payload.get('candidate_sha256') != candidate_sha256:
            raise RuntimeError('publication payload belongs to a different task or candidate')
        return self._call(dict(action='dispatch_commit_publication', root=str(self.runtime), payload=payload))

    def publication_receipt(self, task_id, candidate_sha256, payload):
        if payload.get('task_id') != task_id or payload.get('candidate_sha256') != candidate_sha256:
            raise RuntimeError('publication receipt query belongs to a different task or candidate')
        return self._call(dict(action='dispatch_publication_receipt', root=str(self.runtime),
            payload=payload))

    def prepare_publication(self, payload):
        """Pure native acceptance replay under the caller's publication lock."""
        self._require_recovered_publication()
        return self._call(dict(action='dispatch_prepare_publication', root=str(self.runtime), payload=payload))

    def commit_failure(self, payload):
        """Record a checked non-Valid outcome under the caller's shared lock."""
        self._require_recovered_publication()
        return self._call(dict(action='dispatch_worker_failed', root=str(self.runtime), payload=payload))

    def observe_sources(self, state, repo, *, response=None):
        # Host-owned cache, discarded across restart/deployment. Invalidate on
        # every source/global input, dependency or closure-evidence change.
        shared = self._call(dict(action='dispatch_verifier_inputs', repo_path=str(repo)))
        with phase(self.measurements, 'source_observation.key'):
            identity = digest(canonical_json(dict(inputs=shared, state={k: state.get(k) for k in
                ('phase', 'live', 'deps', 'local_closure_records')}, response=response)))
        self.last_source_observation = dict(key_sha256=identity, cache_hit=identity in self._source_cache,
            entries=len(self._source_cache), repo=str(repo))
        if identity in self._source_cache:
            return self._source_cache[identity]
        view = self._call(dict(action='dispatch_observe_sources', state=state,
                              repo_path=str(repo), response=response))
        if shared != self._call(dict(action='dispatch_verifier_inputs', repo_path=str(repo))):
            raise RuntimeError('source inputs changed during authoritative dependency observation')
        view['input_hashes'].update({p: h for p, h in shared.items()
            if not p.startswith(('Tablet/', 'reference/', '.trellis/'))})
        # Host authority carriers are freshness inputs, not source paths a
        # worker may seal or overlay. Keep the candidate namespace unchanged.
        view['authority_inputs'] = {p: h for p, h in shared.items() if p.startswith('.trellis/')}
        if len(self._source_cache) >= 16:
            self._source_cache.clear()
        self._source_cache[identity] = view
        return view

    def prepare_retries(self, repo, *, now_ms=None, proceed=None):
        """Commit at most one ordinary retry preparation before fresh review.

        A saved preparation survives the reserve/materialize crash window.
        A changed current state requires fresh native legality and input checks.
        The coordinator launches/adopts the committed work before calling again.
        """
        now_ms = int(time.time() * 1000) if now_ms is None else now_ms
        with self.locked():
            self._require_recovered_publication()
            if proceed is not None and not proceed():
                return []
            state_path = self.runtime / 'protocol_state.json'
            # This is only a negative scheduling cache. No candidate, input,
            # legality or acceptance result is cached. Every published state
            # has a new inode; in-place operator changes also invalidate it.
            with state_path.open() as stream:
                st = os.fstat(stream.fileno())
                signature = (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns)
                if signature == self._no_retry_snapshot:
                    return []
                state = json.load(stream)
            ready = [int(id) for id, t in state['dispatch']['tasks'].items()
                     if t.get('pending_retry') is not None and t['status']['status'] == 'resolved']
            self._no_retry_snapshot = signature if not ready else None
            prepared = []
            storage_deferred = False
            for index, predecessor in enumerate(ready):
                if proceed is not None and not proceed():
                    return prepared
                # The first preparation uses the state just read under this
                # lock. After a capacity miss, recheck the next predecessor.
                if index:
                    state = json.loads(state_path.read_text())
                base = digest(canonical_json(state))
                path = self.root / 'retries' / str(predecessor) / 'prepared.json'
                saved = json.loads(path.read_text()) if path.exists() else None
                retained = (saved is not None and saved['base_sha256'] == base
                    and (saved['prepared']['task_id'] is None
                         or (self.root / str(saved['prepared']['task_id']) / 'prepared.json').exists()))
                admission = (self.preparation_capacity.admitted(0 if retained else 1)
                             if self.preparation_capacity is not None else nullcontext())
                try:
                    with admission:
                        source = self.observe_sources(state, repo)
                        if saved is not None and saved['base_sha256'] == base:
                            retry = saved['prepared']
                        else:
                            retry = self._call(dict(action='dispatch_prepare_retry', state=state,
                                predecessor=predecessor, graph=source['graph'], inputs=source['input_hashes'],
                                proof_body_rebase_inputs=source.get('proof_body_rebase_inputs', {}),
                                base_snapshot=digest(canonical_json(source['manifest'])), now_ms=now_ms))
                            if retry is None:
                                continue  # Keep pending retries until capacity is available.
                            atomic(path, canonical_json(dict(base_sha256=base, prepared=retry)))
                        if self.materialize is not None and retry['task_id'] is not None:
                            self.materialize({str(retry['task_id']): retry['worker']})
                        if proceed is not None and not proceed():
                            return prepared  # Retain the saved preparation for resume.
                        self._call(dict(action='dispatch_control', root=str(self.runtime), event=retry['event']))
                        prepared.append(retry)
                        break
                except PreparationDeferred:
                    # Another predecessor may already have an authenticated,
                    # fully materialized preparation (or a saved refusal).
                    storage_deferred = True
                    continue
            if storage_deferred and not prepared:
                raise PreparationDeferred("retry preparations are waiting for storage")
            if not prepared:
                self._no_retry_snapshot = signature  # Slot release publishes a new state.
            return prepared

    def snapshot(self):
        with self.locked():
            self._require_recovered_publication()
            return self._call(dict(action='dispatch_control', root=str(self.runtime), event=None))

    def complete_delivery(self, launched, completion, now_ms):
        """Arbitrate delivery against current status without a scheduling sweep."""
        with self.locked():
            self._require_recovered_publication()
            if completion.candidate_sha256 is None and completion.failure is None:
                return None  # Expiry or transport continuation already has a native observation.
            current = self._call(dict(action='dispatch_control', root=str(self.runtime), event=None))
            task = current['tasks'][str(launched['id'])]
            if task['status']['status'] not in ('reserved', 'running', 'submitted'):
                return current  # Another completion or stopping outcome won.
            attempt = task.get('worker_attempt')
            previous = launched.get('worker_attempt')
            if (attempt is None) != (previous is None) or (attempt is not None and
                    (attempt['turn'], attempt['contract_sha256'], attempt.get('transport_retries', 0)) !=
                    (previous['turn'], previous['contract_sha256'], previous.get('transport_retries', 0))):
                return current  # A delayed callback belongs to an earlier turn.
            if completion.failure is not None:
                event = dict(kind='attempt_stopping', task_id=task['id'], outcome=completion.failure)
            else:
                now = now_ms()  # Lock acquisition/native reads may cross the deadline.
                if attempt is None and task.get('deadline_ms') is not None and now >= task['deadline_ms']:
                    if task['status']['status'] != 'running':
                        return current
                    event = dict(kind='attempt_stopping', task_id=task['id'], outcome={'kind': 'timed_out'})
                else:
                    event = dict(kind='candidate_ready', task_id=task['id'],
                                 candidate_sha256=completion.candidate_sha256, now_ms=now)
            return self._call(dict(action='dispatch_control', root=str(self.runtime), event=event))

    def observe(self, event):
        with self.locked():
            self._require_recovered_publication()
            if event.get('kind') in ('attempt_stopping', 'candidate_ready', 'provider_started', 'turn_completed', 'worker_expired', 'worker_interrupted'):
                current = self._call(dict(action='dispatch_control', root=str(self.runtime), event=None))
                task = current['tasks'].get(str(event['task_id']))
                if (task and event.get('kind') == 'attempt_stopping'
                        and event.get('outcome', {}).get('kind') == 'timed_out'
                        and task['status']['status'] != 'running'):
                    # CandidateReady may have won since the scheduler snapshot.
                    # Historical timeout events still replay; a fresh worker
                    # expiry cannot discard a candidate already being checked.
                    return current
                if task and task['status']['status'] in ('stopping', 'resolved'):
                    # Deadline, completion and acceptance can race. The first
                    # durable outcome wins; a late notification cannot replace
                    # it or turn an ordinary race into a coordinator halt.
                    return current
            return self._call(dict(action='dispatch_control', root=str(self.runtime), event=event))

    def start_review(self):
        with self.locked():
            return self._start_review_locked()['reviewer']

    def _start_review_locked(self):
        """Start/adopt and capture inputs; caller holds locked() through preparation.

        Return this operation's status so its caller need not reload canonical
        history just to construct the review context under the same lock.
        """
        self._require_recovered_publication()
        status = self._call(dict(action='dispatch_control', root=str(self.runtime), event=None))
        if status['reviewer'] is not None:
            # A crash may have occurred after the durable start but before
            # the immutable input was written. No provider can launch
            # without that file. Capture now; retain the earlier event
            # cursor so intervening events still get a follow-up review.
            path = self.root / 'reviews' / str(status['reviewer']['request_id']) / 'state.json'
            if review_snapshot_files.location(path) is None:
                recovery = path.with_name('state.git.json')
                if recovery.exists() or recovery.is_symlink():
                    # A deliberate rewind may revisit a retired review. Its
                    # exact original input wins over today's canonical state.
                    from .review_snapshots import restore, _read
                    repo = _read(self.runtime / 'runtime_metadata.json')['repo_path']
                    captured = restore(repo, _read(recovery))
                    if captured['dispatch']['reviewer']['request_id'] != status['reviewer']['request_id']:
                        raise ValueError('recovered snapshot belongs to another review')
                    review_snapshot_files.capture(path, canonical_json(captured))
                else:
                    review_snapshot_files.capture(path, self.runtime / 'protocol_state.json')
            return status
        if not status['reviewer_runnable']:
            # Publication or verifier delivery can consume the scheduling
            # opportunity before this start acquires the canonical lock.
            return status
        # The control response carries the current request under this same
        # lock. Only the no-request case needs canonical request_seq.
        request = status.get('request')
        if request:
            identity = request['id']
        else:
            state = json.loads((self.runtime / 'protocol_state.json').read_text())
            request = state.get('in_flight_request')
            identity = request['id'] if request else state['request_seq'] + 1
        status = self._call(dict(action='dispatch_control', root=str(self.runtime),
                                 event=dict(kind='review_started', request_id=identity)))
        # Immutable reviewer state remains separate from the canonical
        # state that may advance while this provider is reasoning.
        path = self.root / 'reviews' / str(identity) / 'state.json'
        review_snapshot_files.capture(path, self.runtime / 'protocol_state.json')
        return status

    def dispatch_batch(self, review_id, raw_response, assignments, graph, *, now_ms=None):
        """Called after host observation, under the same lock as publication.

        Prepare/reserve against current canonical state. A saved batch makes
        restart after reservation replayable without repeating the reviewer.
        """
        with self.locked():
            return self._dispatch_batch(review_id, raw_response, assignments, graph, now_ms=now_ms)

    def dispatch_current_batch(self, review_id, response, repo):
        with self.locked():
            self._require_recovered_publication()
            state = json.loads((self.runtime / 'protocol_state.json').read_text())
            count = sum(not item.get('preparation_error') for item in response['assignments'])
            if self.materialize_batch is not None and count:
                count = 1  # Bound initial native objects; exact batch allocation follows preparation.
            committed = str(review_id) in state['dispatch'].get('receipts', {})
            path = self.root / 'reviews' / str(review_id) / 'batch.json'
            if committed:
                # Authenticate the saved original receipt before any current-source observation.
                return self._dispatch_batch(review_id, response, [], {}, state=state, repo=repo)
            if not committed and path.exists():
                saved = json.loads(path.read_text())
                if saved['base_sha256'] == digest(canonical_json(state)):
                    if self.materialize_batch is not None and saved.get('build_cache_mode') == 1:
                        # Native objects already exist; the callback prices only
                        # remaining carrier/cache allocation, including zero.
                        count = 0
                    else:
                        count = sum(not (self.root / str(id) / 'prepared.json').exists()
                                    for id in saved['prepared']['workers'])
            admission = (self.preparation_capacity.admitted(count)
                         if self.preparation_capacity is not None and not committed else nullcontext())
            with admission:
                source = self.observe_sources(state, repo)
                assignments = []
                default_profile = next(iter(state['dispatch']['config']['profiles']))
                for ordinal, item in enumerate(response['assignments'], 1):
                    bad = item.get('preparation_error')
                    inputs, evidence = source['input_hashes'], []
                    if not bad and item.get('evidence_paths'):
                        try:
                            evidence = evidence_paths(item['evidence_paths'])
                            inputs = dict(inputs)
                            for path in evidence:
                                content = regular_bytes(repo, path)
                                inputs[path] = 'absent' if content is None else digest(content)
                        except (OSError, ValueError, CandidateError) as exc:
                            bad = str(exc)
                            evidence = []
                    assignments.append(dict(review=item.get('review') or {}, preparation_error=bad,
                        options=dict(assignment_id=item['id'], profile=default_profile if bad else item['profile'],
                            timeout_seconds=None if bad else item.get('timeout_seconds'),
                            compiler='managed' if bad else item['compiler'], task_kind='ordinary' if bad else item['task_kind'],
                            # IDs are labels and may contain hyphens; array positions
                            # give bounded, disjoint prefixes legal for Tablet nodes.
                            helper_prefix=f'P{review_id}_T{ordinal}_',
                            base_snapshot=digest(canonical_json(source['manifest'])), input_hashes=inputs,
                            **({'evidence_paths': evidence} if evidence else {}),
                            proof_body_rebase_inputs=source.get('proof_body_rebase_inputs', {}))))
                return self._dispatch_batch(review_id, response, assignments, source['graph'], state=state, repo=repo)

    def _dispatch_batch(self, review_id, raw_response, assignments, graph, *, now_ms=None, state=None, repo=None):
        response_hash = digest(canonical_json(raw_response))
        now_ms = int(time.time() * 1000) if now_ms is None else now_ms
        # Caller holds coordinator.lock, including observation and source copy.
        self._require_recovered_publication()
        path = self.root / 'reviews' / str(review_id) / 'batch.json'
        object_root = path.parent / 'workers'
        if state is None:
            state = json.loads((self.runtime / 'protocol_state.json').read_text())
        state_hash = digest(canonical_json(state))
        if str(review_id) in state['dispatch'].get('receipts', {}) and not path.exists():
            raise RuntimeError('committed dispatch batch lost its original capture receipt')
        if path.exists():
            saved = json.loads(path.read_text())
            from .terminal_batches import FORMAT as TERMINAL_BATCH_FORMAT, acknowledge
            if saved.get('format') == TERMINAL_BATCH_FORMAT:
                return acknowledge(saved, state, review_id, response_hash)
            prepared = saved['prepared']
            cache_mode = saved.get('build_cache_mode')
            if cache_mode not in (None, 1):
                raise RuntimeError('unsupported batch cache preparation mode')
            if prepared['receipt']['response_sha256'] != response_hash:
                raise RuntimeError('review ID already has a different prepared response')
            committed = state['dispatch']['receipts'].get(str(review_id))
            if committed is not None:
                if committed != prepared['receipt']:
                    raise RuntimeError('prepared dispatch receipt differs from the committed receipt')
                if self.materialize_batch is not None and cache_mode == 1:
                    self.materialize_batch(batch_storage.workers(prepared, object_root), path=path, committed=True)
                elif self.materialize is not None:
                    self.materialize(batch_storage.workers(prepared, object_root), committed=True)
                return prepared
            if saved['base_sha256'] != state_hash:
                # Nothing was reserved. Re-run ordinary legality against
                # current state; never replay stale prepared scopes merely
                # because a response file survived a coordinator crash.
                prepared = None
        else:
            prepared = None
            cache_mode = None
        if prepared is None:
            prepared = self._call(dict(action='dispatch_prepare_batch', state=state,
                assignments=assignments, graph=graph, review_id=review_id,
                response_sha256=response_hash, now_ms=now_ms,
                **({'storage': dict(repo_path=str(repo), object_root=str(object_root))} if repo is not None else {})))
            cache_mode = (1 if self.materialize_batch is not None
                          and prepared.get('format') == batch_storage.FORMAT else None)
            atomic(path, canonical_json(dict(base_sha256=state_hash, prepared=prepared,
                **({'build_cache_mode': cache_mode} if cache_mode is not None else {}))))
        # Capture source bytes before the reservation is committed. A
        # restart cannot reconstruct an old task's full source snapshot
        # from canonical after another task has published in the meantime.
        if self.materialize_batch is not None and cache_mode == 1:
            self.materialize_batch(batch_storage.workers(prepared, object_root), path=path, committed=False)
        elif self.materialize is not None:
            self.materialize(batch_storage.workers(prepared, object_root))
        self._call(dict(action='dispatch_control', root=str(self.runtime), event=prepared['event']))
        # Return retained private states for workspace creation. This is
        # idempotent after a crash between reservation and materialization.
        return prepared

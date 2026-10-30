"""Discard private caches after native resolution and process cleanup."""
from collections import deque
import fcntl
import json
import logging
import os
from pathlib import Path
import shutil
import threading
import time

from .candidate import canonical_json
from .publication import atomic
from .process import alive


def _numbered_directories(parent):
    if parent.is_symlink():
        raise ValueError('execution history cannot traverse a symlink')
    children = []
    for child in sorted(parent.glob('*')):
        if child.is_symlink():
            raise ValueError('execution history cannot traverse a symlink')
        if child.name.isdigit():
            if not child.is_dir():
                raise ValueError('numbered execution history entry is not a directory')
            children.append(child)
    return children


def _attempt_directories(directory):
    """Enumerate owned layouts explicitly; never recursively discover repos."""
    directory = Path(directory)
    acceptance = _numbered_directories(directory / 'acceptance')
    paths = [directory, *acceptance, *_numbered_directories(directory / 'turns')]
    for turn in _numbered_directories(directory / 'turns'):
        paths.extend(_numbered_directories(turn / 'transports'))
    for acceptance_round in acceptance:
        paths.extend(_numbered_directories(acceptance_round / 'finalization'))
    return paths


def execution_directories(directory):
    """Legacy execution, worker turns, acceptance and staged finalization."""
    return [attempt / 'execution' for attempt in _attempt_directories(directory)]


def worker_execution_coordinates(execution):
    """Submission and transport ordinals, excluding host acceptance executions."""
    if execution.parent.parent.name == 'turns':
        return int(execution.parent.name), 0
    if execution.parent.parent.name == 'transports' and execution.parents[3].name == 'turns':
        return int(execution.parents[2].name), int(execution.parent.name)
    return None


def _pinned(attempts):
    return any((attempt / 'retain-builds.json').exists() or (attempt / 'retain-builds.json').is_symlink()
               for attempt in attempts)


def prune(directory, *, cancelled=lambda: False):
    if cancelled():
        return False
    directory = Path(directory)
    marker = directory / 'build-caches-pruned.json'
    if directory.is_symlink():
        raise RuntimeError('refusing to prune through a symlinked attempt directory')
    if not directory.is_dir():
        return False
    # Operators pin build evidence for an unresolved issue in this host-only
    # directory. Sources, receipts and sealed candidates are always retained.
    if (directory / 'retain-builds.json').exists():
        return False
    if marker.exists():
        with marker.open('rb') as stream:
            raw = stream.read(65537)
        if len(raw) > 65536:
            raise ValueError('oversized cache cleanup marker')
        if json.loads(raw).get('schema') == 5:
            return False
    attempts = _attempt_directories(directory)
    if _pinned(attempts):
        return False
    executions = [attempt / 'execution' for attempt in attempts]
    for execution in executions:
        if execution.is_symlink():
            raise RuntimeError('refusing to prune through a symlinked execution directory')
        process = execution / 'process.json'
        if process.exists():
            identity = json.loads(process.read_text())
            cleanup = execution / 'cleanup.json'
            if alive(identity) or not cleanup.exists() or json.loads(cleanup.read_text()) != identity:
                raise RuntimeError('refusing to prune an attempt before acknowledged process cleanup')
    # The caller has observed a native terminal receipt, after compiler-channel
    # release. Keep sources, candidates, logs, artifact stores and publication
    # journals. Shared package caches and deployments outside these private
    # repositories are never traversed; rmtree unlinks internal symlinks.
    candidates = []
    provider_caches = set()
    for attempt in attempts:
        repo = attempt / 'repo'
        for relative in ('.lake', '.trellis/supervisor/repo/.lake'):
            candidates.append(repo / relative)
        # These exact provider entries are regenerable downloads. Keep config,
        # credentials, session databases, transcripts and user-authored skills.
        for home in attempt.glob('runtime/burst-homes/*/.codex'):
            for relative in ('cache/remote_plugin_catalog', 'cache/codex_apps_tools',
                             'plugins/cache', 'models_cache.json', 'skills/.system'):
                target = home / relative
                candidates.append(target)
                provider_caches.add(target)
    targets = []
    for target in candidates:
        for parent in [target, *target.parents]:
            if parent == directory:
                break
            if parent.is_symlink():
                raise RuntimeError(f'refusing to prune through a symlink: {parent}')
        if target in provider_caches and (target.exists() or target.is_symlink()):
            from .retained_files import _untracked
            if not _untracked(target):
                continue
            if not target.is_dir() and not target.is_file():
                raise RuntimeError(f'provider cache is not a regular file or directory: {target}')
            targets.append(target)
        elif target.is_dir():
            targets.append(target)
    # Validate every root before deleting any of them.
    removed = []
    for target in targets:
        if cancelled():
            return bool(removed)
        # A pin installed while another root was being removed stops the rest.
        if _pinned(attempts):
            return bool(removed)
        if target in provider_caches:
            from .retained_files import _untracked
            if not _untracked(target):
                continue
        if target.is_dir():
            shutil.rmtree(target)
        else:
            target.unlink()
        if target.exists() or target.is_symlink():
            raise RuntimeError(f'private cache cleanup left its target present: {target}')
        removed.append(str(target.relative_to(directory)))
    if cancelled():
        return bool(removed)
    atomic(marker, canonical_json(dict(schema=5, removed=removed)))
    return True


def terminal_directories(root, state, *, include_batch_caches=True):
    """Only terminal, never-reopened IDs are eligible; retries get new IDs."""
    ledger = state['dispatch']
    for id, task in ledger['tasks'].items():
        if task['status']['status'] == 'resolved':
            yield root / str(id)
    for id, job in ledger['verifiers']['jobs'].items():
        if job['status']['status'] == 'resolved':
            yield root / f'verifier-{id}'
    for id in ledger.get('receipts', {}):
        yield root / f'review-{id}'
        if include_batch_caches:
            yield from terminal_batch_caches(root, ledger, id)


def _resolved_batch_tasks(ledger):
    return {id: task.get('review_id') for id, task in ledger['tasks'].items()
            if task['status']['status'] == 'resolved'}


def _batch_candidates(root, review_ids):
    """Incremental metadata scan; boundary tokens bound empty-review scans too."""
    for review_id in review_ids:
        yield None
        parent = root / 'batch-caches' / str(review_id)
        try:
            if parent.parent.is_symlink() or parent.is_symlink():
                continue
            # Keep one scandir cursor across bounded turns instead of listing
            # the full historical catalog on every scheduler wake.
            with os.scandir(parent) as entries:
                for entry in entries:
                    yield review_id, Path(entry.path)
        except OSError:
            continue


def _terminal_batch_cache(root, terminal_tasks, review_id, cache):
    try:
        if (cache.parent.parent.is_symlink() or cache.parent.is_symlink()
                or cache.is_symlink() or not cache.is_dir()):
            return False
        from .batch_cache import read, FORMAT
        receipt = read(cache / 'capture.json')
        binding = receipt['binding']
        workers = binding['workers']
        return (receipt['format'] == FORMAT and binding['review_id'] == int(review_id) and bool(workers)
                and all(task_id in terminal_tasks and terminal_tasks[task_id] == int(review_id) for task_id in workers)
                and not any(_pinned(_attempt_directories(root / task_id)) for task_id in workers))
    except (OSError, ValueError, TypeError, KeyError, RuntimeError):
        return False  # Optional malformed/pinned captures remain available for inspection.


def terminal_batch_caches(root, ledger, review_id):
    terminal_tasks = _resolved_batch_tasks(ledger)
    for item in _batch_candidates(root, (review_id,)):
        if item is not None:
            _, cache = item
            if _terminal_batch_cache(root, terminal_tasks, review_id, cache):
                yield cache


def collect(root, state):
    for directory in terminal_directories(root, state):
        prune(directory)


class BackgroundCollector:
    """Drain eligible housekeeping independently of scheduler turns.

    Retained inputs are attempted once per drain. Updates replace the small
    eligibility snapshot and append newly terminal IDs without old verifier maps.
    Deferred build caches retry at spaced intervals while other work drains, so
    process cleanup acknowledgements do not wait behind historical retirement.
    Deferred caches alone do not prevent idle. No disk discovery runs on callers.
    """
    def __init__(self, root, *, repo=None, wait_for_cleanup_lock=False,
                 owner_workers=32, preparers=4, lease_seconds=None, cache_retry_seconds=60.0,
                 git_maintenance_threshold_bytes=1024 ** 3, git_maintenance_interval=60.0,
                 retire_failed_sources=False, recoverable_source_admission=None,
                 archive_provider_sessions=False, provider_session_admission=None,
                 compact_terminal_batches=False, terminal_batch_admission=None,
                 retire_publication_caches=False, publication_cache_admission=None,
                 retire_terminal_batch_caches=False,
                 retire_retry_preparations=False, retry_preparation_admission=None):
        if (owner_workers < 1 or preparers < 1
                or (lease_seconds is not None and not 0 < lease_seconds < float('inf'))
                or not 0 < cache_retry_seconds < float('inf')):
            raise ValueError('cleanup concurrency, lease quantum and retry interval must be positive')
        self.owner_workers, self.preparers, self.lease_seconds = owner_workers, preparers, lease_seconds
        # Deploy reader-capable coordinator code before explicitly opting a
        # cleanup service into this new logical source representation.
        if type(retire_failed_sources) is not bool:
            raise ValueError('failed source retirement requires an explicit boolean opt-in')
        if recoverable_source_admission is not None and not callable(recoverable_source_admission):
            raise ValueError('recoverable source admission must be callable')
        self.recoverable_source_admission = recoverable_source_admission
        self.retire_failed_sources = retire_failed_sources
        if type(archive_provider_sessions) is not bool:
            raise ValueError('provider session archival requires an explicit boolean opt-in')
        if provider_session_admission is not None and not callable(provider_session_admission):
            raise ValueError('provider session admission must be callable')
        self.archive_provider_sessions = archive_provider_sessions
        self.provider_session_admission = provider_session_admission
        if type(compact_terminal_batches) is not bool:
            raise ValueError('terminal batch compaction requires an explicit boolean opt-in')
        if terminal_batch_admission is not None and not callable(terminal_batch_admission):
            raise ValueError('terminal batch admission must be callable')
        self.compact_terminal_batches = compact_terminal_batches
        self.terminal_batch_admission = terminal_batch_admission
        self._terminal_batches = {}
        if type(retire_terminal_batch_caches) is not bool:
            raise ValueError('terminal batch cache retirement requires an explicit boolean opt-in')
        self.retire_terminal_batch_caches = retire_terminal_batch_caches
        if type(retire_publication_caches) is not bool:
            raise ValueError('publication cache retirement requires an explicit boolean opt-in')
        if publication_cache_admission is not None and not callable(publication_cache_admission):
            raise ValueError('publication cache admission must be callable')
        self.retire_publication_caches = retire_publication_caches
        self._publication_candidates = {}
        if type(retire_retry_preparations) is not bool:
            raise ValueError('retry preparation retirement requires an explicit boolean')
        if retry_preparation_admission is not None and not callable(retry_preparation_admission):
            raise ValueError('retry preparation admission must be callable')
        self._retry_candidates = {}
        self._retry_pending = deque()
        self._retry_queued = set()
        from .git_maintenance import Worker as GitMaintenance
        self._git_maintenance = (GitMaintenance(repo, threshold_bytes=git_maintenance_threshold_bytes,
            interval=git_maintenance_interval) if repo is not None and git_maintenance_threshold_bytes is not None else None)
        self._parallel = repo is not None
        self._broker = None
        self.root = Path(root)
        self.wait_for_cleanup_lock = wait_for_cleanup_lock
        self._guard = threading.Lock()
        self._condition = threading.Condition(self._guard)
        self._cache_enabled = repo is not None
        self.cache_retry_seconds = cache_retry_seconds
        self._cache_queue = deque()
        self._cache_queued = set()  # Includes the currently active cache owner.
        self._cache_done = set()
        self._cache_retry = {}
        self._cache_active = False
        self._thread = None
        self._running = False
        self._stop = threading.Event()
        self._idle = threading.Event()
        self._idle.set()
        self._queue = deque()
        self._queued = set()
        self._batch_pending = deque()
        self._batch_seen = set()
        self._batch_iterator = None
        self._batch_retry = {}
        self._batch_retry_queue = deque()
        self._batch_retry_queued = set()
        self._batch_retry_turn = True
        self._review_pending = deque()
        self._terminal_tasks = None
        self._accepted_sources = {}
        self._directories = set()
        self._batch_reviews = ()
        self._batch_tasks = {}
        self._summary = {}
        self._drain_sequence = 0
        from .review_snapshots import Collector as ReviewCollector
        from .terminal_retention import Collector as TerminalCollector
        self._review_snapshots = ReviewCollector(self.root, repo, stop_event=self._stop) if repo is not None else None
        self._terminal_retention = TerminalCollector(self.root, repo, wait_for_lock=True,
            stop_event=self._stop) if repo is not None else None
        from .publication_retention import Collector as PublicationCollector
        self._publication_retention = (PublicationCollector(self._terminal_retention,
            publication_cache_admission) if self._terminal_retention is not None and retire_publication_caches else None)

        from .retry_retention import Collector as RetryCollector
        self._retry_retention = (RetryCollector(self._terminal_retention, retry_preparation_admission)
            if self._terminal_retention is not None and retire_retry_preparations else None)

    @property
    def summary(self):
        with self._guard:
            result = dict(self._summary)
            if self._git_maintenance is not None:
                # Lifetime counters/gauges are nested, not drain counters.
                result['git_maintenance'] = self._git_maintenance.summary
            if self._broker is not None:
                for key, value in (('coordinator_leases', self._broker.leases),
                                   ('retirement_sections', self._broker.sections)):
                    result[key] = result.get(key, 0) + value
                for key, value in self._broker.timing.items():
                    result[key] = result.get(key, 0.0) + value
            return result

    def wait_idle(self, timeout=None):
        """Wait for this finite drain; does not schedule or retry blocked work."""
        started = time.monotonic()
        if not self._idle.wait(timeout):
            return False
        if self._stop.is_set() and self._git_maintenance is not None:
            remaining = None if timeout is None else max(0, timeout - (time.monotonic() - started))
            return self._git_maintenance.wait(remaining)
        return True

    def close(self):
        """Stop accepting work; current atomic deletion may finish.

        A queued lock waiter checks cancellation after acquisition and releases
        without mutation. Closing never waits for protocol locks or tree removal. It allows up to
        three seconds for owned Git packing child termination and lease release.
        """
        self._stop.set()
        if self._git_maintenance is not None:
            self._git_maintenance.close()
            self._git_maintenance.wait(3)
        with self._guard:
            self._summary['cancelled'] = True
            self._condition.notify_all()
        if self._broker is not None:
            self._broker.close()

    def schedule(self, state):
        with self._guard:
            if self._stop.is_set():
                return False
            if self._git_maintenance is not None:
                self._git_maintenance.schedule()
            directories = list(terminal_directories(self.root, state, include_batch_caches=False))
            self._directories = set(directories)
            self._batch_reviews = tuple(state['dispatch'].get('receipts', {}))
            if self.compact_terminal_batches or self.retire_terminal_batch_caches:
                from .terminal_batches import observations
                self._terminal_batches = observations(state)
            previous_tasks = self._batch_tasks
            self._batch_tasks = _resolved_batch_tasks(state['dispatch'])
            changed_reviews = {str(review_id) for task_id, review_id in self._batch_tasks.items()
                               if previous_tasks.get(task_id) != review_id}
            if self._terminal_retention is not None:
                from .terminal_retention import terminal_owners, accepted_source_candidates, publication_cache_candidates
                self._accepted_sources = accepted_source_candidates(state)
                if self.retire_publication_caches:
                    publications = publication_cache_candidates(state)
                    if not self._running or publications != self._publication_candidates:
                        self._publication_retention.invalidate_catalog()
                    self._publication_candidates = publications
                self._terminal_tasks = terminal_owners(state, defer_verifiers=True)
            if not self._running:
                self._queue.clear(); self._queued.clear()
                self._cache_queue.clear(); self._cache_queued.clear()
                self._cache_done.clear(); self._cache_retry.clear()
                self._batch_pending.clear(); self._batch_seen.clear()
                self._batch_retry.clear(); self._batch_retry_queue.clear(); self._batch_retry_queued.clear()
                self._review_pending.clear()
                self._retry_pending.clear(); self._retry_queued.clear()
            if self._retry_retention is not None:
                from .retry_retention import observations as retry_observations
                self._retry_candidates = retry_observations(state)
                # Exact paths only. Changes during a busy drain revisit custody;
                # no historical payloads are opened or scanned.
                for key in self._retry_candidates:
                    if key not in self._retry_queued:
                        self._retry_pending.append(key); self._retry_queued.add(key)
            for directory in directories:
                if directory not in self._queued:
                    self._queued.add(directory)
                    self._queue.append(directory)
            if self._cache_enabled:
                self._admit_caches(directories)
                self._admit_batch_retries()
            pending_reviews = set(self._batch_pending)
            for review_id in self._batch_reviews:
                if review_id not in self._batch_seen:
                    self._batch_seen.add(review_id)
                    self._batch_pending.append(review_id)
                    self._review_pending.append(review_id)
                elif review_id in changed_reviews and review_id not in pending_reviews:
                    # A cache may have materialized after an earlier empty
                    # catalog scan. Revisit only reviews with newly terminal
                    # worker bindings, not every historical catalog per tick.
                    self._batch_pending.append(review_id)
            if self.compact_terminal_batches or self.retire_terminal_batch_caches:
                pending_review_cleanup = set(self._review_pending)
                for review_id in sorted(changed_reviews & self._terminal_batches.keys()):
                    if review_id not in pending_review_cleanup:
                        self._review_pending.append(review_id)
            self._condition.notify_all()
            if self._running:
                return False  # Updated the existing worker; never launch a second.
            if not self._queue and not self._batch_pending and not self._retry_pending:
                return False
            self._drain_sequence += 1
            self._summary = dict(drain_id=str(self._drain_sequence), owners=0, build_caches=0, private_files=0,
                recoverable_source_files=0, recovery_index_allocated_bytes=0,
                framework_trees=0, source_trees=0, source_owners_retained=0, auxiliary_files=0, auxiliary_files_retained=0, worker_source_files=0, worker_source_files_retained=0, accepted_source_files=0, accepted_source_files_retained=0, execution_inputs=0, history_indexes=0, compiled_probe_files=0, checker_cache_files=0, audit_copy_files=0, audit_copy_files_retained=0, audit_empty_directories=0, audit_empty_directory_allocated_bytes=0, assignment_archives=0, host_response_archives=0, redundant_host_results=0, provider_session_archives=0, provider_archives=0, git_object_stores=0, git_object_stores_retained=0, git_object_links=0, git_object_unlinked_allocated_bytes=0, git_pack_stores=0, git_pack_files=0, git_new_pack_allocated_bytes=0, removed_bytes=0, removed_allocated_bytes=0,
                retry_preparation_files=0, publication_cache_journals=0, publication_cache_files=0, publication_cache_marker_allocated_bytes=0,
                terminal_batch_caches=0, terminal_batch_cache_files=0, terminal_batch_cache_retained_files=0, terminal_batch_cache_directories=0,
                review_snapshots=0, terminal_batch_compactions=0, terminal_batch_worker_objects=0, terminal_batch_record_allocated_bytes=0, batch_caches=0, errors=0, coordinator_leases=0, retirement_sections=0,
                lease_acquired=False, lease_busy=False, cancelled=False)
            self._idle.clear()
            self._running = True
            self._thread = threading.Thread(target=self._collect, name='private-build-cleanup', daemon=True)
            try:
                self._thread.start()
            except RuntimeError:
                self._thread = None
                self._running = False
                self._idle.set()
                logging.getLogger(__name__).warning('Private build cleanup could not start', exc_info=True)
                return False
            return True

    def _count(self, key, value=1):
        with self._guard:
            self._summary[key] = self._summary.get(key, 0) + value

    def _failed(self, directory):
        self._count('errors')
        logging.getLogger(__name__).warning('Private build cleanup deferred for %s', directory, exc_info=True)

    def _collect_owner(self, directory):
        with self._guard:
            if directory not in self._directories:
                return
            observed = self._terminal_tasks
        self._count('owners')
        if not self._cache_enabled:
            self._count('build_caches', bool(prune(directory, cancelled=self._stop.is_set)))
        if self._stop.is_set() or self._terminal_retention is None or directory.name not in observed:
            return
        # No per-turn budget: the retention layer processes every allowed file,
        # with its committed identity, live-process, pin and Git guards intact.
        result = self._terminal_retention.retire(directory, observed, apply=True)
        self._count('private_files', result['removed'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        result = self._terminal_retention.retire_execution(directory, observed, apply=True)
        self._count('execution_inputs', result['removed'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        result = self._terminal_retention.retire_history(directory, observed, apply=True)
        self._count('history_indexes', result['removed'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        result = self._terminal_retention.retire_compiled_probes(directory, observed, apply=True)
        self._count('compiled_probe_files', result['removed'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        result = self._terminal_retention.retire_checker_caches(directory, observed, apply=True)
        self._count('checker_cache_files', result['removed'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .framework_retention import retire
        result = retire(self._terminal_retention, directory, observed, apply=True)
        self._count('framework_trees', result['removed'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .source_retention import retire as retire_sources
        result = retire_sources(self._terminal_retention, directory, observed, apply=True)
        self._count('source_trees', result['removed'])
        self._count('source_owners_retained', result.get('retained_sources', 0))
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .auxiliary_retention import retire as retire_auxiliary
        result = retire_auxiliary(self._terminal_retention, directory, observed, apply=True)
        self._count('auxiliary_files', result['removed'])
        self._count('auxiliary_files_retained', result.get('retained_files', 0))
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .provider_retention import collect as retain_provider
        result = retain_provider(self._terminal_retention, directory, observed, apply=True)
        self._count('provider_archives', result['archived'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .git_retention import collect as retain_git_objects
        result = retain_git_objects(self._terminal_retention, directory, observed, apply=True)
        self._count('git_object_stores', result['stores'])
        self._count('git_object_stores_retained', result['retained_stores'])
        self._count('git_object_links', result['removed_links'])
        self._count('git_object_unlinked_allocated_bytes', result['unlinked_allocated_bytes'])
        self._count('git_pack_stores', result['pack_stores'])
        self._count('git_pack_files', result['removed_pack_files'])
        self._count('git_new_pack_allocated_bytes', result['new_pack_allocated_bytes'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        if self.archive_provider_sessions:
            from .provider_session_retention import collect as retain_sessions
            result = retain_sessions(self._terminal_retention, directory, observed, apply=True,
                runtime_admission=self.provider_session_admission)
            self._count('provider_session_archives', result['archived'])
            self._count_removed_bytes(result)
        if self._stop.is_set() or not directory.name.isdigit():
            return
        from .worker_source_retention import retire as retire_worker_sources
        result = retire_worker_sources(self._terminal_retention, directory, observed, apply=True)
        self._count('worker_source_files', result['removed'])
        self._count('worker_source_files_retained', result.get('retained_files', 0))
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .accepted_source_retention import retire as retire_accepted_sources
        result = retire_accepted_sources(self._terminal_retention, directory, observed, apply=True,
            accepted=self._accepted_source_observation)
        self._count('accepted_source_files', result['removed'])
        self._count('accepted_source_files_retained', result.get('retained_files', 0))
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        if self.retire_failed_sources:
            from .recoverable_source_retention import retire as retire_recoverable_sources
            result = retire_recoverable_sources(self._terminal_retention, directory, observed, apply=True,
                runtime_admission=self.recoverable_source_admission)
            self._count('recoverable_source_files', result['removed'])
            self._count('recovery_index_allocated_bytes', result.get('new_recovery_index_allocated_bytes', 0))
            self._count_removed_bytes(result)
            if self._stop.is_set():
                return
        from .audit_copy_retention import retire as retire_audit_copies
        result = retire_audit_copies(self._terminal_retention, directory, observed, apply=True)
        self._count('audit_copy_files', result['removed'])
        self._count('audit_copy_files_retained', result.get('retained_files', 0))
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .audit_copy_retention import retire_empty_directories
        result = retire_empty_directories(self._terminal_retention, directory, observed, apply=True)
        self._count('audit_empty_directories', result['removed_directories'])
        self._count('audit_empty_directory_allocated_bytes', result['removed_allocated_bytes'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .host_response_retention import collect as retain_host_responses
        result = retain_host_responses(self._terminal_retention, directory, observed, apply=True)
        self._count('host_response_archives', result['archived'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        from .redundant_host_result_retention import collect as retire_host_results
        result = retire_host_results(self._terminal_retention, directory, observed, apply=True)
        self._count('redundant_host_results', result['removed'])
        self._count_removed_bytes(result)
        if self._stop.is_set():
            return
        if self._publication_retention is not None:
            def current_publication():
                with self._guard:
                    return self._publication_candidates.get(directory.name)
            result = self._publication_retention.retire(directory, observed, current_publication, apply=True)
            self._count('publication_cache_journals', result['journals'])
            self._count('publication_cache_files', result['removed'])
            self._count('publication_cache_marker_allocated_bytes', result['marker_allocated_bytes'])
            self._count_removed_bytes(result)
            if self._stop.is_set():
                return
        from .assignment_retention import collect
        result = collect(self._terminal_retention, directory, observed, apply=True)
        self._count('assignment_archives', result['archived'])
        self._count_removed_bytes(result)

    def _collect_retry_preparation(self):
        with self._guard:
            if not self._retry_pending or self._retry_retention is None:
                return False
            key = self._retry_pending.popleft()
            self._retry_queued.discard(key)
        def current():
            with self._guard:
                return self._retry_candidates.get(key)
        if not self._stop.is_set():
            result = self._retry_retention.retire(int(key), current, apply=True)
            self._count('retry_preparation_files', result['removed'])
            self._count_removed_bytes(result)
        return True

    def _accepted_source_observation(self, name):
        with self._guard:
            return (self._terminal_tasks.get(name), self._accepted_sources.get(name))

    def _count_removed_bytes(self, result):
        # Removed private blocks plus net assignment archive savings, not a free-space
        # delta: concurrent writes and filesystem bookkeeping remain separate.
        for key in ('removed_bytes', 'removed_allocated_bytes'):
            self._count(key, result.get(key, 0))

    def _admit_batch_retries(self):
        """Condition-held exact-path retries; never rescan historical catalogs."""
        now = time.monotonic()
        for cache, (review_id, deadline) in tuple(self._batch_retry.items()):
            if review_id not in self._batch_reviews:
                self._batch_retry.pop(cache, None)
            elif deadline <= now and cache not in self._batch_retry_queued:
                self._batch_retry_queue.append((review_id, cache))
                self._batch_retry_queued.add(cache)
                self._batch_retry.pop(cache, None)

    def _collect_batch_cache(self):
        """Advance discovery and exact deferred paths fairly, without rescans."""
        with self._guard:
            retry = bool(self._batch_retry_queue) and (self._batch_retry_turn
                or self._batch_iterator is None and not self._batch_pending)
            self._batch_retry_turn = not retry
            item = self._batch_retry_queue.popleft() if retry else None
        if not retry:
            if self._batch_iterator is None:
                with self._guard:
                    if not self._batch_pending:
                        return False
                    review_id = self._batch_pending.popleft()
                self._batch_iterator = iter(_batch_candidates(self.root, (review_id,)))
            try:
                item = next(self._batch_iterator)
            except StopIteration:
                self._batch_iterator = None
                return True
            if item is None:
                return True
        review_id, cache = item
        with self._guard:
            eligible = review_id in self._batch_reviews
            tasks = self._batch_tasks
        complete = False
        try:
            if not eligible or self._stop.is_set():
                return True
            if not cache.exists() and not cache.is_symlink():
                complete = True
                return True
            marker = cache / 'build-caches-pruned.json'
            try:
                if not marker.is_symlink():
                    with marker.open('rb') as stream:
                        raw = stream.read(65537)
                    if len(raw) <= 65536 and json.loads(raw).get('schema') == 5:
                        complete = True
                        return True
            except (OSError, ValueError, AttributeError):
                pass
            if _terminal_batch_cache(self.root, tasks, review_id, cache):
                with self._guard:
                    current = self._batch_tasks
                    eligible = review_id in self._batch_reviews
                if not eligible or (current is not tasks and not _terminal_batch_cache(self.root, current, review_id, cache)):
                    return True
                # Revalidate exact capture/aliases before prune's process/pin guards.
                if not (cache.parent.parent.is_symlink() or cache.parent.is_symlink() or cache.is_symlink()):
                    self._count('batch_caches', bool(prune(cache, cancelled=self._stop.is_set)))
                    complete = self._cache_complete(cache)
            return True
        finally:
            with self._guard:
                if retry:
                    self._batch_retry_queued.discard(cache)
                if complete or not eligible:
                    self._batch_retry.pop(cache, None)
                elif not self._stop.is_set():
                    self._batch_retry[cache] = (review_id, time.monotonic() + self.cache_retry_seconds)
                self._condition.notify_all()

    def _collect_review(self):
        with self._guard:
            if not self._review_pending:
                return False
            review_id = self._review_pending.popleft()
            eligible = review_id in self._batch_reviews
        if eligible and self._review_snapshots is not None and not self._stop.is_set():
            result = self._review_snapshots.retire(int(review_id), apply=True)
            self._count('review_snapshots', result['status'] == 'retired')
        if self.compact_terminal_batches and eligible and self._terminal_retention is not None and not self._stop.is_set():
            from .terminal_batches import retire
            def current():
                with self._guard:
                    return self._terminal_batches.get(str(review_id))
            result = retire(self._terminal_retention, int(review_id), current, apply=True,
                runtime_admission=self.terminal_batch_admission)
            self._count('terminal_batch_compactions', result['compacted'])
            self._count('terminal_batch_worker_objects', result['removed_objects'])
            self._count('terminal_batch_record_allocated_bytes', result['new_compact_allocated_bytes'])
            self._count_removed_bytes(result)
        if self.retire_terminal_batch_caches and eligible and self._terminal_retention is not None and not self._stop.is_set():
            from .batch_cache_retention import retire
            def current():
                with self._guard:
                    return self._terminal_batches.get(str(review_id))
            result = retire(self._terminal_retention, int(review_id), current, apply=True,
                runtime_admission=self.terminal_batch_admission)
            self._count('terminal_batch_caches', result['caches'])
            self._count('terminal_batch_cache_files', result['removed_files'])
            self._count('terminal_batch_cache_retained_files', result['retained_files'])
            self._count('terminal_batch_cache_directories', result['removed_directories'])
            self._count_removed_bytes(result)
        return True

    def _admit_caches(self, directories):
        """Condition-held admission; no filesystem checks on scheduler callers."""
        now = time.monotonic()
        for directory in directories:
            if (directory in self._directories and directory not in self._cache_done
                    and directory not in self._cache_queued
                    and self._cache_retry.get(directory, 0) <= now):
                self._cache_queue.append(directory)
                self._cache_queued.add(directory)
                self._cache_retry.pop(directory, None)

    @staticmethod
    def _cache_complete(directory):
        if not directory.exists():
            return True
        marker = directory / 'build-caches-pruned.json'
        if not marker.is_file() or marker.is_symlink():
            return False
        with marker.open('rb') as stream:
            raw = stream.read(65537)
        return len(raw) <= 65536 and json.loads(raw).get('schema') == 5

    def _collect_caches(self, closing):
        """Prune builds without waiting behind publication-lock retirement.

        A single daemon owns batch discovery and the cache queue. schedule only
        appends IDs; fresh terminal owners remain discoverable during long
        historical retirement or legacy review-snapshot lock waits.
        """
        while not closing.is_set() and not self._stop.is_set():
            with self._condition:
                # Due deferrals join the FIFO even during continued batch
                # discovery; a large batch inventory cannot starve retries.
                self._admit_caches(tuple(self._cache_retry))
                self._admit_batch_retries()
                while (not self._cache_queue and not self._batch_pending and not self._batch_retry_queue and self._batch_iterator is None
                       and not closing.is_set() and not self._stop.is_set()):
                    # A process can be reaped without another native state
                    # change. Retry on time while the historical drain remains
                    # active, but never keep a drain alive solely for deferrals.
                    self._cache_retry = {path: deadline for path, deadline in self._cache_retry.items()
                                         if path in self._directories}
                    self._admit_caches(tuple(self._cache_retry))
                    self._admit_batch_retries()
                    if self._cache_queue or self._batch_retry_queue:
                        break
                    deadlines = [*self._cache_retry.values(), *(value[1] for value in self._batch_retry.values())]
                    delay = max(0, min(deadlines) - time.monotonic()) if deadlines else None
                    self._condition.wait(timeout=delay)
                if closing.is_set() or self._stop.is_set():
                    return
                directory = self._cache_queue.popleft() if self._cache_queue else None
                eligible = directory in self._directories if directory is not None else False
                self._cache_active = True
            complete = False
            try:
                if eligible and not self._stop.is_set():
                    try:
                        self._count('build_caches', bool(prune(directory, cancelled=self._stop.is_set)))
                        complete = self._cache_complete(directory)
                    except Exception:
                        self._failed(directory)
                if not self._stop.is_set():
                    try:
                        self._collect_batch_cache()
                    except Exception:
                        self._failed(self.root)
            finally:
                with self._condition:
                    self._cache_active = False
                    if directory is not None:
                        self._cache_queued.discard(directory)
                        if complete:
                            self._cache_done.add(directory)
                        elif eligible and not closing.is_set() and not self._stop.is_set():
                            self._cache_retry[directory] = time.monotonic() + self.cache_retry_seconds
                    self._condition.notify_all()

    def _collect_parallel(self, stream, cache_closing):
        """Bound pending owner contexts, not the amount retired in a drain."""
        from .retirement_batch import Broker
        while not self._stop.is_set():
            broker = Broker(self._terminal_retention.direct_retirement_lock, self._stop,
                            preparers=self.preparers, quantum=self.lease_seconds)
            self._broker = broker
            self._terminal_retention.batch = broker
            workers_closed = threading.Event()
            active = 0
            fatal = []

            def worker():
                nonlocal active
                while True:
                    with self._condition:
                        while not self._queue and not workers_closed.is_set() and not self._stop.is_set():
                            self._condition.wait()
                        if workers_closed.is_set() or self._stop.is_set():
                            return
                        directory = self._queue.popleft()
                        active += 1
                    try:
                        with broker.preparing():
                            if not self._stop.is_set():
                                self._collect_owner(directory)
                    except Exception:
                        self._failed(directory)
                    except BaseException as error:
                        with self._condition:
                            fatal.append(error)
                            workers_closed.set()
                            self._condition.notify_all()
                    finally:
                        with self._condition:
                            active -= 1
                            self._condition.notify_all()

            workers = []
            try:
                for index in range(self.owner_workers):
                    thread = threading.Thread(target=worker, name=f'cleanup-owner-{index}', daemon=True)
                    thread.start()
                    workers.append(thread)
                while not self._stop.is_set():
                    progressed = False
                    # These iterators stay single-threaded. Owned input cleanup
                    # continues in parallel while legacy snapshots await locks.
                    for collect in (self._collect_review, self._collect_retry_preparation):
                        if self._stop.is_set():
                            break
                        try:
                            progressed = collect() or progressed
                        except Exception:
                            self._failed(self.root)
                    with self._condition:
                        if fatal:
                            raise fatal[0]
                        if not active and not self._queue and not self._batch_pending and not self._batch_retry_queue and not self._review_pending and not self._retry_pending and self._batch_iterator is None and not self._cache_active and not self._cache_queue:
                            # Fence worker admission before dropping this guard;
                            # schedule may enqueue during the subsequent joins.
                            workers_closed.set()
                            self._condition.notify_all()
                            break
                        if not progressed:
                            self._condition.wait()
            finally:
                workers_closed.set()
                broker.close()
                with self._condition:
                    self._condition.notify_all()
                for thread in workers:
                    thread.join()
                broker.join()
                self._terminal_retention.batch = None
                with self._guard:
                    self._summary['coordinator_leases'] += broker.leases
                    self._summary['retirement_sections'] += broker.sections
                    for key, value in broker.timing.items():
                        self._summary[key] = self._summary.get(key, 0.0) + value
                    self._broker = None
            with self._guard:
                # schedule() may append while the finished pool is joining.
                # Consume that work in this same drain before exposing idle.
                if not self._stop.is_set() and (self._queue or self._batch_pending or self._batch_retry_queue or self._review_pending or self._retry_pending
                                                or self._cache_queue or self._cache_active or self._batch_iterator is not None):
                    continue
                if self._stop.is_set():
                    # The outer build-lease context must join the cache lane
                    # before exposing idle or releasing its filesystem fence.
                    return
                cache_closing.set()
                self._condition.notify_all()
                fcntl.flock(stream, fcntl.LOCK_UN)
                self._running = False
                self._idle.set()
                return

    def _collect(self):
        cache_thread = None
        cache_closing = threading.Event()
        try:
            with (self.root / 'build-cleanup.lock').open('a+b') as stream:
                try:
                    fcntl.flock(stream, fcntl.LOCK_EX | (0 if self.wait_for_cleanup_lock else fcntl.LOCK_NB))
                except BlockingIOError:
                    with self._guard:
                        self._summary['lease_busy'] = True
                    return
                with self._guard:
                    self._summary['lease_acquired'] = True
                try:
                    if self._cache_enabled:
                        cache_thread = threading.Thread(target=self._collect_caches, args=(cache_closing,),
                                                        name='private-cache-cleanup', daemon=True)
                        cache_thread.start()
                    if self._parallel:
                        self._collect_parallel(stream, cache_closing)
                        return
                    while not self._stop.is_set():
                        with self._guard:
                            directory = self._queue.popleft() if self._queue else None
                        if directory is not None:
                            try:
                                self._collect_owner(directory)
                            except Exception:
                                self._failed(directory)
                        # Interleave catalog/reviewer work with the FIFO. These are
                        # units of work, not caps: continue until every queue drains.
                        progressed = directory is not None
                        for collect in ((self._collect_review, self._collect_retry_preparation) if self._cache_enabled else
                                        (self._collect_batch_cache, self._collect_review, self._collect_retry_preparation)):
                            if self._stop.is_set():
                                break
                            try:
                                progressed = collect() or progressed
                            except Exception:
                                self._failed(self.root)
                        with self._condition:
                            cache_pending = self._cache_enabled and (self._cache_active or self._cache_queue or self._batch_iterator is not None)
                            if not progressed and not self._queue and not self._batch_pending and not self._batch_retry_queue and not self._review_pending and not self._retry_pending and not cache_pending:
                                cache_closing.set()
                                self._condition.notify_all()
                                # Release the handoff fence before exposing idle;
                                # schedule racing with completion can start safely.
                                fcntl.flock(stream, fcntl.LOCK_UN)
                                self._running = False
                                self._idle.set()
                                return
                            if not progressed and cache_pending:
                                self._condition.wait()
                finally:
                    cache_closing.set()
                    with self._condition:
                        self._condition.notify_all()
                    if cache_thread is not None and cache_thread.ident is not None:
                        cache_thread.join()
        except Exception:
            self._failed(self.root)
        finally:
            with self._guard:
                if self._thread is threading.current_thread():
                    if self._batch_iterator is not None:
                        self._batch_iterator.close()
                        self._batch_iterator = None
                    self._running = False
                    self._idle.set()

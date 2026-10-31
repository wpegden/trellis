"""Retire completed reviewer snapshots using committed Git state plus a small delta.

The active review still needs its full, immutable input. Completed reviews may
drop that file only after their receipt is in Git and a bounded recovery record
has reproduced the entire JSON value. No replay, checkout, native transition,
batch receipt, or provider transcript is changed by this housekeeping.
"""
from __future__ import annotations

import argparse
import copy
from contextlib import nullcontext
import fcntl
import gzip
import json
import logging
import os
from pathlib import Path
import re
import stat
import subprocess
import time

from trellis.history_artifacts import decode_shared_state_text
from .candidate import canonical_json, digest, _sync_dir
from .history import resolve
from .publication import atomic
from . import review_snapshot_files

FORMAT = 'trellis-review-state-git/1'
HISTORY = '.trellis-history/supervisor_state.json'
MAX_DELTA_BYTES = 8 * 1024 * 1024


def _git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args], stderr=subprocess.PIPE)


def _commit(value):
    if not isinstance(value, str) or not re.fullmatch(r'[0-9a-f]{40}|[0-9a-f]{64}', value):
        raise ValueError('invalid Git commit identity')
    return value


def _regular(path):
    path = Path(path)
    if any(parent.is_symlink() for parent in (path, *path.parents)):
        raise ValueError(f'symlink in snapshot path: {path}')
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f'not a regular snapshot file: {path}')
    return path


def _read(path):
    return json.loads(_regular(path).read_bytes())


def git_state(repo, commit):
    """Read the immutable tree, never the possibly dirty history worktree."""
    commit = _commit(commit)
    document = decode_shared_state_text(_git(repo, 'show', f'{commit}:{HISTORY}').decode())
    state = document['state']
    for task in (state.get('dispatch') or {}).get('tasks', {}).values():
        request = task['assignment']['request']
        if '$stored_dispatch_request' in request:
            if set(request) != {'$stored_dispatch_request'}:
                raise ValueError('malformed stored dispatch request')
            task['assignment']['request'] = resolve(repo, request['$stored_dispatch_request'],
                read_object=lambda path: _git(repo, 'show', f'{commit}:{path}'))
    return state


def _delta(before, after):
    """Dictionary edits; arrays are atomic. Oversized differences stay on disk."""
    edits, size = [], 0

    def add(edit):
        nonlocal size
        size += len(canonical_json(edit))
        if size > MAX_DELTA_BYTES:
            raise ValueError('snapshot delta exceeds retention limit')
        edits.append(edit)

    def visit(old, new, path):
        if type(old) is type(new) and old == new:
            return
        if isinstance(old, dict) and isinstance(new, dict):
            for key in sorted(old.keys() - new.keys()):
                add(dict(path=path + [key], delete=True))
            for key in sorted(new):
                if key not in old:
                    add(dict(path=path + [key], value=new[key]))
                else:
                    visit(old[key], new[key], path + [key])
        else:
            add(dict(path=path, value=new))

    visit(before, after, [])
    return edits


def _apply(state, edits):
    if len(canonical_json(edits)) > MAX_DELTA_BYTES + 65536:
        raise ValueError('oversized recovery delta')
    for edit in edits:
        path = edit['path']
        if not isinstance(path, list) or not path or not all(isinstance(k, str) for k in path):
            raise ValueError('invalid recovery path')
        parent = state
        for key in path[:-1]:
            parent = parent[key]
        if set(edit) == {'path', 'delete'} and edit['delete'] is True:
            del parent[path[-1]]
        elif set(edit) == {'path', 'value'}:
            parent[path[-1]] = copy.deepcopy(edit['value'])
        else:
            raise ValueError('invalid recovery edit')
    return state


def restore(repo, record):
    """Recover the exact JSON value (whitespace is not retained)."""
    if record['format'] != FORMAT:
        raise ValueError('unsupported reviewer recovery record')
    state = _apply(git_state(repo, record['base_commit']), record['delta'])
    _restore_requests(repo, state, record.get('request_objects', []))
    if digest(canonical_json(state)) != record['state_sha256']:
        raise ValueError('recovered reviewer state digest differs')
    return state


def _restore_requests(repo, state, objects):
    for item in objects:
        assignment = state['dispatch']['tasks'][item['task_id']]['assignment']
        if assignment['request'] != item['reference']:
            raise ValueError('recovery request reference differs')
        commit = _commit(item['commit'])
        assignment['request'] = resolve(repo, item['reference'],
            read_object=lambda path: _git(repo, 'show', f'{commit}:{path}'))


class Collector:
    def __init__(self, root, repo, *, stop_event=None):
        self.root, self.repo = Path(root), Path(repo).resolve()
        self.stop_event = stop_event
        self._head = None
        self._receipts = {}
        self._request_refs = {}
        self._cursor = 0

    def _witness(self):
        head = _git(self.repo, 'rev-parse', 'HEAD').decode().strip()
        if head != self._head:
            # No cache of hundreds of MB of checkpoint state, only its receipts.
            document = decode_shared_state_text(_git(self.repo, 'show', f'{head}:{HISTORY}').decode())
            self._receipts = (document['state'].get('dispatch') or {}).get('receipts', {})
            self._request_refs = {}
            for task_id, task in (document['state'].get('dispatch') or {}).get('tasks', {}).items():
                request = task['assignment']['request']
                reference = request.get('$stored_dispatch_request', request)
                if '$dispatch_request' in reference:
                    self._request_refs[task_id] = reference
            self._head = head
        return head

    def _pinned(self, review_id):
        return any(path.exists() or path.is_symlink() for path in (
            self.root / 'reviews' / str(review_id) / 'retain-state.json',
            self.root / f'review-{review_id}' / 'retain-builds.json',
        ))

    def retire(self, review_id, *, apply=False):
        if self.stop_event is not None and self.stop_event.is_set():
            return dict(review_id=review_id, status='cancelled')
        if type(review_id) is not int or review_id < 0:
            raise ValueError('invalid review ID')
        folder = self.root / 'reviews' / str(review_id)
        logical_path, record_path = folder / 'state.json', folder / 'state.git.json'
        path = review_snapshot_files.location(logical_path)
        if path is None:
            return dict(review_id=review_id, status='absent')
        _regular(path)
        from .retained_files import _untracked
        if not _untracked(path) or not _untracked(record_path):
            return dict(review_id=review_id, status='tracked')
        if self._pinned(review_id):
            return dict(review_id=review_id, status='pinned')
        witness = self._witness()
        receipt = self._receipts.get(str(review_id))
        if receipt is None:
            return dict(review_id=review_id, status='not_committed')
        prepared = _read(self.root / f'review-{review_id}' / 'prepared.json')
        if prepared['request_id'] != review_id or Path(prepared['source_repo']).resolve() != self.repo:
            raise ValueError('review preparation belongs to another request or repository')
        base = _commit(prepared['source_head'])
        _git(self.repo, 'merge-base', '--is-ancestor', base, witness)
        info = path.stat()
        raw = path.read_bytes()
        raw_sha = digest(raw)
        snapshot = json.loads(gzip.decompress(raw) if path.suffix == '.gz' else raw)
        del raw
        if snapshot['dispatch']['reviewer']['request_id'] != review_id:
            raise ValueError('snapshot belongs to a different reviewer')
        original = git_state(self.repo, base)
        state_sha = digest(canonical_json(snapshot))
        request_objects = []
        # Tasks created since the base checkpoint can carry huge frozen inputs.
        # Reuse their exact Git objects from the completed checkpoint instead
        # of embedding those already-recorded inputs in the local delta.
        for task_id, task in snapshot['dispatch'].get('tasks', {}).items():
            request = task['assignment']['request']
            before = original['dispatch'].get('tasks', {}).get(task_id, {}).get('assignment', {}).get('request')
            reference = self._request_refs.get(task_id)
            if reference is None or '$dispatch_request' in request or request == before:
                continue
            frozen = resolve(self.repo, reference,
                read_object=lambda path: _git(self.repo, 'show', f'{witness}:{path}'))
            if digest(canonical_json(frozen)) != digest(canonical_json(request)):
                continue
            task['assignment']['request'] = reference
            request_objects.append(dict(task_id=task_id, commit=witness, reference=reference))
        edits = _delta(original, snapshot)
        # Check the complete reconstructed value before trusting any delta.
        reconstructed = _apply(original, edits)
        _restore_requests(self.repo, reconstructed, request_objects)
        if digest(canonical_json(reconstructed)) != state_sha:
            raise ValueError('review snapshot did not round-trip')
        record = dict(format=FORMAT, review_id=review_id, base_commit=base,
            receipt_commit=witness, original_sha256=raw_sha, original_bytes=info.st_size,
            state_sha256=state_sha, delta=edits)
        if path.suffix == '.gz':
            record['original_encoding'] = 'gzip'
        if request_objects:
            record['request_objects'] = request_objects
        encoded = canonical_json(record)
        if len(encoded) >= info.st_size:
            return dict(review_id=review_id, status='no_savings')
        del reconstructed, original, snapshot
        result = dict(review_id=review_id, status='eligible', original_bytes=info.st_size,
            allocated_bytes=info.st_blocks * 512, recovery_bytes=len(encoded),
            base_commit=base, receipt_commit=witness)
        if not apply:
            return result
        # Heavy Git reads/diffing happen outside the coordinator lock. This
        # short final check excludes active/reopened reviews and source rewinds.
        with _regular(self.root / 'coordinator.lock').open('rb') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            if self.stop_event is not None and self.stop_event.is_set():
                return dict(result, status='cancelled')
            current = _read(self.root.parent / 'protocol_state.json')['dispatch']
            if ((current.get('reviewer') or {}).get('request_id') == review_id
                    or current.get('receipts', {}).get(str(review_id)) != receipt):
                raise ValueError('review is active or no longer has its committed receipt')
            del current
            _git(self.repo, 'merge-base', '--is-ancestor', witness, 'HEAD')
            if self._pinned(review_id):
                return dict(review_id=review_id, status='pinned')
            if review_snapshot_files.location(logical_path) != path:
                raise ValueError('snapshot representation changed during verification')
            if not _untracked(path) or not _untracked(record_path):
                raise ValueError('snapshot or recovery record became Git tracked')
            fresh = _regular(path).stat()
            if (fresh.st_dev, fresh.st_ino, fresh.st_size, fresh.st_mtime_ns, fresh.st_ctime_ns) != (
                    info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns):
                raise ValueError('snapshot changed during verification')
            if record_path.exists() or record_path.is_symlink():
                old = _read(record_path)
                # An interrupted unlink may leave an earlier witness commit.
                if any(old.get(k) != record[k] for k in record
                       if k not in {'receipt_commit', 'original_sha256', 'original_bytes', 'original_encoding', 'request_objects'}):
                    raise ValueError('existing recovery record differs')
            if self.stop_event is not None and self.stop_event.is_set():
                return dict(result, status='cancelled')
            atomic(record_path, encoded)
            path.unlink()
            _sync_dir(folder)
        result['status'] = 'retired'
        return result

    def collect_one(self):
        """Bound each background pass to one snapshot; rotate past held inputs."""
        ids = sorted(int(p.name) for p in (self.root / 'reviews').iterdir()
                     if p.name.isdigit() and any((p / name).exists() or (p / name).is_symlink()
                        for name in ('state.json', 'state.json.gz'))) if (self.root / 'reviews').is_dir() else []
        if not ids:
            return
        review_id = next((id for id in ids if id > self._cursor), ids[0])
        self._cursor = review_id
        try:
            self.retire(review_id, apply=True)
        except Exception:
            logging.getLogger(__name__).warning('Reviewer snapshot cleanup deferred for %s', review_id, exc_info=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', type=Path, required=True)
    parser.add_argument('--repo', type=Path, required=True)
    parser.add_argument('--apply', action='store_true', help='default is a read-only eligibility report')
    parser.add_argument('--review', type=int, action='append')
    parser.add_argument('--restore', type=int, help='write recovered JSON to stdout; never overwrite the runtime')
    parser.add_argument('--watch-seconds', type=int,
        help='repeat cleanup while the coordinator owns its lock; supports older live coordinators')
    args = parser.parse_args()
    root = args.runtime / 'dispatch'
    if args.restore is not None:
        if args.apply or args.watch_seconds is not None:
            parser.error('--restore cannot be combined with cleanup')
        print(canonical_json(restore(args.repo, _read(root / 'reviews' / str(args.restore) / 'state.git.json'))).decode())
        return
    if args.watch_seconds is not None and (args.watch_seconds < 30 or not args.apply):
        parser.error('--watch-seconds requires --apply and an interval of at least 30 seconds')
    # This is separate from coordinator.lock: never hold the native lock while
    # reading Git or constructing recovery records. Serializes one-off/watcher
    # housekeeping; background collectors recheck the file under the native lock.
    lease = (root / 'review-snapshot-cleanup.lock').open('a+b') if args.apply else nullcontext()
    with lease as lock:
        if lock is not None:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        collector = Collector(root, args.repo)
        while True:
            ids = args.review if args.review is not None else sorted(
                int(p.name) for p in (root / 'reviews').iterdir()
                if p.name.isdigit() and any((p / name).exists() or (p / name).is_symlink()
                    for name in ('state.json', 'state.json.gz')))
            for review_id in ids:
                try:
                    result = collector.retire(review_id, apply=args.apply)
                except Exception as error:
                    result = dict(review_id=review_id, status='retained', reason=str(error))
                print(json.dumps(result, sort_keys=True), flush=True)
            if args.watch_seconds is None:
                return
            # No service or restart policy is installed. This compatibility
            # helper ends when the old coordinator stops; upgraded coordinators
            # run the same collector themselves.
            with _regular(root / 'owner.lock').open('rb') as owner:
                try:
                    fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    pass
                else:
                    return
            time.sleep(args.watch_seconds)


if __name__ == '__main__':
    main()

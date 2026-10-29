"""Retire optional publication caches only after reader-capable deployment.

Keep the immutable journal, source copies and exact committed receipt. The
native reader acknowledges a retired journal only for its already committed,
resolved accepted task; it cannot reinstall caches or authorize publication.
"""
import hashlib
import json
from pathlib import Path
import re
import threading

from . import retained_files as retained, terminal_retention as terminal
from .auxiliary_retention import _binding, _parents
from .candidate import canonical_json, digest, _sync_dir
from .publication import atomic
from .publication_artifacts import paths as artifact_paths

FORMAT = 'trellis-publication-artifacts-retired/1'
MARKER = 'artifacts-retired.json'
MAX_RECORD = 64 << 20


def _sha(value):
    return isinstance(value, str) and re.fullmatch('[0-9a-f]{64}', value) is not None


def _read(path, limit):
    with retained.regular_path(path).open('rb') as stream:
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError('publication retirement metadata exceeds supported bound')
    return raw


def _json(raw):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate publication metadata key')
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=unique)


def _native_payload_sha(event):
    # Rust serde_json serializes UTF-8 strings directly. The journal's own
    # identity instead uses candidate.canonical_json (ASCII escapes).
    return digest(json.dumps(event, sort_keys=True, separators=(',', ':'),
                             ensure_ascii=False, allow_nan=False).encode('utf-8'))


def _listed(record):
    if (set(record) != {'version', 'task_id', 'candidate_sha256', 'repo', 'before', 'after',
                        'changes', 'kernel_event', 'artifacts'}
            or type(record.get('version')) is not int):
        raise ValueError('unsupported publication journal schema')
    for field in ('before', 'after'):
        if not isinstance(record[field], dict) or any(not isinstance(k, str) or not _sha(v)
                for k, v in record[field].items()):
            raise ValueError('invalid publication source manifest')
    if not isinstance(record['changes'], dict) or any(not isinstance(k, str)
            or not isinstance(v, dict) or set(v) != {'before', 'after'}
            or any(sha is not None and not _sha(sha) for sha in v.values())
            for k, v in record['changes'].items()):
        raise ValueError('invalid publication change inventory')
    handoff = record.get('artifacts')
    if (record.get('version') != 2 or not isinstance(handoff, dict)
            or set(handoff) - {'version', 'nodes', 'portable_environments'}
            or type(handoff.get('version')) is not int or handoff['version'] != 1
            or not isinstance(handoff.get('nodes'), dict)):
        raise ValueError('unsupported publication cache inventory')
    result = {}
    for node, entry in handoff['nodes'].items():
        if (not isinstance(entry, dict) or set(entry) != {'source_closure_sha256', 'artifact_bundle', 'files'}
                or not _sha(entry['source_closure_sha256']) or not isinstance(entry['artifact_bundle'], list)):
            raise ValueError('invalid publication artifact node')
        for part in entry['artifact_bundle']:
            if (not isinstance(part, dict) or set(part) != {'level', 'sha256', 'size_bytes'}
                    or not isinstance(part['level'], str) or not _sha(part['sha256'])
                    or type(part['size_bytes']) is not int or not 0 <= part['size_bytes'] < 2**64):
                raise ValueError('invalid publication artifact bundle')
        files = entry['files']
        if not isinstance(files, dict) or set(files) != set(artifact_paths(node)):
            raise ValueError('publication cache file inventory differs')
        for name, sha in files.items():
            if sha is not None:
                if not _sha(sha):
                    raise ValueError('invalid publication artifact hash')
                result['artifacts/' + name] = sha
    environments = handoff.get('portable_environments', [])
    if (not isinstance(environments, list) or len(environments) > 128
            or any(not _sha(sha) for sha in environments) or len(set(environments)) != len(environments)):
        raise ValueError('invalid publication environment inventory')
    for sha in environments:
        result[f'.trellis/kernel-replay-environments/{sha}.json'] = sha
    return result


def _marker(raw, identity, receipt):
    value = _json(raw)
    if (not isinstance(value, dict) or set(value) != {'format', 'journal_sha256', 'receipt', 'complete'}
            or value['format'] != FORMAT or value['journal_sha256'] != identity
            or canonical_json(value['receipt']) != canonical_json(receipt)
            or type(value['complete']) is not bool):
        raise ValueError('publication cache retirement marker differs')
    return value


class Collector:
    def __init__(self, collector, admission=None):
        self.collector, self.admission = collector, admission
        self._catalog = {}
        self._indexed = False
        self._catalog_lock = threading.Lock()

    def invalidate_catalog(self):
        # Called at each finite drain and when native publication observations
        # change. Negative results never suppress a later eligible journal.
        with self._catalog_lock:
            self._indexed = False

    def _journals(self, receipt):
        """Small committed receipts index journals; never scan payloads per owner."""
        key = canonical_json(receipt)
        with self._catalog_lock:
            if not self._indexed:
                self._catalog = {}
                root = self.collector.root / 'publication'
                if not root.is_dir() or root.is_symlink():
                    return ()
                for folder in root.iterdir():
                    if not _sha(folder.name) or folder.is_symlink() or not folder.is_dir():
                        continue
                    committed = folder / 'committed.json'
                    if not committed.exists() and not committed.is_symlink():
                        continue
                    value = _json(_read(committed, 4096))
                    self._catalog.setdefault(canonical_json(value), set()).add(folder)
                self._indexed = True
            return tuple(sorted(self._catalog.get(key, ())))

    def retire(self, directory, observed, current, *, apply=False):
        c = self.collector
        result = dict(status='ineligible', journals=0, candidates=0, removed=0, removed_bytes=0,
                      removed_allocated_bytes=0, marker_allocated_bytes=0)
        _, directory = retained.owner_directory(c.root, Path(directory))
        if not directory.name.isdigit() or directory.name not in observed or c.cancelled():
            return result
        expected = current()
        if expected is None:
            return result
        for folder in self._journals(expected['receipt']):
            value = self._retire_journal(folder, directory, observed, expected, current, apply)
            for key in result:
                if key != 'status':
                    result[key] += value.get(key, 0)
            result['status'] = value['status']
        return result

    def _retire_journal(self, folder, directory, observed, expected, current, apply):
        from .storage import _attempt_directories
        c = self.collector
        result = dict(status='ineligible', journals=0, candidates=0, removed=0, removed_bytes=0,
                      removed_allocated_bytes=0, marker_allocated_bytes=0)
        if self.admission is not None:
            self.admission()
        witness = c.witness()
        if (witness.get('publication_cache_candidates', {}).get(directory.name) != expected
                or witness['owners'].get(directory.name) != observed[directory.name]):
            return dict(result, status='not_committed')
        record_path, committed_path, marker_path = folder/'record.json', folder/'committed.json', folder/MARKER
        signatures = {p: retained._signature(retained.regular_path(p).stat()) for p in (record_path, committed_path)}
        parents = {p: _parents(p, c.root) for p in signatures}
        marker = None
        marker_signature = None
        if marker_path.exists() or marker_path.is_symlink():
            marker_signature = retained._signature(retained.regular_path(marker_path).stat())
            marker = _marker(_read(marker_path, 4096), folder.name, expected['receipt'])
            if marker['complete']:
                return dict(result, status='complete')  # Skip only, never authorizes new deletion.
        raw = _read(record_path, MAX_RECORD)
        if digest(raw) != folder.name:
            raise ValueError('publication journal bytes differ from identity')
        record = _json(raw)
        if (type(record.get('task_id')) is not int or record['task_id'] != expected['task_id']
                or record.get('candidate_sha256') != expected['receipt']['candidate_sha256']
                or record.get('repo') != str(c.repo)
                or canonical_json(_json(_read(committed_path, 4096))) != canonical_json(expected['receipt'])
                or _native_payload_sha(record['kernel_event']) != expected['receipt']['payload_sha256']):
            raise ValueError('publication differs from exact native receipt')
        if record.get('version') == 1 and record.get('artifacts') is None:
            return dict(result, status='no_optional_cache')
        listed = _listed(record)
        if not listed:
            return dict(result, status='no_optional_cache')
        guard = retained.terminal_owner_guard(c.root, directory, observed)
        binding = _binding(directory, c, witness, dict(kind='worker', id=int(directory.name)))
        pins = [attempt/'retain-builds.json' for attempt in _attempt_directories(directory)]
        candidates, absent_parents = {}, {}
        for name, sha in listed.items():
            path = folder/name
            parent = _parents(path, c.root)
            if not path.exists() and not path.is_symlink():
                if marker is None:
                    raise ValueError('unretired publication cache file is missing')
                absent_parents[path.parent] = parent
                continue
            signature = retained._signature(retained.regular_path(path).stat())
            hashed = hashlib.sha256()
            with path.open('rb') as stream:
                while True:
                    if c.cancelled():
                        return dict(result, status='cancelled')
                    block = stream.read(1 << 20)
                    if not block:break
                    hashed.update(block)
            if hashed.hexdigest() != sha or retained._signature(retained.regular_path(path).stat()) != signature:
                raise ValueError('publication cache content changed')
            if not retained._untracked_cache(path, c.root):
                return dict(result, status='tracked')
            candidates[path] = (signature, parent)
        if any(retained._signature(retained.regular_path(p).stat()) != sig for p,sig in signatures.items()):
            raise ValueError('publication journal changed during preparation')
        if not apply:
            return dict(result, status='eligible', candidates=len(candidates))

        def fence():
            if c.cancelled():raise InterruptedError('publication retirement cancelled')
            if self.admission is not None:self.admission()
            if current() != expected or not c.checkpoint_current(witness):
                raise ValueError('publication native receipt or checkpoint changed')
            if (c.root/'publication/pending.json').exists() or (c.root/'publication/pending.json').is_symlink():
                raise ValueError('publication transaction is pending')
            guard()
            if list(retained._signature(retained.regular_path(directory/'prepared.json').stat())) != binding['prepared_signature']:
                raise ValueError('publication owner preparation changed')
            for p,sig in signatures.items():
                if retained._signature(retained.regular_path(p).stat()) != sig or _parents(p,c.root) != parents[p]:
                    raise ValueError('publication journal changed before retirement')
            if not retained._untracked_cache(marker_path, c.root):
                raise ValueError('publication retirement marker is tracked')

        if marker is None:
            with c.retirement_lock() as acquired:
                if not acquired:return dict(result,status='coordinator_busy')
                fence()
                if marker_path.exists() or marker_path.is_symlink():
                    raise ValueError('publication retirement marker appeared')
                marker = dict(format=FORMAT,journal_sha256=folder.name,receipt=expected['receipt'],complete=False)
                atomic(marker_path,canonical_json(marker))
                marker_signature = retained._signature(marker_path.stat())
                result['marker_allocated_bytes'] += marker_path.stat().st_blocks*512
        # Re-establish marker durability after an interrupted atomic replace.
        with retained.regular_path(marker_path).open('rb') as stream:
            import os
            os.fsync(stream.fileno())
        _sync_dir(folder)
        dirty = dict(absent_parents)
        try:
            with c.retirement_lock() as acquired:
                if not acquired:return dict(result,status='coordinator_busy')
                fence()
                if retained._signature(retained.regular_path(marker_path).stat()) != marker_signature:
                    raise ValueError('publication retirement marker changed')
                for path,(signature,parent) in candidates.items():
                    if c.cancelled():return dict(result,status='cancelled')
                    if any(pin.exists() or pin.is_symlink() for pin in pins):
                        raise ValueError('publication owner is pinned')
                    if (_parents(path,c.root) != parent
                            or retained._signature(retained.regular_path(path).stat()) != signature
                            or not retained._untracked_cache(path, c.root)):
                        raise ValueError('publication cache path changed before unlink')
                    info=path.stat();path.unlink();dirty[path.parent]=parent
                    result['removed']+=1;result['removed_bytes']+=info.st_size
                    result['removed_allocated_bytes']+=info.st_blocks*512
        finally:
            for parent,expected_parents in dirty.items():
                if _parents(parent/'.retirement-sync',c.root) != expected_parents:
                    raise ValueError('publication cache parent changed before sync')
                _sync_dir(parent)
        with c.retirement_lock() as acquired:
            if not acquired:return dict(result,status='coordinator_busy')
            fence()
            if retained._signature(retained.regular_path(marker_path).stat()) != marker_signature:
                raise ValueError('publication retirement marker changed before completion')
            if any((folder/name).exists() or (folder/name).is_symlink() for name in listed):
                return dict(result,status='cache_reappeared')
            info=marker_path.stat();atomic(marker_path,canonical_json(dict(marker,complete=True)))
            result['removed_allocated_bytes']+=info.st_blocks*512
            result['marker_allocated_bytes']+=marker_path.stat().st_blocks*512
            result['journals']=1
        return dict(result,status='retired')

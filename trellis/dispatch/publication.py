"""Recoverable source publication around the existing kernel event commit.

Only host-checked, rebased work enters here. This journal does not approve it.
The caller supplies the ordinary authoritative kernel commit, idempotent by
attempt/candidate identity. All canonical writers share this file lock.
"""
from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
import copy
import fcntl
import json
import os
from pathlib import Path
import shutil
import tempfile

from .candidate import CandidateError, canonical_json, digest, regular_bytes, source_manifest, _sync_dir, _write


_RENDERED_SUPPORT = frozenset({'Tablet/INDEX.md', 'Tablet/README.md'})


class PublicationChanged(CandidateError):
    """An uncommitted packet needs a fresh captured canonical source view."""


@dataclass
class PreparedPublication:
    directory: Path
    staged: Path
    record: dict
    config_sha256: str | None


def _event_material(event):
    # Only native event bookkeeping may move while the same packet is staged.
    # In particular all refreshed certificates, claims and artifact bindings
    # remain exact, even when their owner set has not changed.
    material = dict(event)
    material.pop('base_state_sha256', None)
    material.pop('decided_at_ms', None)
    if isinstance(material.get('response'), dict):
        material['response'] = dict(material['response'])
        material['response'].pop('request_id', None)
        material['response'].pop('cycle', None)
    return material


def _config_digest(repo):
    content = regular_bytes(repo, 'trellis.config.json')
    return digest(content) if content is not None else None


def atomic(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temp = tempfile.mkstemp(prefix='.publish-', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, path)
        _sync_dir(path.parent)
    finally:
        Path(temp).unlink(missing_ok=True)


class Publisher:
    def __init__(self, runtime, repo):
        self.root = Path(runtime) / 'dispatch/publication'
        self.repo = Path(repo).resolve()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        if self.root.resolve().is_relative_to(self.repo):
            raise CandidateError('publication evidence must remain outside agent repositories')

    @contextmanager
    def locked(self):
        with (self.root.parent / 'coordinator.lock').open('a+b') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            try:
                yield self
            finally:
                fcntl.flock(lock, fcntl.LOCK_UN)

    def _merged_sources(self, staged, checked, baseline, expected, temporary, config_sha256):
        """Apply checked changes to current sources, then render their shared docs."""
        after = dict(expected)
        for path in sorted(set(baseline) | set(checked)):
            if path in _RENDERED_SUPPORT or baseline.get(path) == checked.get(path):
                continue
            if expected.get(path) != baseline.get(path):
                raise CandidateError(f'checked source conflicts with current publication: {path}')
            if path in checked:
                after[path] = checked[path]
            else:
                after.pop(path, None)

        # Only source bytes are needed by the native renderer. Keep this view
        # temporary; neither the checked workspace nor canonical files change.
        merged = temporary / 'merged'
        (merged / 'Tablet').mkdir(parents=True)
        render_inputs = {path: identity for path, identity in after.items()
                         if path.startswith('Tablet/') and path not in _RENDERED_SUPPORT}
        for path, identity in render_inputs.items():
            source = staged if baseline.get(path) != checked.get(path) else self.repo
            data = regular_bytes(source, path)
            if data is None or digest(data) != identity:
                raise (PublicationChanged if source == self.repo else CandidateError)(
                    f'publication source changed during support rendering: {path}')
            destination = merged / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(data)  # Scratch only; durable files are written below.
        config = regular_bytes(self.repo, 'trellis.config.json')
        if (digest(config) if config is not None else None) != config_sha256:
            raise PublicationChanged('publication renderer configuration changed')
        if config is not None:
            (merged / 'trellis.config.json').write_bytes(config)
        from trellis.runtime.kernel_cli import run_kernel_cli
        result = run_kernel_cli(dict(action='dispatch_render_tablet_support', repo_path=str(merged)))
        output = result.get('output')
        if result.get('status') != 'dispatch_ok' or not isinstance(output, dict):
            raise CandidateError('native publication support rendering failed')
        rendered = {}
        for path, field in [('Tablet/INDEX.md', 'index_md_content'), ('Tablet/README.md', 'readme_md_content')]:
            if not isinstance(output.get(field), str):
                raise CandidateError('native publication support rendering returned invalid content')
            rendered[path] = output[field].encode()
            after[path] = digest(rendered[path])
        if source_manifest(merged) != render_inputs or _config_digest(merged) != config_sha256:
            raise CandidateError('publication support rendering changed its source inputs')
        shutil.rmtree(merged)
        return after, rendered

    def stage(self, task_id, candidate_sha256, staged, *, expected_manifest, kernel_event,
                artifact_source=None, checked_baseline=None):
        """Stage immutable recovery bytes without taking the canonical lock.

        No sources or pending pointer are written. The caller retains the
        finalization gate, then revalidates current native inputs before activate.
        A crash leaves only disposable .prepare-* files, never a decision.
        """
        if type(task_id) is not int or task_id < 1:
            raise CandidateError('publication requires a positive task ID')
        if len(candidate_sha256) != 64 or any(c not in '0123456789abcdef' for c in candidate_sha256):
            raise CandidateError('publication requires an exact candidate digest')
        if (self.root / 'pending.json').exists():
            raise CandidateError('recover the pending publication before preparing another')
        if source_manifest(self.repo) != expected_manifest:
            raise PublicationChanged('canonical sources changed during publication preparation')
        staged = Path(staged).resolve()
        if staged == self.repo or staged.is_relative_to(self.repo):
            raise CandidateError('publication requires an isolated checked workspace')
        checked = source_manifest(staged)
        config_sha256 = _config_digest(self.repo)
        temp = Path(tempfile.mkdtemp(prefix='.prepare-', dir=self.root))
        try:
            if artifact_source is not None:
                from . import acceptance_receipts, publication_artifacts
                attempt = Path(artifact_source)
                nested = (attempt.name.isdigit() and attempt.parent.name == 'finalization'
                          and attempt.parent.parent.name.isdigit()
                          and attempt.parent.parent.parent.name == 'acceptance'
                          and attempt.parent.parent.parent.parent == self.root.parent / str(task_id))
                if (staged != attempt / 'repo' or not nested
                        and attempt.parent.parent != self.root.parent / str(task_id)):
                    raise CandidateError('artifact source is not this task host acceptance')
                capture = json.loads(acceptance_receipts.regular(attempt / 'canonical.json'))
                if nested:
                    from . import staged_acceptance
                    completed = staged_acceptance.reusable_finalizer(attempt, capture['key'])
                    if (completed is None or completed.get('stage') != 'finalize'
                            or completed.get('status') != 'checked'
                            or completed.get('candidate_sha256') != candidate_sha256):
                        raise CandidateError('artifact source has no intact staged finalization receipt')
                elif acceptance_receipts.reusable(attempt, capture['key']) is None:
                    raise CandidateError('artifact source has no intact host acceptance receipt')
                if (checked_baseline is not None and checked_baseline !=
                        json.loads(acceptance_receipts.regular(attempt / 'base-manifest.json'))):
                    raise CandidateError('publication baseline differs from authenticated host acceptance')
            after, rendered = (self._merged_sources(staged, checked, checked_baseline, expected_manifest, temp, config_sha256)
                               if checked_baseline is not None else (checked, {}))
            changes = {p: {'before': expected_manifest.get(p), 'after': after.get(p)}
                       for p in sorted(set(expected_manifest) | set(after))
                       if expected_manifest.get(p) != after.get(p)}
            record = dict(version=1, task_id=task_id, candidate_sha256=candidate_sha256,
                          repo=str(self.repo), before=expected_manifest, after=after,
                          changes=changes, kernel_event=kernel_event)
            if artifact_source is not None:
                record['version'] = 2
                record['artifacts'] = publication_artifacts.stage(staged, self.repo, temp, kernel_event['response'])
            for relative, change in changes.items():
                if change['after'] is not None:
                    data = rendered[relative] if relative in rendered else regular_bytes(staged, relative)
                    if data is None or digest(data) != change['after']:
                        raise CandidateError('checked source changed while preparing publication')
                    _write(temp / 'files' / relative, data)
            if source_manifest(staged) != checked:
                raise CandidateError('checked sources changed while preparing the journal')
            if source_manifest(self.repo) != expected_manifest or _config_digest(self.repo) != config_sha256:
                raise PublicationChanged('canonical sources changed while preparing the journal')
            for directory, _, _ in os.walk(temp, topdown=False):
                _sync_dir(directory)
            return PreparedPublication(temp, staged, copy.deepcopy(record), config_sha256)
        except BaseException:
            shutil.rmtree(temp)
            raise

    def matches(self, packet, task_id, candidate_sha256, staged, *, expected_manifest, kernel_event):
        """Caller holds the canonical lock and has recomputed the native event."""
        return (packet.staged == Path(staged).resolve()
            and packet.record['task_id'] == task_id
            and packet.record['candidate_sha256'] == candidate_sha256
            and packet.record['before'] == expected_manifest
            and packet.config_sha256 == _config_digest(self.repo)
            and _event_material(packet.record['kernel_event']) == _event_material(kernel_event))

    def activate(self, packet, *, kernel_event):
        """Under locked(): arm the unchanged journal, then recover without unlocking."""
        if (self.root / 'pending.json').exists():
            raise CandidateError('recover the pending publication before activating another')
        if not self.matches(packet, packet.record['task_id'], packet.record['candidate_sha256'],
                packet.staged, expected_manifest=source_manifest(self.repo), kernel_event=kernel_event):
            raise PublicationChanged('publication packet no longer matches current inputs')
        if packet.directory.parent != self.root or not packet.directory.name.startswith('.prepare-'):
            raise CandidateError('publication packet is not an uncommitted preparation')
        record = dict(packet.record, kernel_event=kernel_event)
        content = canonical_json(record)
        identity = digest(content)
        destination = self.root / identity
        _write(packet.directory / 'record.json', content)
        _sync_dir(packet.directory)
        if destination.exists():
            if (destination / 'record.json').read_bytes() != content:
                raise CandidateError('publication identity collision')
        else:
            packet.directory.rename(destination)
            _sync_dir(self.root)
        # The durable decision remains exactly here. No source write precedes
        # this pointer; the ordinary recovery/native checkpoint owns the rest.
        atomic(self.root / 'pending.json', canonical_json({'identity': identity}))
        return identity

    def discard(self, packet):
        if packet is not None and packet.directory.exists():
            if packet.directory.parent != self.root or not packet.directory.name.startswith('.prepare-'):
                raise CandidateError('refusing to discard committed publication evidence')
            shutil.rmtree(packet.directory)

    def discard_unused_preparations(self):
        """Startup only, under coordinator ownership before integrations start."""
        for directory in self.root.glob('.prepare-*'):
            if directory.is_symlink() or not directory.is_dir():
                raise CandidateError('invalid unused publication preparation')
            shutil.rmtree(directory)

    def prepare(self, task_id, candidate_sha256, staged, *, expected_manifest, kernel_event,
                artifact_source=None, checked_baseline=None):
        """Compatibility path for callers already holding locked()."""
        packet = self.stage(task_id, candidate_sha256, staged, expected_manifest=expected_manifest,
            kernel_event=kernel_event, artifact_source=artifact_source, checked_baseline=checked_baseline)
        try:
            return self.activate(packet, kernel_event=kernel_event)
        finally:
            self.discard(packet)

    def recover(self, commit_kernel, *, publication_receipt=None, checkpoint=lambda _stage: None):
        """Resume under locked(); commit_kernel must query/commit by exact identity.

        It must never rerun an agent. Once this durable publication starts,
        restart finishes that same transaction.
        """
        pending = self.root / 'pending.json'
        if not pending.exists():
            return None
        identity = json.loads(pending.read_text())['identity']
        if not isinstance(identity, str) or len(identity) != 64 or any(c not in '0123456789abcdef' for c in identity):
            raise CandidateError('invalid publication journal identity')
        directory = self.root / identity
        content = regular_bytes(directory, 'record.json')
        if content is None or digest(content) != identity:
            raise CandidateError('publication journal was modified')
        record = json.loads(content)
        if record.get('version') not in (1, 2) or record.get('repo') != str(self.repo):
            raise CandidateError('publication journal belongs to a different repository')
        if record['version'] == 2:
            if publication_receipt is None:
                raise CandidateError('artifact recovery requires an exact native publication receipt query')
            receipt = publication_receipt(record['task_id'], record['candidate_sha256'], record['kernel_event'])
            if receipt is not None:
                # Native commit may have rebuilt a cold/mismatched cache and
                # issued different exact artifacts. Never roll those bytes
                # back to the precommit packet after a crash or RPC timeout.
                atomic(directory / 'committed.json', canonical_json(receipt))
                pending.unlink()
                _sync_dir(self.root)
                return receipt
        current = source_manifest(self.repo)
        # Validate the *entire* tree before any write; unexpected additions or
        # modifications mean another writer ignored the lock. Preserve evidence.
        for p in set(current) | set(record['before']) | set(record['after']):
            if current.get(p) not in (record['before'].get(p), record['after'].get(p)):
                raise CandidateError(f'publication recovery found an unrelated source change: {p}')
        for p, change in record['changes'].items():
            if change['after'] is not None:
                data = regular_bytes(directory / 'files', p)
                if data is None or digest(data) != change['after']:
                    raise CandidateError(f'publication recovery evidence was modified: {p}')
        if record['version'] == 2:
            from . import publication_artifacts
            publication_artifacts.validate(directory, record['artifacts'])
        for p, change in record['changes'].items():
            if current.get(p) == change['after']:
                continue
            if change['after'] is None:
                (self.repo / p).unlink()
                _sync_dir((self.repo / p).parent)
            else:
                atomic(self.repo / p, regular_bytes(directory / 'files', p))
            checkpoint('source:' + p)
        if source_manifest(self.repo) != record['after']:
            raise CandidateError('published source manifest differs from publication journal')
        if record['version'] == 2:
            if record['artifacts']['nodes']:
                publication_artifacts.install_via_checker(self.root.parent.parent, identity)
            checkpoint('artifacts_installed')
        # The kernel handles its event/checkpoint durability using the existing
        # Trellis transaction. Its replay receipt prevents a double application
        # if the operator dies between that commit and our final marker.
        receipt = commit_kernel(record['task_id'], record['candidate_sha256'], record['kernel_event'])
        checkpoint('kernel_committed')
        atomic(directory / 'committed.json', canonical_json(receipt))
        pending.unlink()
        _sync_dir(self.root)
        return receipt

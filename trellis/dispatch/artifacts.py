"""Preserve referenced exclusive-lane evidence before its native transition."""
import json
from pathlib import Path, PurePosixPath
import stat

from .candidate import CandidateError, canonical_json, digest
from .publication import atomic


def probe_bytes(repo, relative):
    path = PurePosixPath(relative)
    if (str(path) != relative or path.is_absolute() or '..' in path.parts
            or len(path.parts) < 4 or path.parts[:2] != ('.trellis', 'stuck-math-audit')
            or not path.parts[2].startswith('cycle-')):
        raise CandidateError('audit probe must be inside a cycle-* audit scratch directory')
    current = Path(repo)
    for part in path.parts:
        current = current / part
        if current.is_symlink():
            raise CandidateError(f'audit evidence cannot traverse symlinks: {relative}')
    info = current.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        raise CandidateError(f'audit evidence must be an unlinked regular file: {relative}')
    return current.read_bytes()


def publish_probes(directory, canonical, response):
    """Caller holds the native lock, has reaped the lane and checked inputs.

    Files are immutable supplementary evidence, not mathematical acceptance.
    An interrupted copy can be replayed. Existing canonical evidence is never
    overwritten, including if an old path was edited in the private workspace.
    """
    paths = response.get('probe_paths', []) if response and response.get('status') == 'Ok' else []
    if not paths:
        return
    directory, canonical = Path(directory), Path(canonical)
    captured = {p: probe_bytes(directory / 'repo', p) for p in paths}
    manifest = {p: digest(data) for p, data in captured.items()}
    marker = directory / 'probe-publication.json'
    if marker.exists() and json.loads(marker.read_text()) != manifest:
        raise CandidateError('audit probe evidence changed after its publication decision')
    for relative, data in captured.items():
        path = canonical / relative
        if path.exists() or path.is_symlink():
            if probe_bytes(canonical, relative) != data:
                raise CandidateError(f'audit probe conflicts with retained evidence: {relative}')
        else:
            for parent in path.parents:
                if parent == canonical:
                    break
                if parent.is_symlink():
                    raise CandidateError(f'audit evidence destination traverses a symlink: {relative}')
    atomic(marker, canonical_json(manifest))
    for relative, data in captured.items():
        if not (canonical / relative).exists():
            atomic(canonical / relative, data)

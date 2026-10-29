"""Decode task-private snapshots without changing their physical receipt bytes.

Canonical runtime state is still written as plain ProtocolState JSON. Private
readers accept historical plain files and the self-contained shared-state
carrier. Hashing/authentication of the stored file remains the caller's job.
"""
import json
import hashlib
import os
from pathlib import Path
import stat
import subprocess
import sys

from .history_artifacts import decode_shared_state


def decode_private_state(document):
    if not isinstance(document, dict):
        raise ValueError('private protocol state must be an object')
    if '$format' not in document:
        return document
    if set(document) != {'$format', '$strings', '$pool', 'state'}:
        raise ValueError('private shared-state carrier has unexpected members')
    state = decode_shared_state(document)['state']
    if not isinstance(state, dict):
        raise ValueError('private shared-state payload must be an object')
    return state


def parse_private_state(raw):
    return decode_private_state(json.loads(raw))


def read_private_state(path):
    return parse_private_state(Path(path).read_bytes())


def _small_plain(state, maximum=65536):
    """Bound the small-fixture fast path without serializing a huge state twice."""
    chunks, size = [], 0
    for chunk in json.JSONEncoder(sort_keys=True, separators=(',', ':'), ensure_ascii=True).iterencode(state):
        raw = chunk.encode()
        size += len(raw)
        if size >= maximum:
            return None
        chunks.append(raw)
    return b''.join(chunks)


def _write_new(path, raw):
    path = Path(path)
    if not path.is_absolute() or path.name != 'protocol_state.json':
        raise ValueError('private snapshot destination must be an absolute protocol_state.json path')
    if path.parent.resolve() != path.parent:
        raise ValueError('private snapshot parent traverses a symlink')
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(raw); stream.flush(); os.fsync(stream.fileno())
        fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try: os.fsync(fd)
        finally: os.close(fd)
    except BaseException:
        path.unlink(missing_ok=True)
        raise


def write_private_state(path, state, *, kernel=None):
    """Create a snapshot; physical file hashes and decoded identities stay distinct."""
    path = Path(path)
    if not isinstance(state, dict) or '$format' in state or state.get('dispatch') is not None:
        raise ValueError('private snapshot must be a plain state without a dispatch ledger')
    small = _small_plain(state)
    if small is not None:
        _write_new(path, small)
        return
    if kernel is None:
        from .runtime.kernel_cli import run_kernel_cli
        kernel = run_kernel_cli
    result = kernel(dict(action='write_private_state', path=str(path), state=state))
    if result.get('status') != 'dispatch_ok':
        raise RuntimeError('private snapshot writer returned an unexpected response')
    output = result['output']
    if output == {'status': 'unsupported_float'}:
        # Only this explicit outcome permits a fallback. Native errors or I/O
        # failures must not become silent alternate writes.
        if path.exists() or path.is_symlink():
            raise RuntimeError('native float fallback unexpectedly created its destination')
        payload = json.dumps(state, separators=(',', ':'), ensure_ascii=True).encode()
        source = Path(__file__).resolve().parent.parent
        environment = dict(os.environ)
        environment['PYTHONPATH'] = str(source) + os.pathsep + environment.get('PYTHONPATH', '')
        child = subprocess.run([sys.executable, '-m', 'trellis.private_state', str(path)],
            input=payload, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, env=environment, cwd=source)
        output = json.loads(child.stdout)
    if output.get('status') != 'written':
        raise RuntimeError('private snapshot writer did not confirm completion')
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size != output.get('bytes'):
        raise RuntimeError('private snapshot writer output changed')
    with path.open('rb') as stream:
        if hashlib.file_digest(stream, 'sha256').hexdigest() != output.get('sha256'):
            raise RuntimeError('private snapshot writer bytes changed')


def _main():
    from .history_artifacts import encode_shared_state
    from .dispatch.candidate import canonical_json
    state = json.load(sys.stdin)
    if not isinstance(state, dict) or '$format' in state or state.get('dispatch') is not None:
        raise ValueError('invalid private snapshot float fallback input')
    raw = canonical_json(encode_shared_state({'state': state}))
    _write_new(Path(sys.argv[1]), raw)
    print(json.dumps(dict(status='written', bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest())))


if __name__ == '__main__':
    _main()

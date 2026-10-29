"""Exact frozen request objects shared by dispatch history documents.

The live protocol archives only resolved requests. Checkpoint storage may also
reference active requests; readers hydrate those before ordinary validation.
Objects use the existing ZIP/DEFLATE format, with one bounded JSON entry.
"""
from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import stat
import tempfile
import zipfile

FORMAT = 'trellis-dispatch-request/1'
DIRECTORY = '.trellis-history/dispatch-requests'


def _execution(context):
    if context is None:
        return None
    if not isinstance(context, dict) or set(context) != {'task_id', 'profile', 'binding', 'compiler', 'task_kind'}:
        raise ValueError('invalid task execution binding')
    # Reuse launch/restart normalization for historical defaulted profile
    # fields; never normalize or rewrite the exact hashed request bytes.
    from trellis.dispatch.config import parse
    binding = parse({'enabled': True, 'profiles': {'frozen': context['binding']}})['profiles']['frozen']
    return dict(context, binding=binding)


def _validate(reference):
    if (not isinstance(reference, dict) or set(reference) != {'$dispatch_request', 'sha256', 'bytes', 'task_execution'}
            or reference['$dispatch_request'] != FORMAT
            or not isinstance(reference['sha256'], str) or len(reference['sha256']) != 64
            or any(c not in '0123456789abcdef' for c in reference['sha256'])
            or type(reference['bytes']) is not int or reference['bytes'] <= 0):
        raise ValueError('invalid frozen dispatch request reference')


def _directory(repo, *, create=False):
    path = Path(repo)
    for part in ('.trellis-history', 'dispatch-requests'):
        path /= part
        if create:
            try:
                path.mkdir()
                fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(fd)
                finally:
                    os.close(fd)
            except FileExistsError:
                pass
        if not stat.S_ISDIR(path.lstat().st_mode):
            raise ValueError('dispatch history directory is not a regular directory')
    return path


def resolve(repo, reference, *, read_object=None):
    try:
        return _resolve(repo, reference, read_object=read_object)
    except (OSError, ValueError, zipfile.BadZipFile) as error:
        identity = reference.get('sha256', '<invalid reference>') if isinstance(reference, dict) else '<invalid reference>'
        raise ValueError(f'dispatch request {identity}: {error}') from error


def _resolve(repo, reference, *, read_object=None):
    _validate(reference)
    name = f"{reference['sha256']}.zip"
    if read_object is None:
        path = _directory(repo) / name
        if not stat.S_ISREG(path.lstat().st_mode):
            raise ValueError('dispatch request object is not a regular file')
        source = path
    else:
        source = io.BytesIO(read_object(f'{DIRECTORY}/{name}'))
    with zipfile.ZipFile(source) as archive:
        entries = archive.infolist()
        if len(entries) != 1 or entries[0].filename != 'request.json' or entries[0].file_size != reference['bytes']:
            raise ValueError('dispatch request archive has wrong entry shape or length')
        with archive.open(entries[0]) as stream:
            raw = stream.read(reference['bytes'] + 1)
    if len(raw) != reference['bytes'] or hashlib.sha256(raw).hexdigest() != reference['sha256']:
        raise ValueError('frozen dispatch request object has wrong size or digest')
    request = json.loads(raw)
    if not isinstance(request, dict) or _execution(request.get('task_execution')) != _execution(reference['task_execution']):
        raise ValueError('frozen dispatch request object has wrong binding or encoding')
    return request


def checkpoint_requests(document, repo, *, store=False, references=None, read_object=None):
    """Copy only dispatch headers being changed; all other evidence is retained.

    Stored active references hydrate to exact inline requests. Terminal
    references remain compact, but are verified at this history boundary.
    """
    state = document.get('state', document)
    dispatch = state.get('dispatch')
    if not dispatch:
        return document
    tasks = {}
    for key, original in dispatch['tasks'].items():
        task = dict(original)
        assignment = dict(task['assignment'])
        request = assignment['request']
        stored = isinstance(request, dict) and '$stored_dispatch_request' in request
        archived = isinstance(request, dict) and '$dispatch_request' in request
        if stored:
            if set(request) != {'$stored_dispatch_request'}:
                raise ValueError('malformed stored dispatch request')
            full = resolve(repo, request['$stored_dispatch_request'], read_object=read_object)
            if not store:
                assignment['request'] = full
        elif archived:
            if store:
                _validate(request)
            else:
                resolve(repo, request, read_object=read_object)
        elif store and references and key in references:
            reference = references[key]
            if resolve(repo, reference, read_object=read_object) != request:
                raise ValueError('checkpoint object differs from exact frozen request')
            assignment['request'] = {'$stored_dispatch_request': reference}
        task['assignment'] = assignment
        tasks[key] = task
    changed_state = dict(state, dispatch=dict(dispatch, tasks=tasks))
    return dict(document, state=changed_state) if 'state' in document else changed_state


def referenced_requests(state):
    """Distinct exact objects required by one compact protocol state."""
    references = {}
    for task in (state.get('dispatch') or {}).get('tasks', {}).values():
        request = task['assignment']['request']
        if not isinstance(request, dict):
            raise ValueError('invalid frozen dispatch request')
        reference = request.get('$stored_dispatch_request', request)
        if '$dispatch_request' not in reference:
            continue
        _validate(reference)
        assignment = task['assignment']
        context = dict(task_id=task['id'], profile=assignment['profile'], binding=task['binding'],
                       compiler=assignment['compiler'], task_kind=assignment['task_kind'])
        if _execution(reference['task_execution']) != _execution(context):
            raise ValueError('archived request differs from its task execution binding')
        prior = references.setdefault(reference['sha256'], reference)
        if prior != reference:
            raise ValueError('conflicting frozen request references')
    return references


def object_bytes(repo, reference):
    _validate(reference)
    path = _directory(repo) / f"{reference['sha256']}.zip"
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError('dispatch request object is not a regular file')
    raw = path.read_bytes()
    resolve(repo, reference, read_object=lambda _: raw)
    return path, raw


def copy_state_objects(state, source, destination):
    """Preserve the verified relative object closure alongside copied state."""
    for reference in referenced_requests(state).values():
        _, raw = object_bytes(source, reference)
        directory = _directory(destination, create=True)
        path = directory / f"{reference['sha256']}.zip"
        if path.exists() or path.is_symlink():
            resolve(destination, reference)
            continue
        fd, temporary = tempfile.mkstemp(prefix='.request-', suffix='.tmp', dir=directory)
        try:
            with os.fdopen(fd, 'wb') as stream:
                stream.write(raw)
                stream.flush()
                os.fsync(stream.fileno())
            try:
                os.link(temporary, path)
            except FileExistsError:
                resolve(destination, reference)
            fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        finally:
            os.unlink(temporary)


def verify_added_object(path, sha256):
    try:
        _verify_added_object(path, sha256)
    except (OSError, ValueError, zipfile.BadZipFile) as error:
        raise ValueError(f'dispatch request {path}: {error}') from error


def _verify_added_object(path, sha256):
    """Hash new Git objects in bounded chunks; existing immutable objects need
    no repeated inflation at ordinary checkpoint publication.
    """
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError('dispatch request object is not a regular file')
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        if len(entries) != 1 or entries[0].filename != 'request.json' or entries[0].file_size <= 0:
            raise ValueError('dispatch request archive has wrong entry shape')
        count = 0
        digest = hashlib.sha256()
        with archive.open(entries[0]) as stream:
            while chunk := stream.read(min(1024 * 1024, entries[0].file_size - count + 1)):
                count += len(chunk)
                if count > entries[0].file_size:
                    raise ValueError('dispatch request object exceeds its declared length')
                digest.update(chunk)
        if count != entries[0].file_size or digest.hexdigest() != sha256:
            raise ValueError('new dispatch request object has wrong length or digest')

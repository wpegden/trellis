"""Write host-generated files in agent-writable homes without following links."""
from contextlib import contextmanager
import os
from pathlib import Path
import uuid


@contextmanager
def provider_directory(burst_home):
    with safe_directory(Path(os.path.abspath(burst_home)) / '.codex') as opened:
        yield opened


@contextmanager
def safe_directory(directory, *, create=True):
    """Hold the actual directory open throughout preparation and publication.

    Walk from the root with no-follow directory opens. Checking is_symlink
    before a pathname write would leave a check/use race with a renamed parent.
    """
    directory = Path(os.path.abspath(directory))
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    fd = os.open('/', flags)
    try:
        for component in directory.parts[1:]:
            try:
                child = os.open(component, flags, dir_fd=fd)
            except FileNotFoundError:
                if not create:
                    raise
                try:
                    os.mkdir(component, mode=0o700, dir_fd=fd)
                except FileExistsError:
                    pass
                child = os.open(component, flags, dir_fd=fd)
            os.close(fd)
            fd = child
        yield directory, fd
    finally:
        os.close(fd)


def atomic_text(directory, fd, name, text, *, mode=None):
    """Replace the directory entry itself, including an existing symlink."""
    if Path(name).name != name or name in ('', '.', '..'):
        raise ValueError('host-generated filename must be one path component')
    temporary = '.trellis-' + uuid.uuid4().hex
    opened = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                     0o600, dir_fd=fd)
    try:
        with os.fdopen(opened, 'w', encoding='utf-8') as stream:
            stream.write(text)
            stream.flush()
            if mode is not None:
                os.fchmod(stream.fileno(), mode)
            os.fsync(stream.fileno())
        os.replace(temporary, name, src_dir_fd=fd, dst_dir_fd=fd)
        os.fsync(fd)
    finally:
        try:
            os.unlink(temporary, dir_fd=fd)
        except FileNotFoundError:
            pass
    return directory / name

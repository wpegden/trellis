"""Read-only process identification for dispatch status and resume controls."""
import json
from pathlib import Path


def coordinator_pid(runtime: Path, *, proc_root=Path('/proc')) -> int | None:
    """A retained PID record alone is not evidence of a live coordinator."""
    try:
        runtime = runtime.resolve()
        pid = json.loads((runtime / 'dispatch/coordinator.json').read_text())['pid']
        if type(pid) is not int or pid <= 0:
            return None
        args = (proc_root / str(pid) / 'cmdline').read_bytes().split(b'\0')
        if (len(args) < 6 or args[1:3] != [b'-m', b'trellis.dispatch']
                or args[3] not in (b'run', b'resume')):
            return None
        target = Path(args[4].decode())
        return pid if target.is_absolute() and target.resolve() == runtime else None
    except (OSError, ValueError, KeyError, TypeError):
        return None

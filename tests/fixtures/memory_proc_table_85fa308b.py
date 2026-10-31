"""Exact process-table reader from 85fa308b, differential test oracle only."""
import os
from pathlib import Path
_PAGE = os.sysconf("SC_PAGE_SIZE") if hasattr(os, "sysconf") else 4096

def _read_proc_table(proc_root: Path = Path("/proc")) -> dict[int, tuple[int, int, str]]:
    """pid -> (ppid, rss_bytes, comm) for every process /proc lists."""
    table: dict[int, tuple[int, int, str]] = {}
    try:
        entries = os.listdir(proc_root)
    except OSError:
        return table
    for name in entries:
        if not name.isdigit():
            continue
        pid = int(name)
        try:
            stat = (proc_root / name / "stat").read_text()
            statm = (proc_root / name / "statm").read_text()
        except OSError:
            continue
        # `comm` may contain spaces/parens; it ends at the last ')'.
        close = stat.rfind(")")
        comm = stat[stat.find("(") + 1:close]
        fields = stat[close + 2:].split()
        try:
            ppid = int(fields[1])
            rss = int(statm.split()[1]) * _PAGE
        except (IndexError, ValueError):
            continue
        table[pid] = (ppid, rss, comm)
    return table

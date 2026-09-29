"""Opt-in Linux /proc sampling. Lower bounds, never exact child-tree usage.

No join, no waiting for a slot, no subprocesses, no writes. At most eight
daemon threads, 512 tasks/processes per poll, 4096 lifetime identities.
Process identity uses /proc/PID/exe plus stat starttime, never comm/argv.
"""
import os
from pathlib import Path
import threading
import time

from trellis.checker.telemetry import safe

_slots = threading.BoundedSemaphore(8)


class Sampler:
    def __init__(self, pid):
        self.pid = pid
        self.done = threading.Event()
        self.snapshot = None
        self.ticks = os.sysconf("SC_CLK_TCK")

    def run(self):
        totals = {}
        peak = 0
        peak_exe = None
        polls = 0
        limited = False
        started = time.monotonic_ns()
        sampler_cpu = 0
        try:
            while not self.done.is_set():
                cpu_start = time.thread_time_ns()
                todo, seen = [self.pid], set()
                budget = 512
                while todo and budget > 0:
                    pid = todo.pop()
                    if pid in seen:
                        continue
                    seen.add(pid)
                    budget -= 1
                    root = Path(f"/proc/{pid}")
                    try:
                        stat = (root / "stat").read_text().rsplit(")", 1)[1].split()
                        exe = os.readlink(root / "exe")
                        # Fields 14/15 and 22; suffix starts at field 3.
                        # exec changes exe but preserves cumulative CPU ticks;
                        # keep one total for that process lifetime.
                        identity = (pid, stat[19])
                        own_ticks = int(stat[11]) + int(stat[12])
                        if identity in totals or len(totals) < 4096:
                            totals[identity] = max(totals.get(identity, 0), own_ticks)
                        else:
                            limited = True
                        for line in (root / "status").read_text().splitlines():
                            if line.startswith("VmHWM:"):
                                hwm = int(line.split()[1])
                                if hwm > peak:
                                    peak, peak_exe = hwm, exe
                                break
                    except (OSError, ValueError, IndexError):
                        pass
                    try:
                        # Bound both thread traversal and the process queue.
                        with os.scandir(root / "task") as tasks:
                            for task in tasks:
                                if budget <= 0:
                                    limited = True
                                    break
                                budget -= 1
                                try:
                                    children = (Path(task.path) / "children").read_text().split()
                                    room = max(0, 512 - len(todo))
                                    limited |= len(children) > room
                                    todo.extend(int(c) for c in children[:room])
                                except (OSError, ValueError):
                                    pass
                    except OSError:
                        pass
                limited |= bool(todo)
                polls += 1
                sampler_cpu += time.thread_time_ns() - cpu_start
                # Publish a fresh immutable-by-convention snapshot atomically.
                self.snapshot = dict(cpu_sampled_ms=sum(totals.values()) * 1000 / self.ticks if totals else None,
                                     peak_process_rss_kib=peak or None,
                                     peak_process_exe=peak_exe, polls=polls,
                                     identities=len(totals), limited=limited,
                                     sampler_cpu_ms=sampler_cpu / 1e6,
                                     sampled_through_ms=(time.monotonic_ns() - started) / 1e6)
                self.done.wait(0.1)
        except BaseException:
            self.snapshot = {"error": True}
        finally:
            _slots.release()

    def stop(self):
        self.done.set()
        # Never join a diagnostic thread on the certification path.
        return self.snapshot


@safe
def start_sampler(pid):
    if not _slots.acquire(blocking=False):
        return None
    try:
        sampler = Sampler(pid)
        threading.Thread(target=sampler.run, name="checker-resource-sampler", daemon=True).start()
        return sampler
    except BaseException:
        _slots.release()
        raise

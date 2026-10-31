"""Share a publication-lock lease across already prepared cleanup fences.

The callers still execute their own guards and mutations. Only one caller is
admitted at a time, and a lease is never released while its body is running.
Preparation permits are surrendered while a caller waits for its final fence.
By default each acquisition serves its ready batch, then hands the lock back.
Later arrivals cannot extend that batch indefinitely. There is no time cutoff
or limit on the total work drained; an explicit quantum is optional.
"""
from collections import deque
from contextlib import contextmanager
from dataclasses import dataclass, field
import threading
import time


@dataclass
class Request:
    ready: threading.Event = field(default_factory=threading.Event)
    done: threading.Event = field(default_factory=threading.Event)
    granted: bool = False
    error: BaseException | None = None


class Broker:
    def __init__(self, lock, stop, *, preparers=4, quantum=None):
        if preparers < 1 or (quantum is not None and not 0 < quantum < float('inf')):
            raise ValueError('cleanup preparation concurrency and lease quantum must be positive')
        self.lock, self.stop, self.quantum = lock, stop, quantum
        self._condition = threading.Condition()
        self._pending = deque()
        self._closed = False
        self._error = None
        self._preparation = threading.Semaphore(preparers)
        self._local = threading.local()
        self.leases = 0
        self.sections = 0
        self._wait_seconds = self._hold_seconds = 0.0
        self._waiting_since = self._holding_since = None
        self._thread = threading.Thread(target=self._run, name='cleanup-lock-broker', daemon=True)
        self._thread.start()

    @property
    def timing(self):
        """Include the current wait/hold so a blocked drain remains observable."""
        with self._condition:
            now = time.monotonic()
            return {
                'coordinator_wait_seconds': self._wait_seconds + (
                    now - self._waiting_since if self._waiting_since is not None else 0),
                'coordinator_hold_seconds': self._hold_seconds + (
                    now - self._holding_since if self._holding_since is not None else 0),
            }

    @contextmanager
    def _timed_lock(self):
        with self._condition:
            self._waiting_since = time.monotonic()
        try:
            with self.lock() as acquired:
                with self._condition:
                    now = time.monotonic()
                    self._wait_seconds += now - self._waiting_since
                    self._waiting_since = None
                    if acquired:
                        self._holding_since = now
                yield acquired
        finally:
            with self._condition:
                now = time.monotonic()
                if self._waiting_since is not None:
                    self._wait_seconds += now - self._waiting_since
                    self._waiting_since = None
                if self._holding_since is not None:
                    self._hold_seconds += now - self._holding_since
                    self._holding_since = None

    @contextmanager
    def preparing(self):
        self._preparation.acquire()
        self._local.preparing = True
        try:
            yield
        finally:
            self._local.preparing = False
            self._preparation.release()

    @contextmanager
    def critical(self):
        if getattr(self._local, 'critical', False):
            raise RuntimeError('nested cleanup critical section')
        preparation = getattr(self._local, 'preparing', False)
        if preparation:
            self._local.preparing = False
            self._preparation.release()
        request = Request()
        try:
            with self._condition:
                if self._error is not None:
                    raise self._error
                if self._closed or self.stop.is_set():
                    request.ready.set()
                else:
                    self._pending.append(request)
                    self._condition.notify_all()
            request.ready.wait()
            if request.error is not None:
                raise request.error
            self._local.critical = request.granted
            try:
                yield request.granted
            finally:
                self._local.critical = False
                # The broker may release the lease only after this signal.
                request.done.set()
        finally:
            # Reacquire CPU/I/O capacity only AFTER returning the lease grant;
            # otherwise preparers waiting for that lease can deadlock us.
            if preparation:
                self._preparation.acquire()
                self._local.preparing = True

    def close(self):
        with self._condition:
            self._closed = True
            self._reject_pending()
            self._condition.notify_all()

    def join(self, timeout=None):
        self._thread.join(timeout)
        return not self._thread.is_alive()

    def _reject_pending(self, error=None):
        while self._pending:
            request = self._pending.popleft()
            request.error = error
            request.ready.set()

    def _run(self):
        try:
            while True:
                with self._condition:
                    while not self._pending and not self._closed and not self.stop.is_set():
                        self._condition.wait()
                    if self._closed or self.stop.is_set():
                        self._reject_pending()
                        return
                with self._timed_lock() as acquired:
                    if not acquired:
                        self.close()
                        return
                    self.leases += 1
                    with self._condition:
                        batch_remaining = len(self._pending)
                    deadline = time.monotonic() + self.quantum if self.quantum is not None else None
                    while True:
                        with self._condition:
                            if self._closed or self.stop.is_set():
                                self._reject_pending()
                                break
                            if (not self._pending or (deadline is None and not batch_remaining)
                                    or (deadline is not None and time.monotonic() >= deadline)):
                                break
                            request = self._pending.popleft()
                            request.granted = True
                            request.ready.set()
                        # Cancellation cannot revoke an active body. Its guards
                        # observe stop themselves; always await its finalizer.
                        request.done.wait()
                        self.sections += 1
                        batch_remaining -= 1
                # Give already-blocked protocol waiters a scheduling turn before
                # queuing another lease; flock does not promise FIFO fairness.
                self.stop.wait(.01)
        except BaseException as error:
            with self._condition:
                self._error = error
                self._closed = True
                self._reject_pending(error)
                self._condition.notify_all()

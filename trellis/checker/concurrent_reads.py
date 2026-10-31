"""Coalesce simultaneous exact reads; retain no result between read passes."""
from __future__ import annotations

import threading
import time


class ConcurrentReadBatch:
    def __init__(self, read, *, window_seconds=.002):
        self.read = read
        self.window_seconds = window_seconds
        self.condition = threading.Condition()
        self.pending = []
        self.running = False

    def __call__(self, node):
        ticket = dict(node=node, done=False)
        with self.condition:
            self.pending.append(ticket)
            self.condition.notify_all()
            while not ticket['done']:
                if self.running:
                    self.condition.wait()
                    continue
                self.running = True
                deadline = time.monotonic() + self.window_seconds
                while (remaining := deadline - time.monotonic()) > 0:
                    self.condition.wait(remaining)
                batch, self.pending = self.pending, []
                # Only a running reader owns this batch. Later arrivals wait
                # for a new pass; they never receive these earlier results.
                self.condition.release()
                try:
                    values = self.read(list(dict.fromkeys(t['node'] for t in batch)))
                    error = None
                except BaseException as exc:
                    values, error = {}, exc
                finally:
                    self.condition.acquire()
                for request in batch:
                    request.update(done=True, value=values.get(request['node']), error=error)
                self.running = False
                self.condition.notify_all()
            if ticket['error'] is not None:
                raise ticket['error']
            return ticket['value']

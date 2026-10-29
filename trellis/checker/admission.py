"""Host-owned admission shared by authoritative and advisory Lean work.

Launch-configured isolated workloads may run, with one background job at a time.
Authoritative work has priority. Warm sessions keep their reservations until
reaped. Optional delegated compiler groups enforce separate execution ceilings.
"""
from contextlib import contextmanager
from contextvars import ContextVar
from pathlib import Path
import threading
import time

_cooperative_scheduler = ContextVar('cooperative_compute_scheduler', default=None)


class Admission:
    """Bounded compiler leases, including idle warm processes.

    Available mode admits on the host floor and recovers the newest execution
    on pressure. Legacy modes retain reservation/PSS accounting. Admission is
    a polling policy; delegated per-job groups enforce execution hard limits.
    """
    def __init__(self, max_active=1, *, available=None, floor_bytes=0, reserve_bytes=0,
                 readonly_slots=1, readonly_reserve_bytes=None, optimistic=False,
                 groups=None, retry_root=None, generation='0', available_memory=False,
                 cpu_budget=None, host_cpu_pool=None, adaptive=False,
                 recovery_seconds=5.0, recovery_margin_bytes=2 * 2**30, clock=time.monotonic):
        if type(max_active) is not int or max_active < 1:
            raise ValueError("managed compiler parallelism must be a positive integer")
        self.max_active = max_active
        self.cpu_accounting = cpu_budget is not None
        self.cpu_budget = max_active if cpu_budget is None else cpu_budget
        if type(self.cpu_budget) is not int or self.cpu_budget < 1:
            raise ValueError('checker CPU budget must be a positive integer')
        self.host_cpu_pool = host_cpu_pool
        self.adaptive = adaptive
        self.concurrency_limit = max_active
        self.recovery_seconds = recovery_seconds
        self.recovery_margin_bytes = recovery_margin_bytes
        self.clock = clock
        self.healthy_since = None
        self.pressure_preemptions = 0
        self.concurrency_changes = []
        self.available = available
        self.floor_bytes = floor_bytes
        self.reserve_bytes = reserve_bytes
        if type(readonly_slots) is not int or readonly_slots < 1:
            raise ValueError("read-only compiler parallelism must be a positive integer")
        self.readonly_slots = min(readonly_slots, max_active)
        self.readonly_reserve_bytes = reserve_bytes if readonly_reserve_bytes is None else readonly_reserve_bytes
        self.memory_samples = {}
        self.recovery_headroom = {}  # Budget-only, bounded, never a shared retry allowance.
        self.incomplete_peaks = {}
        self.pressure_victim = None
        self.condition = threading.Condition(threading.RLock())
        self.active = {}
        self.residents = {}
        self.waiters = []
        self.sequence = 0
        self.cleanup_failed = False
        self.optimistic, self.groups = optimistic, groups
        self.available_memory = available_memory
        self.admission_sequence = 0
        self.retry_root, self.generation = retry_root, generation
        self.maximum_job_bytes = 48 * 2**30
        self.serial_maximum_bytes = 56 * 2**30
        self.serial_floor_bytes = 4 * 2**30
        self.serial_wait_seconds = 300
        import weakref
        self.retry_locks = weakref.WeakValueDictionary()
        from trellis.memory_guard import ProcessSnapshot
        self.process_snapshot = ProcessSnapshot(clock=clock)

    def memory_available(self):
        available = self.available() if self.available else None
        if available is not None and self.groups is not None and self.adaptive:
            available = min(available, self.groups.headroom())
        return available

    def _cpu_used(self):
        return sum(r.get('cpu_units', 1) for r in self.active.values())

    def _cpu_request(self, operation, readonly, recovery=None):
        if not self.cpu_accounting:
            return 1
        if recovery and (recovery.data.get('serial') or recovery.data.get('pressure_requeues')):
            return 1
        if readonly or operation in {'manifest', 'kernel_replay', 'helper:compiled_probe'}:
            return 1
        if operation and operation.endswith(':dependencies_reused'):
            return 1
        import os
        return min(self.cpu_budget, int(os.environ.get('TRELLIS_LEAN_PARALLELISM', '6')))

    def _change_concurrency(self, value, reason):
        value = max(1, min(self.max_active, value))
        if value != self.concurrency_limit:
            self.concurrency_changes.append(dict(at=self.clock(), before=self.concurrency_limit,
                                                 after=value, reason=reason))
            self.concurrency_changes = self.concurrency_changes[-32:]
            self.concurrency_limit = value

    def _sample_recovery(self, *, completed=False):
        if not self.adaptive:
            return
        available = self.memory_available()
        if (self.pressure_victim is not None or available is None
                or available < self.floor_bytes + self.recovery_margin_bytes):
            self.healthy_since = None
            return
        now = self.clock()
        if self.healthy_since is None:
            self.healthy_since = now
        if completed and now - self.healthy_since >= self.recovery_seconds:
            self._change_concurrency(self.concurrency_limit + 1, 'stable memory and completed execution')
            self.healthy_since = now

    def snapshot(self):
        with self.condition:
            return dict(compiler_slots=self.max_active, readonly_slots=self.readonly_slots,
                        cpu_budget=self.cpu_budget, cpu_units_in_use=self._cpu_used(),
                        executing=len(self.active), queued=len(self.waiters),
                        concurrency_limit=self.concurrency_limit,
                        pressure_preemptions=self.pressure_preemptions,
                        concurrency_changes=list(self.concurrency_changes))

    def retry_lock(self, key):
        with self.condition:
            lock = self.retry_locks.get(key)
            if lock is None:
                lock = threading.Lock()
                self.retry_locks[key] = lock
            return lock

    @property
    def authoritative_waiters(self):
        return sum(w[0] == 0 for w in self.waiters)

    @property
    def evict(self):
        return next((r["evict"] for r in self.residents.values()), None)

    @staticmethod
    def _resident_bytes(record):
        from pathlib import Path
        from trellis.memory_guard import descendant_rss
        now = time.monotonic()
        prior = record.get("resident_sample")
        if prior is not None and now - prior[0] < .25:
            return prior[1]
        total = 0
        seen = set()
        for root in record["processes"]:
            _rss, descendants = descendant_rss(root)
            for pid, _rss, _name in descendants:
                if pid in seen:
                    continue
                seen.add(pid)
                try:
                    for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
                        # MemAvailable already includes reclaimable file pages.
                        # Crediting total PSS here would count those twice.
                        if line.startswith(("Pss_Anon:", "Pss_Shmem:")):
                            total += int(line.split()[1]) * 1024
                except (OSError, ValueError):
                    pass  # Missing accounting earns no capacity credit.
        record["resident_sample"] = (now, total)
        return total

    def _evict_idle(self, owner=None):
        for key in list(self.residents):
            if owner is not None and key != owner:
                continue
            record = self.residents[key]
            if record.get('cgroup') and getattr(record['cgroup'], 'cpu_control', False):
                # Frozen warm trees cannot answer a graceful shutdown RPC.
                # Kill and drain this owned group before the adapter reaps it.
                try:
                    record['cgroup'].close()
                except BaseException:
                    self.cleanup_failed = True
                    raise
                record.pop('cgroup')
            try:
                record["evict"]()  # Reap before returning the reservation.
                if record['processes']:
                    raise RuntimeError('idle compiler cleanup did not drain tracked processes')
            except BaseException:
                self.cleanup_failed = True
                raise
            self.residents.pop(key, None)

    def evict_idle(self, owner):
        with self.condition:
            self._evict_idle(owner)
            self.condition.notify_all()

    def _evict_one(self):
        owner = min(self.residents, key=lambda key: (-self.residents[key]["priority"],
                                                     self.residents[key].get("last_used", 0)))
        self._evict_idle(owner)

    def _can_admit(self, owner, priority, estimate, *, exclusive=False, readonly=False, serial=False):
        if self.cleanup_failed:
            raise OSError("compiler cleanup incomplete; restart the checker to reap recorded processes before accepting more work")
        if self.pressure_victim is not None:
            return False, "waiting for memory-pressure cleanup"
        if (exclusive and self.active) or any(r.get("exclusive") for r in self.active.values()):
            return False, "waiting for dependency cache installation"
        if len(self.active) >= min(self.max_active, self.concurrency_limit):
            return False, "waiting for a compiler slot"
        if self._cpu_used() >= self.cpu_budget:
            return False, 'waiting for compute CPU capacity'
        own = [r for r in self.active.values() if r["owner"] == owner]
        if own and (not readonly or any(not r.get("readonly") for r in own)
                    or len(own) >= self.readonly_slots):
            return False, "waiting for this workspace's compiler"
        if priority == 2 and any(r["priority"] == 2 for r in self.active.values()):
            return False, "waiting for the background compiler slot"
        if self.available is None:
            return True, None
        available = self.memory_available()
        if available is None:
            raise OSError("cannot read host memory availability")
        if self.available_memory:
            floor = self.serial_floor_bytes if serial else self.floor_bytes
            if available < floor:
                return False, f"waiting for memory floor ({available / 2**30:.1f} GiB available; {floor / 2**30:.1f} GiB floor)"
            return True, None
        # Active work keeps its not-yet-used reservation. Idle processes may
        # be evicted and are never counted as free compiler slots.
        reserved_growth = sum(max(0, r["estimate"] - self._resident_bytes(r)) for r in self.active.values())
        own_resident = self.residents.get(owner)
        resident_credit = self._resident_bytes(own_resident) if own_resident else 0
        required = self.floor_bytes + reserved_growth + max(0, estimate - resident_credit)
        if available < required:
            return False, f"waiting for memory headroom ({available / 2**30:.1f} GiB available; {required / 2**30:.1f} GiB needed)"
        return True, None

    @contextmanager
    def acquire(self, owner, *, priority=None, estimate_bytes=None, cancelled=None, waiting=None,
                exclusive=False, readonly=False, operation=None, limit_bytes=0, controlled=False,
                recovery=None, cpu_units=None):
        start = time.monotonic()
        if self.available_memory and not limit_bytes:
            limit_bytes = self.maximum_job_bytes
        thread = threading.get_ident()
        priority = (0 if owner == "authoritative" else 1) if priority is None else priority
        def current_estimate():
            if callable(estimate_bytes):
                return estimate_bytes()
            return self.estimate(operation, readonly=readonly, owner=owner) if estimate_bytes is None else estimate_bytes
        estimate = current_estimate()
        requested_cpu = self._cpu_request(operation, readonly, recovery) if cpu_units is None else cpu_units
        if type(requested_cpu) is not int or requested_cpu < 1:
            raise ValueError('execution CPU request must be a positive integer')
        with self.condition:
            if self.cleanup_failed:
                raise OSError('compiler cleanup incomplete; restart the checker before accepting more work')
            if thread in self.active:
                if exclusive and not self.active[thread].get("exclusive"):
                    raise RuntimeError("dependency cache installation cannot upgrade an active compiler lease")
                if self.active[thread].get("readonly") and not readonly:
                    raise RuntimeError("read-only compiler lease cannot start a workspace writer")
                self.active[thread]["depth"] += 1
            else:
                ticket = (priority, self.sequence)
                self.sequence += 1
                self.waiters.append(ticket)
                if not hasattr(self, "_requests"):
                    self._requests = {}
                self._requests[ticket] = dict(owner=owner, priority=priority, estimate=estimate,
                    exclusive=exclusive, readonly=readonly, operation=operation, since=start, bypasses=0,
                    serial=bool(recovery and recovery.data['serial']),
                    queued_at=recovery.data.get('queued_at', time.time()) if recovery else time.time())
                try:
                    while True:
                        if cancelled:
                            cancelled()
                        if recovery:
                            recovery.check_wait()
                        self._sample_recovery()
                        estimate = current_estimate()
                        self._requests[ticket]["estimate"] = estimate
                        head = self._next_eligible()
                        first = head == ticket
                        if first and recovery and recovery.data['serial'] and not self.active:
                            # Only this scheduler's own idle compiler sessions.
                            self._evict_idle()
                        # Resident sessions belonging to other lanes can be
                        # discarded without stopping running compiler work.
                        admitted, reason = self._can_admit(owner, priority, estimate, exclusive=exclusive, readonly=readonly,
                            serial=bool(recovery and recovery.data['serial']))
                        while first and not admitted and self.residents and reason.startswith("waiting for memory"):
                            self._evict_one()
                            admitted, reason = self._can_admit(owner, priority, estimate, exclusive=exclusive, readonly=readonly,
                                serial=bool(recovery and recovery.data['serial']))
                        if first and admitted and recovery and recovery.data['serial']:
                            admitted, reason = recovery.admission()
                            limit_bytes = recovery.data['limit_bytes']
                        cpu_handles = []
                        granted_cpu = min(requested_cpu, self.cpu_budget - self._cpu_used())
                        if first and admitted and self.host_cpu_pool is not None:
                            cpu_handles = self.host_cpu_pool.acquire(granted_cpu, exclusive=exclusive)
                            granted_cpu = len(cpu_handles)
                            if not granted_cpu:
                                admitted, reason = False, 'waiting for shared host CPU capacity'
                        if first and admitted:
                            try:
                                if exclusive:
                                    self._evict_idle()
                                if (self.residents and owner not in self.residents
                                        and len(self.active) + len(self.residents) >= self.max_active):
                                    self._evict_one()
                            except BaseException:
                                if self.host_cpu_pool is not None:
                                    self.host_cpu_pool.release(cpu_handles)
                                raise
                            record = self.residents.pop(owner, None) or dict(processes=set(), evict=None)
                            for older in self.waiters:
                                if older < ticket:
                                    self._requests[older]["bypasses"] += 1
                            record.update(owner=owner, priority=priority, estimate=estimate, depth=1,
                                          exclusive=exclusive, readonly=readonly, operation=operation,
                                          peak_bytes=0, limit_bytes=limit_bytes, controlled=controlled, completed=False,
                                          cpu_units=granted_cpu, cpu_handles=cpu_handles,
                                          pressure_requeues=recovery.data.get('pressure_requeues', 0) if recovery else 0)
                            record['serial'] = bool(recovery and recovery.data['serial'])
                            record['floor_bytes'] = self.serial_floor_bytes if record['serial'] else self.floor_bytes
                            record.pop('spawn_checked', None)
                            for running in self.active.values():
                                if 'recovery_attempt' in running:
                                    running['recovery_attempt']['other_groups'] = max(
                                        running['recovery_attempt']['other_groups'], len(self.active))
                            record['admission_sequence'] = self.admission_sequence
                            record.pop('pressure_stop', None)
                            self.admission_sequence += 1
                            self.active[thread] = record
                            break
                        if waiting:
                            if first:
                                waiting(reason)
                            else:
                                blocker = self._requests[head]
                                description = blocker['operation'] or 'cold_or_mutating'
                                if not self.available_memory:
                                    description += f"; estimated reservation {blocker['estimate'] / 2**30:.1f} GiB"
                                waiting(f"waiting behind compiler work ({description})")
                        self.condition.wait(.5)
                finally:
                    self.waiters.remove(ticket)
                    self._requests.pop(ticket, None)
                    self.condition.notify_all()
        try:
            record = self.active[thread]
            if record.get('cgroup') and record['depth'] == 1:
                record['cgroup'].limit(record.get('limit_bytes', 0))
                if self.cpu_accounting:
                    record['cgroup'].cpu_limit(record['cpu_units'])
                    record['cgroup'].freeze(False)
            yield (time.monotonic() - start) * 1000
        finally:
            with self.condition:
                record = self.active[thread]
                record["depth"] -= 1
                if record["depth"] == 0:
                    if (record.get('serial') or record.get('pressure_stop')) and record.get('evict'):
                        try:
                            record['evict']()
                        except BaseException:
                            self.cleanup_failed = True
                            raise
                        record['evict'] = None
                    # Polls belong to this execution until its outcome is
                    # known. Only an unfinished execution leaves a durable
                    # lower bound outside the bounded completed history.
                    operation = record.get("operation")
                    if operation and record["peak_bytes"] and not record.get('controlled'):
                        self.incomplete_peaks[operation] = max(
                            record["peak_bytes"], self.incomplete_peaks.get(operation, 0))
                        self._trim_history()
                    if not record['evict'] and record.get('cgroup') is not None:
                        try:
                            record['cgroup'].close()
                        except BaseException:
                            self.cleanup_failed = True
                            raise
                        record.pop('cgroup')
                    if record['processes'] and not record['evict']:
                        self.cleanup_failed = True
                        # Keep CPU locks until restart reconciliation drains
                        # the live tree; a failed cleanup is not free capacity.
                        raise RuntimeError('compiler processes did not drain; compute lease retained')
                    if record['evict'] and record.get('cgroup') and self.cpu_accounting:
                        try:
                            record['cgroup'].freeze(True)
                        except BaseException:
                            self.cleanup_failed = True
                            raise
                    if self.host_cpu_pool is not None:
                        self.host_cpu_pool.release(record.pop('cpu_handles', []))
                    del self.active[thread]
                    if self.pressure_victim == thread:
                        self.pressure_victim = None
                    if record["evict"]:
                        record["last_used"] = time.monotonic()
                        self.residents[owner] = record
                    elif record["processes"]:
                        # A failed kill/wait must not turn an unaccounted live
                        # compiler into capacity for another job. Restart
                        # reconciliation owns the journalled process identities.
                        self.cleanup_failed = True
                    self.condition.notify_all()

    def _next_eligible(self):
        """Skip busy workspaces; reserve headroom for an aged large request."""
        # A pending recovery drains this checker even when later authoritative
        # work has higher priority. Multiple recoveries retain FIFO ordering.
        serial = [ticket for ticket in self.waiters if self._requests[ticket].get('serial')]
        if serial:
            return min(serial, key=lambda ticket: ticket[1])
        for ticket in sorted(self.waiters, key=lambda ticket:
                             (ticket[0], self._requests[ticket].get('queued_at', ticket[1]), ticket[1])):
            request = self._requests[ticket]
            ready, reason = self._can_admit(**{k: request[k] for k in
                ("owner", "priority", "estimate", "exclusive", "readonly")})
            if ready:
                return ticket
            # A package installer must drain readers. A large memory request
            # may be bypassed briefly, but cannot starve behind small probes.
            if request["exclusive"] or (reason.startswith("waiting for memory") and
                    (self.residents or request["bypasses"] >= 8 or
                     time.monotonic() - request["since"] >= 30)):
                return ticket
        return min(self.waiters)

    def estimate(self, operation=None, *, readonly=False, owner=None, conservative=False):
        from trellis.memory_guard import GIB
        seed = self.readonly_reserve_bytes if readonly else self.reserve_bytes
        if operation is None:
            return seed
        # A workspace name is a scheduling boundary, not a memory workload.
        # Share host measurements across private copies of the same operation.
        # Keep the maximum recent peak and a separate interrupted lower bound.
        key = operation
        from . import memory_retry
        typical = self.optimistic and not conservative and memory_retry.eligible(operation)
        if typical:
            seed = memory_retry.seed(operation)
        if operation == "manifest":
            seed = min(seed, 8 * GIB)
        with self.condition:
            samples = self.memory_samples.get(key, [])
            if typical:
                # Tail demand remains in execution ceilings and exact-workload
                # retry floors. It is not every future job's initial budget.
                if len(samples) < 8:
                    return seed
                ordered = sorted(samples)
                peak = ordered[(3 * len(ordered) + 3) // 4 - 1]
                return max((12 if operation == 'kernel_replay' else 6) * GIB,
                           peak * 1.25 + 2 * GIB)
            active = [r["peak_bytes"] for r in self.active.values() if r.get("operation") == key]
            peak = max([*samples, *active, self.incomplete_peaks.get(key, 0)])
            learned = max(6 * GIB, peak * 1.25 + 2 * GIB) if peak else seed
            # Unsampled/short/interrupted work never lowers the initial budget.
            return max(seed, learned) if len(samples) < 3 else learned

    def observe_peak(self, peak, *, completed=False):
        """Learn only from host-sampled compiler trees, never agent reports."""
        if not peak and not completed:
            return
        from trellis.memory_guard import GIB
        with self.condition:
            record = self.active.get(threading.get_ident())
            if record is None:
                return
            record["peak_bytes"] = max(record["peak_bytes"], peak)
            if 'recovery_attempt' in record:
                attempt = record['recovery_attempt']
                attempt['sampled_peak_bytes'] = max(attempt['sampled_peak_bytes'], peak)
            if record["peak_bytes"]:
                record["estimate"] = max(record["estimate"], record["peak_bytes"] * 1.25 + 2 * GIB)
            operation = record.get("operation")
            if completed:
                if operation and record["peak_bytes"]:
                    self.memory_samples.setdefault(operation, []).append(record["peak_bytes"])
                    self.memory_samples[operation] = self.memory_samples[operation][-32:]
                    self._trim_history()
                # A later compiler in the same lease starts a new sample.
                # Keep the lease's growth reservation until it is released.
                record["peak_bytes"] = 0

    def _trim_history(self):
        for table in (self.memory_samples, self.incomplete_peaks):
            while len(table) > 128:
                table.pop(next(iter(table)))

    def pressure_reason(self, reason, *, limit=False):
        """One host-pressure victim at a time, reaped before more admission."""
        with self.condition:
            thread = threading.get_ident()
            if thread not in self.active:
                return reason
            if self.available_memory:
                # A local hard limit belongs to its own job, regardless of
                # which other execution is draining for host pressure.
                if limit:
                    return reason
                floor = self.execution_floor()
                if self.pressure_victim is None:
                    available = self.memory_available()
                    if available is not None and available < floor:
                        self._evict_idle()
                        available = self.memory_available()
                        if available is not None and available < floor:
                            # A just-admitted lease also owns a cancellation
                            # handshake: process_command observes the flag
                            # before its first spawn, or lease exit ACKs without
                            # spawning. Ticket priority is not admission order.
                            running = {key: value for key, value in self.active.items()
                                       if not value.get('completed')}
                            if running:
                                victim = max(running, key=lambda key:
                                    (-running[key].get('pressure_requeues', 0), running[key]['admission_sequence']))
                                self.pressure_victim = victim
                                self.pressure_preemptions += 1
                                if self.adaptive:
                                    self._change_concurrency(min(self.concurrency_limit, len(running) - 1),
                                                             'memory pressure')
                                    self.healthy_since = None
                                running[victim]['pressure_stop'] = (
                                    f'host memory pressure: {available / 2**30:.1f} GiB available; '
                                    f'{floor / 2**30:.1f} GiB floor; newest admitted execution requeued')
                # Sticky through recovery of MemAvailable, small RSS, and
                # process completion. The selected owner's ordinary cleanup
                # must drain its namespace before the lease releases capacity.
                self._sample_recovery()
                return self.active[thread].get('pressure_stop')
            if self.pressure_victim is None:
                self._evict_idle()
                available = self.available() if self.available else None
                if not limit and available is not None and available >= self.floor_bytes:
                    return None
                # Select the reporting thread: its guard has confirmed a live
                # tree large enough to stop. Selecting an unpolled small job
                # could strand every caller waiting for an ACK it never sends.
                self.pressure_victim = thread
            if self.pressure_victim == thread:
                callback = _authoritative_resource_failure.get()
                if callback is not None and not self.active[thread].get('controlled'):
                    callback(reason)
                return reason
            return None

    def complete_execution(self):
        """Linearize success against newest-victim selection before delivery."""
        if not self.available_memory:
            return
        with self.condition:
            record = self.active[threading.get_ident()]
            if record.get('pressure_stop'):
                from .memory_retry import MemoryStopped
                raise MemoryStopped('host_pressure', record['peak_bytes'], record['pressure_stop'])
            was_completed = record.get('completed')
            record['completed'] = True
            if not was_completed and not record.get('serial'):
                self._sample_recovery(completed=True)

    def process_command(self, command):
        with self.condition:
            record = self.active.get(threading.get_ident())
            if record is not None and record.get('serial') and not record.get('spawn_checked'):
                from .memory_retry import MemoryStopped
                available = self.available() if self.available else None
                required = record['floor_bytes']
                if (available is None or available < required
                        or self.groups is not None and self.groups.headroom() < required):
                    raise MemoryStopped('host_pressure', 0, 'serial headroom changed before compiler launch')
                record['spawn_checked'] = True
            if record is not None and self.available_memory:
                reason = self.pressure_reason(None)
                if reason:
                    from .memory_retry import MemoryStopped
                    if record.get('controlled'):
                        raise MemoryStopped('host_pressure', record['peak_bytes'], reason)
                    raise OSError('checker resource exhaustion: ' + reason)
            if record is None or self.groups is None:
                return command
            if record.get('cgroup') is None:
                record['cgroup'] = self.groups.create(record.get('limit_bytes', 0))
                if self.cpu_accounting:
                    record['cgroup'].cpu_limit(record['cpu_units'])
            return record['cgroup'].command(command)

    def check_carrier(self, process):
        record = self.active.get(threading.get_ident(), {})
        if record.get('cgroup') is not None and process.poll() == 125:
            # Reserved carrier startup status is infrastructure, never a Lean
            # verdict or evidence of a memory stop. A compiler's own exit125
            # is also conservatively infrastructure; no controlled retry.
            raise OSError('compiler group launch failed; inspect checker compiler stderr')

    def memory_stop(self, guard, pid):
        """Return a typed host observation; never infer OOM from stderr/rc."""
        from .memory_retry import MemoryStopped
        from trellis.memory_guard import MemoryGuard
        record = self.active[threading.get_ident()]
        if self.adaptive and isinstance(guard, MemoryGuard):
            guard.process_snapshot = self.process_snapshot
        reason = guard.check(pid)
        self.observe_peak(guard.peak_bytes)
        cause = 'execution_limit' if guard.limit_bytes and guard.peak_bytes > guard.limit_bytes else 'host_pressure'
        group = record.get('cgroup')
        killed = False
        if group is not None:
            sample = group.sample()
            record['cgroup_measurement'] = sample
            if 'recovery_attempt' in record:
                record['recovery_attempt']['cgroup_events'] = sample
                record['recovery_attempt']['cgroup_peak_bytes'] = sample['peak_bytes']
            killed = sample['oom_kills'] > 0
            if killed:
                # oom_kill counts any OOM, including an ancestor's limit or
                # global pressure. Only local max+oom events bind our cap.
                if record.get('limit_bytes') and sample['max_events'] > 0 and sample['oom_events'] > 0:
                    cause, reason = 'cgroup_oom', 'compiler cgroup memory limit exhausted'
                else:
                    cause, reason = 'host_pressure', 'compiler killed by host or ancestor memory pressure'
        selected = self.pressure_reason(reason, limit=cause != 'host_pressure') if reason or self.available_memory else None
        if selected and not reason:
            reason = selected
        # A kernel-confirmed OOM has already killed this group, even if a
        # different pressure victim is currently draining. Never turn that
        # definite resource outcome into ordinary Lean diagnostics.
        if reason and (selected or cause != 'host_pressure' or killed):
            if (cause != 'host_pressure' or killed) and not selected and not record.get('controlled'):
                callback = _authoritative_resource_failure.get()
                if callback is not None:
                    callback(reason)
            return MemoryStopped(cause, guard.peak_bytes, reason,
                cgroup_peak_bytes=record.get('cgroup_measurement', {}).get('peak_bytes', 0))
        return None

    def execution_limit(self):
        record = self.active.get(threading.get_ident(), {})
        # cgroup charges and sampled tree RSS are different metrics. The
        # cgroup enforces its own ceiling; RSS remains the portable fallback.
        return 0 if self.groups else record.get('limit_bytes', 0)

    def execution_floor(self):
        return self.active.get(threading.get_ident(), {}).get('floor_bytes', self.floor_bytes)

    def lean_threads(self):
        import os
        if self.active.get(threading.get_ident(), {}).get('serial'):
            return 1
        configured = int(os.environ.get('TRELLIS_LEAN_PARALLELISM', '6').strip() or '6')
        return (self.active.get(threading.get_ident(), {}).get('cpu_units', configured)
                if self.cpu_accounting else configured)

    def retain(self, owner, evict):
        with self.condition:
            record = self.active[threading.get_ident()]
            assert record["owner"] == owner
            record["evict"] = evict

    def track_process(self, pid, *, remove=False):
        with self.condition:
            if remove:
                for record in [*self.active.values(), *self.residents.values()]:
                    record["processes"].discard(pid)
                    record.pop("resident_sample", None)
                self.condition.notify_all()
                return
            record = self.active.get(threading.get_ident())
            if record is None:
                return
            record["processes"].add(pid)
            record.pop("resident_sample", None)
            self.condition.notify_all()

    def drop_residency(self, owner):
        with self.condition:
            for record in [*self.active.values(), *self.residents.values()]:
                if record["owner"] == owner:
                    record["evict"] = None
            record = self.residents.pop(owner, None)
            if record and record.get('cgroup'):
                try:
                    record['cgroup'].close()
                except BaseException:
                    self.cleanup_failed = True
                    raise


def _configured_maximum(floor_bytes, name='TRELLIS_CHECK_MAX_JOB_GIB', default='48'):
    import math
    import os
    from pathlib import Path
    gib = 2**30
    requested = float(os.environ.get(name, default)) * gib
    total = next(int(line.split()[1]) * 1024 for line in Path('/proc/meminfo').read_text().splitlines()
                 if line.startswith('MemTotal:'))
    if not math.isfinite(requested) or requested <= 0 or floor_bytes < 0 or floor_bytes >= total:
        raise ValueError('checker maximum job memory and floor must fit physical memory')
    return int(min(requested, total - floor_bytes))


def configured(runtime=None):
    import os
    from pathlib import Path
    from .compiler_cgroup import configured as configured_groups
    from trellis.memory_guard import system_available_bytes, GIB
    mode = os.environ.get('TRELLIS_CHECK_MEMORY_POLICY', 'conservative')
    if mode not in {'conservative', 'optimistic', 'available'}:
        raise ValueError('unknown checker memory policy')
    generation = os.environ.get('TRELLIS_CHECK_MEMORY_POLICY_GENERATION', '0')
    if not generation or len(generation) > 128:
        raise ValueError('invalid checker memory policy generation')
    groups = configured_groups()
    if mode in {'optimistic', 'available'} and groups is None:
        raise ValueError(f'{mode} checker memory policy requires TRELLIS_CHECK_CGROUP=delegated')
    from .compute_budget import capacity, positive_setting, HostCpuPool
    topology = capacity()
    cpu_budget = positive_setting('TRELLIS_CHECK_CPU_BUDGET', topology['default_cpu_budget'])
    slots = positive_setting('TRELLIS_CHECK_JOBS_PARALLELISM', cpu_budget)
    pool_path = os.environ.get('TRELLIS_CHECK_HOST_CPU_POOL')
    host_budget = positive_setting('TRELLIS_CHECK_HOST_CPU_BUDGET', cpu_budget)
    host_pool = HostCpuPool(pool_path, host_budget) if pool_path else None
    recovery_seconds = float(os.environ.get('TRELLIS_CHECK_MEMORY_RECOVERY_SECONDS', '5'))
    recovery_margin = float(os.environ.get('TRELLIS_CHECK_MEMORY_RECOVERY_MARGIN_GIB', '2')) * GIB
    import math
    if not math.isfinite(recovery_seconds) or recovery_seconds <= 0 or not math.isfinite(recovery_margin) or recovery_margin < 0:
        raise ValueError('memory recovery interval and margin must be finite and nonnegative')
    scheduler = Admission(slots,
                     available=system_available_bytes,
                     floor_bytes=float(os.environ.get("TRELLIS_CHECK_JOBS_FLOOR_GIB", "8" if mode == 'available' else "6")) * GIB,
                     reserve_bytes=float(os.environ.get("TRELLIS_CHECK_JOBS_RESERVE_GIB", "24")) * GIB,
                     readonly_slots=positive_setting('TRELLIS_CHECK_READONLY_PARALLELISM', slots),
                     readonly_reserve_bytes=float(os.environ.get("TRELLIS_CHECK_READONLY_RESERVE_GIB", "14")) * GIB,
                     optimistic=mode in {'optimistic', 'available'}, available_memory=mode == 'available', groups=groups,
                     retry_root=Path(runtime) / 'checker-state/memory-retries' if runtime else None,
                     generation=generation, cpu_budget=min(cpu_budget, host_budget) if host_pool else cpu_budget,
                     host_cpu_pool=host_pool, adaptive=mode == 'available',
                     recovery_seconds=recovery_seconds, recovery_margin_bytes=recovery_margin)
    scheduler.maximum_job_bytes = _configured_maximum(scheduler.floor_bytes)
    if scheduler.available_memory:
        import math
        scheduler.serial_floor_bytes = float(os.environ.get('TRELLIS_CHECK_SERIAL_FLOOR_GIB', '4')) * GIB
        scheduler.serial_wait_seconds = float(os.environ.get('TRELLIS_CHECK_SERIAL_WAIT_SECONDS', '300'))
        if (not math.isfinite(scheduler.serial_floor_bytes) or scheduler.serial_floor_bytes <= 0
                or not math.isfinite(scheduler.serial_wait_seconds) or scheduler.serial_wait_seconds <= 0):
            raise ValueError('serial memory floor and admission wait must be finite and positive')
        scheduler.serial_maximum_bytes = _configured_maximum(scheduler.serial_floor_bytes,
            'TRELLIS_CHECK_SERIAL_MAX_JOB_GIB', '56')
    if runtime:
        from trellis.json_io import save_json
        save_json(Path(runtime) / 'checker-state/memory-policy.json', dict(version=1,
            runtime_root=str(Path(runtime).resolve()), generation=generation, mode=mode,
            server_identity=dict(pid=os.getpid(), started_ns=time.time_ns()),
            compiler_slots=scheduler.max_active, floor_bytes=scheduler.floor_bytes,
            readonly_slots=scheduler.readonly_slots, cpu_budget=scheduler.cpu_budget,
            host_cpu_pool=pool_path, host_cpu_budget=host_budget if host_pool else None,
            topology=topology, adaptive_concurrency=scheduler.adaptive,
            cpu_control=getattr(scheduler.groups, 'cpu_control', False),
            recovery_seconds=recovery_seconds, recovery_margin_bytes=recovery_margin,
            reserve_bytes=scheduler.reserve_bytes, readonly_reserve_bytes=scheduler.readonly_reserve_bytes,
            admission_uses_reservations=not scheduler.available_memory,
            pressure_victim_policy=('fewest_requeues_then_newest' if scheduler.adaptive else
                'newest_admitted' if scheduler.available_memory else 'reporting_execution'),
            maximum_job_bytes=scheduler.maximum_job_bytes,
            serial_maximum_bytes=scheduler.serial_maximum_bytes,
            serial_floor_bytes=scheduler.serial_floor_bytes,
            serial_wait_seconds=scheduler.serial_wait_seconds,
            cgroup_enabled=scheduler.groups is not None), mode=0o600)
    return scheduler


# Installed only by the host checker server, never by a worker importing us.
ACTIVE = None
_replay_input = ContextVar('checker_replay_input', default=None)


@contextmanager
def replay_input(bundles):
    marker = _replay_input.set(bundles)
    try:
        yield
    finally:
        _replay_input.reset(marker)


def lean_threads():
    import os
    return str(ACTIVE.lean_threads()) if ACTIVE else (os.environ.get('TRELLIS_LEAN_PARALLELISM', '6').strip() or '6')
_reprepare_boundary = ContextVar('checker_pressure_reprepare', default=False)


class ReprepareMemory(Exception):
    """An ungranted cold RPC must rerun its complete host preparation."""
    def __init__(self, repo, detail):
        self.repo, self.detail = repo, detail
        super().__init__('host memory pressure; reprepare captured checker request')


@contextmanager
def reprepare_boundary():
    marker = _reprepare_boundary.set(bool(ACTIVE and ACTIVE.available_memory
        and threading.get_ident() not in ACTIVE.active))
    try:
        yield
    finally:
        _reprepare_boundary.reset(marker)
_authoritative_owner = ContextVar("authoritative_workspace", default="authoritative")
_authoritative_cancel = ContextVar("authoritative_cancellation", default=None)
_capture_cancel = ContextVar("authoritative_capture_cancellation", default=None)
_authoritative_resource_failure = ContextVar("authoritative_resource_failure", default=None)
_authoritative_capacity_failure = ContextVar("authoritative_capacity_failure", default=None)
_readonly_operation = ContextVar("authoritative_readonly_operation", default=None)
_memory_operation = ContextVar("authoritative_memory_operation", default=None)


@contextmanager
def compiler_operation(operation):
    """Classify host work for memory estimates without granting read concurrency."""
    if operation not in {"manifest", "kernel_replay"}:
        raise ValueError("unknown compiler memory operation")
    marker = _memory_operation.set(operation)
    try:
        yield
    finally:
        _memory_operation.reset(marker)


@contextmanager
def prepared_readonly(operation):
    """Host-only scope after replay/collector preparation under a workspace gate."""
    if operation not in {"semantic", "local_closure", "axioms"}:
        raise ValueError("unknown prepared compiler operation")
    marker = _readonly_operation.set(operation)
    try:
        yield
    finally:
        _readonly_operation.reset(marker)


def map_prepared(operation, fn, values):
    """Bounded read-only leaves; caller holds the prepared workspace gate.

    Each leaf retains the exact workspace and cancellation context. Global
    admission still owns every compiler; the pool is not another budget.
    """
    from concurrent.futures import ThreadPoolExecutor
    from contextvars import copy_context
    from trellis.checker import telemetry
    values = list(values)
    limit = min(len(values), ACTIVE.readonly_slots if ACTIVE else 1)
    def run(value):
        check_cancelled()
        with prepared_readonly(operation):
            return fn(value)
    if limit <= 1:
        return [run(value) for value in values]
    with ThreadPoolExecutor(limit, thread_name_prefix="checker-read-probe") as pool:
        futures = [pool.submit(copy_context().run, telemetry.parallel_call, run, value)
                   for value in values]
        results = []
        for future in futures:
            result, measurement = future.result()
            telemetry.merge_parallel(measurement)
            results.append(result)
        return results


@contextmanager
def authoritative_workspace(identity, *, cancelled=None, capture_cancelled=None,
                            resource_failure=None, capacity_failure=None):
    """Separate workspace leases while retaining one admission budget."""
    marker = _authoritative_owner.set("authoritative:" + str(identity))
    cancel_marker = _authoritative_cancel.set(cancelled)
    capture_marker = _capture_cancel.set(capture_cancelled)
    resource_marker = _authoritative_resource_failure.set(resource_failure)
    capacity_marker = _authoritative_capacity_failure.set(capacity_failure)
    try:
        yield
    finally:
        _authoritative_owner.reset(marker)
        _authoritative_cancel.reset(cancel_marker)
        _capture_cancel.reset(capture_marker)
        _authoritative_resource_failure.reset(resource_marker)
        _authoritative_capacity_failure.reset(capacity_marker)


@contextmanager
def capture_cancellation():
    """Use request-local bounded polling only inside a portable capture.

    The server supplies its already-acquired liveness object. Entry and normal
    return force authentication; compiler admission/spawn retain their existing
    force checks. Other work keeps its original cancellation callback.
    """
    callback = _capture_cancel.get()
    if callback is None:
        yield
        return
    callback.force()
    marker = _authoritative_cancel.set(callback)
    try:
        yield
        callback.force()
    finally:
        _authoritative_cancel.reset(marker)


def check_cancelled(*, force=False):
    callback = _authoritative_cancel.get()
    if callback is not None:
        (getattr(callback, 'force', callback) if force else callback)()


def cooperative_poll():
    """Cancel a host CPU stage between bounded chunks, without killing its host."""
    check_cancelled()
    scheduler = ACTIVE or _cooperative_scheduler.get()
    if scheduler is not None and threading.get_ident() in scheduler.active:
        reason = scheduler.pressure_reason(None) if scheduler.available_memory else None
        if reason:
            from .memory_retry import MemoryStopped
            raise MemoryStopped('host_pressure', 0, reason)


@contextmanager
def cooperative_stage(operation, *, cancelled=None):
    """A CPU-only leaf; nested work inherits its caller's existing grant."""
    scheduler = ACTIVE or _cooperative_scheduler.get()
    external = scheduler is None
    if external:
        from .compute_leaf import host_pool
        from .compute_budget import HostCpuPool
        from trellis.memory_guard import system_available_bytes
        import os
        pool = host_pool()
        if pool is not None:
            directory, units = pool
            scheduler = Admission(1, readonly_slots=1, cpu_budget=1, adaptive=True,
                available_memory=True, available=system_available_bytes,
                floor_bytes=float(os.environ.get('TRELLIS_CHECK_JOBS_FLOOR_GIB', '8')) * 2**30,
                host_cpu_pool=HostCpuPool(directory, units))
            scheduler.serial_wait_seconds = float(os.environ.get('TRELLIS_CHECK_SERIAL_WAIT_SECONDS', '300'))
    if scheduler is None or not scheduler.cpu_accounting:
        yield
        return
    nested = threading.get_ident() in scheduler.active
    deadline = time.monotonic() + scheduler.serial_wait_seconds
    def ready():
        (cancelled or check_cancelled)()
        if time.monotonic() >= deadline:
            from .memory_retry import ResourceOutcome
            raise ResourceOutcome(dict(kind='host_pressure_requeue', capacity_wait=True,
                                       exhausted_reason='helper_admission_deadline'))
    marker = _cooperative_scheduler.set(scheduler)
    try:
        with scheduler.acquire(_authoritative_owner.get(), readonly=True, operation=operation,
                               cpu_units=1, controlled=True, cancelled=ready):
            cooperative_poll()
            yield
            cooperative_poll()
            if not nested:
                scheduler.complete_execution()
    finally:
        _cooperative_scheduler.reset(marker)


def cooperative(fn):
    """Bound a pure host preparation stage without acquiring another child grant."""
    from functools import wraps
    @wraps(fn)
    def wrapped(*args, **kwargs):
        from .memory_retry import MemoryStopped, ResourceOutcome
        try:
            with cooperative_stage('host:' + fn.__module__ + '.' + fn.__name__):
                return fn(*args, **kwargs)
        except MemoryStopped as failure:
            if ACTIVE is not None:
                raise
            raise ResourceOutcome(dict(kind='host_pressure_requeue', capacity_wait=True,
                exhausted_reason='helper_requires_repreparation', reason=failure.reason)) from None
    return wrapped


@contextmanager
def authoritative_lease(*, exclusive=False, estimate_bytes=None, limit_bytes=0, controlled=False,
                        cancelled=None, recovery=None, cpu_units=None):
    check_cancelled()
    if ACTIVE is None:
        check_cancelled(force=True)
        yield 0.0
    else:
        readonly = _readonly_operation.get()
        operation = _memory_operation.get() or readonly
        if estimate_bytes is None and not controlled:
            estimate_bytes = lambda: ACTIVE.estimate(operation, readonly=readonly is not None, conservative=True)
        with ACTIVE.acquire(_authoritative_owner.get(), priority=0, cancelled=cancelled or check_cancelled,
                exclusive=exclusive, readonly=readonly is not None, operation=operation,
                estimate_bytes=estimate_bytes, limit_bytes=limit_bytes, controlled=controlled,
                recovery=recovery, cpu_units=cpu_units) as elapsed:
            check_cancelled(force=True)
            yield elapsed


def admitted(fn):
    from functools import wraps
    @wraps(fn)
    def wrapped(*args, **kwargs):
        from trellis.checker import telemetry
        from . import memory_retry
        # Only the host's fixed dependency-cache command can request package
        # writes. Drain ordinary readers while it installs shared artifacts;
        # all ordinary compiler calls retain their existing parallelism.
        exclusive = kwargs.get("bwrap_role") == "lake_dependency_cache"
        operation = _memory_operation.get() or _readonly_operation.get()
        if (ACTIVE and ACTIVE.available_memory and threading.get_ident() not in ACTIVE.active
                and (fn.__name__ == '_run_lake_command'
                     or (fn.__module__, fn.__name__) == ('trellis.checker.compiled_probe', '_compile'))):
            return _recover_available(fn, args, kwargs, exclusive=exclusive)
        if (ACTIVE and ACTIVE.optimistic and (memory_retry.eligible(operation)
                or (ACTIVE.available_memory and operation == 'manifest'))
                and fn.__name__ == '_run_lake_command' and kwargs.get('bwrap_role') == 'lake_compiler'
                and (_authoritative_capacity_failure.get() is not None or ACTIVE.available_memory)
                and threading.get_ident() not in ACTIVE.active):
            key = memory_retry.command_key(_authoritative_owner.get(), operation, args, kwargs)
            timeout = kwargs['timeout_secs']
            rpc_deadline = time.monotonic() + timeout + 3590
            state = None
            def cancelled():
                check_cancelled()
                if time.monotonic() >= rpc_deadline:
                    if state:
                        raise state.outcome('host_pressure_requeue', 'admission_deadline')
                    raise TimeoutError('compiler admission deadline expired')
            lock = ACTIVE.retry_lock(key)
            locked = False
            try:
                while not lock.acquire(timeout=.25):
                    cancelled()
                locked = True
                state = memory_retry.RetryState(ACTIVE, key, operation,
                    ACTIVE.estimate(operation, readonly=_readonly_operation.get() is not None),
                    headroom_key=memory_retry.command_key(None, operation, args, kwargs, shared_headroom=True))
                while True:
                    cancelled()
                    remaining = state.remaining(timeout)
                    started = None
                    try:
                        with telemetry.lock(authoritative_lease(
                                estimate_bytes=state.data['reservation_bytes'],
                                limit_bytes=state.data['limit_bytes'], controlled=True,
                                cancelled=cancelled), 'admission'):
                            started = time.monotonic()
                            cancelled()
                            state.begin()
                            ACTIVE.active[threading.get_ident()]['memory_attempt'] = state.data['launch_count']
                            result = fn(*args, **dict(kwargs, timeout_secs=min(remaining,
                                max(.001, rpc_deadline - started))))
                            ACTIVE.complete_execution()
                        if result.get('timed_out') and state.data['pressure_requeues'] and time.monotonic() >= rpc_deadline:
                            raise state.outcome('host_pressure_requeue', 'admission_deadline')
                        state.succeeded()
                        return result
                    except memory_retry.MemoryStopped as failure:
                        # The with block has reaped and released the failed
                        # lease, including its cgroup, before budget escalation.
                        if ACTIVE.cleanup_failed:
                            raise RuntimeError('compiler cleanup incomplete; memory retry refused')
                        state.stopped(failure, time.monotonic() - started)
                        if failure.cause == 'host_pressure':
                            state.wait_pressure(cancelled)
            except memory_retry.ExecutionBudgetExpired:
                state.succeeded()
                return dict(returncode=None, timed_out=True, stdout='', stderr='compiler recovery consumed its execution time limit')
            except memory_retry.ResourceOutcome as failure:
                callback = _authoritative_capacity_failure.get()
                if callback:
                    callback(failure.detail)
                else:
                    callback = _authoritative_resource_failure.get()
                    if callback:
                        callback(str(failure))
                from .protocol import ProtocolError
                raise ProtocolError(failure.detail['kind'] if _authoritative_capacity_failure.get()
                                    else 'resource_exhausted', str(failure)) from None
            finally:
                if locked:
                    lock.release()
        if ACTIVE and ACTIVE.available_memory and threading.get_ident() not in ACTIVE.active:
            return _available_call(fn, args, kwargs, exclusive=exclusive)
        with telemetry.lock(authoritative_lease(exclusive=exclusive), "admission"):
            return fn(*args, **kwargs)
    return wrapped


def _recover_available(fn, args, kwargs, *, exclusive):
    from . import serial_recovery, memory_retry, telemetry
    from .protocol import ProtocolError
    import inspect
    import subprocess
    compiler = fn.__name__ == '_run_lake_command'
    operation = _memory_operation.get() or _readonly_operation.get() or 'cold_compiler'
    if compiler:
        timeout = kwargs['timeout_secs']
        repo = Path(args[0] if args else kwargs['repo'])
        command = args[1] if len(args) > 1 else kwargs['args']
        inputs = serial_recovery.command_identity(repo, command)
        key = serial_recovery.digest([operation, inputs, _replay_input.get(), kwargs.get('bwrap_role'),
                                      kwargs.get('environment')])
        targets = list(_replay_input.get() or command)
    else:
        bound = inspect.signature(fn).bind(*args, **kwargs)
        values = bound.arguments
        timeout = values['timeout_secs']
        operation = 'helper:compiled_probe'
        key = serial_recovery.digest([operation, values['inputs']])
        targets = [values['module']]
    try:
        lock = serial_recovery.acquire_lock(ACTIVE, key, operation, timeout, check_cancelled)
        try:
            state = serial_recovery.SerialRetry(ACTIVE, key, operation, ACTIVE.maximum_job_bytes)
            state.initialize(timeout, targets=targets)
            while True:
                check_cancelled()
                state.remaining(timeout)
                started = None
                try:
                    options = state.lease_options()
                    options['exclusive'] |= exclusive
                    with telemetry.lock(authoritative_lease(**options), 'admission'):
                        remaining = state.remaining(timeout)
                        started = time.monotonic()
                        state.begin()
                        if compiler:
                            result = fn(*args, **dict(kwargs, timeout_secs=remaining))
                        else:
                            bound.arguments['timeout_secs'] = remaining
                            result = fn(*bound.args, **bound.kwargs)
                        ACTIVE.complete_execution()
                    if isinstance(result, dict) and result.get('timed_out'):
                        state.data['in_flight'] = False
                        raise state.unavailable('command_execution_timeout')
                    state.succeeded()
                    return result
                except memory_retry.MemoryStopped as failure:
                    if ACTIVE.cleanup_failed:
                        raise RuntimeError('compiler cleanup incomplete; memory retry refused')
                    state.stopped(failure, time.monotonic() - started)
                    state.remaining(timeout)
                    if compiler and operation == 'cold_compiler':
                        # Re-enter preparation before repeating a mutable build.
                        if _reprepare_boundary.get():
                            raise ReprepareMemory(repo, state.outcome('host_pressure_requeue',
                                'cold_materialization_interrupted').detail)
                        raise state.outcome('host_pressure_requeue', 'cold_materialization_interrupted')
                    if failure.cause == 'host_pressure':
                        state.wait_pressure(check_cancelled)
                except (TimeoutError, subprocess.TimeoutExpired):
                    state.data['in_flight'] = False
                    raise state.unavailable('command_execution_timeout')
        finally:
            lock.release()
    except memory_retry.ResourceOutcome as failure:
        callback = _authoritative_capacity_failure.get()
        if callback:
            callback(failure.detail)
        elif _authoritative_resource_failure.get():
            _authoritative_resource_failure.get()(str(failure))
        raise ProtocolError(failure.detail['kind'] if callback else 'resource_exhausted', str(failure)) from None


def _available_call(fn, args, kwargs, *, exclusive):
    """Non-restartable helpers unwind to their bounded host stage on failure."""
    from . import memory_retry, serial_recovery, telemetry
    from .protocol import ProtocolError
    operation = 'helper:' + fn.__module__ + '.' + fn.__name__
    key = serial_recovery.digest([_authoritative_owner.get(), operation])
    state = memory_retry.RetryState(ACTIVE, key, operation, ACTIVE.maximum_job_bytes, persist=False)
    deadline = time.monotonic() + ACTIVE.serial_wait_seconds
    def cancelled():
        check_cancelled()
        if time.monotonic() >= deadline:
            raise state.outcome('host_pressure_requeue', 'helper_admission_deadline')
    try:
        try:
            with telemetry.lock(authoritative_lease(exclusive=exclusive, controlled=True,
                    limit_bytes=ACTIVE.maximum_job_bytes, cancelled=cancelled,
                    cpu_units=1 if ACTIVE.cpu_accounting else None), 'admission'):
                started = time.monotonic()
                state.begin()
                result = fn(*args, **kwargs)
                ACTIVE.complete_execution()
            return result
        except memory_retry.MemoryStopped as failure:
            if ACTIVE.cleanup_failed:
                raise RuntimeError('compiler cleanup incomplete; memory retry refused')
            state.stopped(failure, time.monotonic() - started)
            if fn.__module__ == 'trellis.checker.heartbeat':
                return None if fn.__name__ == '_probe_builds_under_cap' else (None, failure.reason)
            if failure.cause == 'host_pressure':
                state.data['capacity_wait'] = True
            raise state.outcome('host_pressure_requeue', 'helper_requires_repreparation')
    except memory_retry.ResourceOutcome as failure:
        # Such a helper may have consumed an input spool before interruption;
        # only its enclosing host stage may prepare it again.
        failure.detail['bounded_recovery'] = True
        failure = memory_retry.ResourceOutcome(failure.detail)
        callback = _authoritative_capacity_failure.get()
        if callback:
            callback(failure.detail)
        elif _authoritative_resource_failure.get():
            _authoritative_resource_failure.get()(str(failure))
        raise ProtocolError(failure.detail['kind'] if callback else 'resource_exhausted', str(failure)) from None


def guarded_run(command, *, timeout, **kwargs):
    """Heartbeat probes share the same host-pressure guard as other compilers.

    Raise OSError on resource exhaustion, which measurement callers already
    classify as unavailable; it must never become a heartbeat-limit verdict.
    """
    import os
    import subprocess
    from trellis.memory_guard import MemoryGuard
    check_cancelled(force=True)
    if ACTIVE is None:
        return subprocess.run(command, timeout=timeout, **kwargs)
    if threading.get_ident() not in ACTIVE.active:
        with authoritative_lease():
            return guarded_run(command, timeout=timeout, **kwargs)
    if kwargs.pop("capture_output", False):
        kwargs.update(stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    guard = MemoryGuard(ACTIVE.execution_limit(), ACTIVE.execution_floor())
    deadline = time.monotonic() + timeout
    with subprocess.Popen(ACTIVE.process_command(command), **kwargs) as process:
        ACTIVE.track_process(process.pid)
        try:
            while True:
                check_cancelled()
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(command, timeout)
                try:
                    out, err = process.communicate(timeout=min(1.0, remaining))
                    failure = ACTIVE.memory_stop(guard, process.pid)
                    if failure:
                        if ACTIVE.active[threading.get_ident()].get('controlled'):
                            raise failure
                        raise OSError('checker resource exhaustion: ' + failure.reason)
                    ACTIVE.check_carrier(process)
                    return subprocess.CompletedProcess(command, process.returncode, out, err)
                except subprocess.TimeoutExpired:
                    failure = ACTIVE.memory_stop(guard, process.pid)
                    if failure:
                        if ACTIVE.active[threading.get_ident()].get('controlled'):
                            raise failure
                        raise OSError('checker resource exhaustion: ' + failure.reason)
        finally:
            if process.poll() is None:
                process.kill()
            process.wait()
            ACTIVE.observe_peak(guard.peak_bytes, completed=process.returncode == 0)
            ACTIVE.track_process(process.pid, remove=True)

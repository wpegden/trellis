"""Durable, scoped advisory jobs hosted by the authoritative checker process.

Journal-before-ack, input-based single flight, bounded queue, reconnect/cancel,
private captured workspaces, and shared compiler admission. These results are
advisory: the existing authoritative materialization/replay gates are intact.
"""
from __future__ import annotations
import hashlib
import base64
import json
import os
from pathlib import Path
import re
import secrets
import select
import shutil
import signal
import socket
import sqlite3
import struct
import subprocess
import threading
import time
import uuid
from contextlib import contextmanager

from trellis import incremental_check as ic
from trellis.check_job import TERMINAL, SCRATCH_ROOTS, input_path
from trellis.checker import admission
from trellis.checker import memory_retry
from trellis.checker import serial_recovery
from trellis.checker.workspaces import capability_live
from trellis.checker.job_workspace import capture, prepare_mirror, compiler_command
from trellis.memory_guard import MemoryGuard, system_available_bytes, GIB


def enabled():
    return os.environ.get("TRELLIS_CHECK_JOBS", "") == "1"


def state_dir(runtime):
    return Path(runtime) / "checker-state/jobs"


def socket_path(runtime):
    # Separate mount: an untrusted burst must not gain access to the authoritative socket.
    return Path(runtime) / "check-jobs-socket/jobs.sock"


def register_workspace(runtime, repo, *, scope, lifetime=86400, priority="interactive", provider_progress=None):
    """Trusted host launcher only; this registry is never mounted in a burst."""
    if priority not in {"interactive", "background"}:
        raise ValueError("invalid trusted workspace priority")
    token = secrets.token_urlsafe(32)
    key = hashlib.sha256(token.encode()).hexdigest()
    directory = state_dir(runtime) / "capabilities"
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    value = dict(repo=str(Path(repo).resolve()), scope=scope, priority=priority,
                 owner=ic._process_identity(os.getpid()),
                 expires=None if lifetime is None else time.time() + lifetime)
    if provider_progress is not None:
        value['provider_progress'] = provider_progress
    temp = directory / (key + ".tmp")
    temp.write_text(json.dumps(value))
    temp.chmod(0o600)
    temp.replace(directory / (key + ".json"))
    return {"TRELLIS_CHECK_JOBS_SOCKET": str(socket_path(runtime)),
            "TRELLIS_CHECK_JOBS_TOKEN": token}


def revoke_workspace(runtime, token):
    """Revoke new requests and cancel this attempt's queued/running jobs."""
    if not isinstance(token, str) or not 16 <= len(token) <= 128:
        raise ValueError("expected a workspace capability")
    key = hashlib.sha256(token.encode()).hexdigest()
    directory = state_dir(runtime) / "capabilities"
    (directory / (key + ".json")).unlink(missing_ok=True)
    if directory.exists():
        fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)


class JobStopped(BaseException):
    def __init__(self, state, message):
        self.state, self.message = state, message
        super().__init__(message)


class WorkspaceSession:
    def __init__(self):
        self.broker = None
        self.broker_owner = None
        self.last_used = time.monotonic()
        self.resident_spec = None
        self.current_check = None
        self.file_mode = None
        # Dependency outputs outlive the resident LSP process. This receipt
        # describes only the current host-owned mirror, never worker artifacts.
        self.materialized_dependencies = None


class JobService:
    def __init__(self, runtime, *, scheduler=None, execute=None):
        self.root = state_dir(runtime)
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.root.chmod(0o700)
        self.path = socket_path(runtime)
        self.scheduler = scheduler or admission.Admission()
        self.execute_override = execute
        code = [Path(__file__), Path(ic.__file__), Path(__file__).with_name("job_workspace.py"),
                Path(__file__).resolve().parents[2] / "scripts/lean_local_closure.lean"]
        self.implementation = hashlib.sha256(b"".join(p.read_bytes() for p in code)).hexdigest()
        self.lock = threading.RLock()
        # Leaf lock for the learned memory peaks. The admission estimate reads
        # them while the admission lock is held, so it must never wait for the
        # database lock: admission -> DB is the only permitted nesting, and a
        # caller holding the database lock while it reaches admission would
        # otherwise deadlock against a waiting worker.
        self.peaks_lock = threading.Lock()
        self.stop = threading.Event()
        self.wake = threading.Event()
        self.cancelled = set()
        self.sessions = {}
        self.claimed = {}
        self.wait_reasons = {}
        self.active_retries = {}
        self.memory_peaks = {}
        log = self.root / "checks.jsonl"
        if log.exists():
            with log.open("rb") as stream:
                stream.seek(max(0, log.stat().st_size - 4 * 1024 * 1024))
                for line in stream:
                    try:
                        row = json.loads(line)
                        target = row["node"]
                        kind = row.get("kind") or ("incremental" if row.get("execution_backend") == "incremental" else
                            "scratch" if any(target.startswith(root + "/") for root in SCRATCH_ROOTS) else "file" if target.endswith(".lean") else "build")
                        peak = row.get("peak_tree_rss_bytes") or 0
                        key = self._memory_key(kind, target, row.get("memory_operation"))
                        self.memory_peaks[key] = max(peak, self.memory_peaks.get(key, 0))
                    except (ValueError, KeyError, TypeError):
                        continue
        self.db = sqlite3.connect(self.root / "jobs.sqlite3", check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.executescript('''
          CREATE TABLE IF NOT EXISTS jobs (
            id TEXT PRIMARY KEY, owner TEXT NOT NULL, dedup TEXT NOT NULL,
            request TEXT NOT NULL, snapshot TEXT NOT NULL, state TEXT NOT NULL,
            submitted REAL NOT NULL, started REAL, finished REAL, result TEXT);
          CREATE TABLE IF NOT EXISTS requests (
            owner TEXT, request_id TEXT, payload TEXT, job TEXT,
            PRIMARY KEY(owner, request_id));
          CREATE TABLE IF NOT EXISTS processes (identity TEXT PRIMARY KEY);
        ''')
        # Never relaunch a job whose outcome became uncertain on restart.
        # Stop any identity-matched old warm namespaces BEFORE serving jobs.
        for row in self.db.execute("SELECT identity FROM processes").fetchall():
            self._stop_identity(json.loads(row[0]))
        self.db.execute("DELETE FROM processes")
        self.db.execute("UPDATE jobs SET state='interrupted', finished=?, result=? WHERE state NOT IN (%s)" % ",".join("?" for _ in TERMINAL),
                        (time.time(), json.dumps(dict(message="checker restarted before the job completed; submit a new check")), *sorted(TERMINAL)))
        self.db.commit()
        # Recover old drained pressure parking automatically. Hard exhaustion
        # is completed advisory feedback, not a wait for a policy change.
        for row in self.db.execute("SELECT * FROM jobs WHERE state='resource_exhausted'").fetchall():
            result = json.loads(row['result'] or '{}')
            detail = result.get('capacity_blocked')
            spec = json.loads(row['request'])
            if (isinstance(detail, dict) and detail.get('drained') is True
                    and detail.get('cause') == 'host_pressure' and self._live_spec(spec)):
                self.db.execute("UPDATE jobs SET state='queued',finished=NULL,result=NULL WHERE id=?", (row['id'],))
            elif isinstance(detail, dict) and detail.get('cause') in {'execution_limit', 'cgroup_oom'}:
                result.pop('capacity_blocked')
                result.update(message='Compiler exhausted its previous memory allowance; this advisory check is complete.',
                              legacy_capacity_detail=detail)
                self.db.execute("UPDATE jobs SET result=? WHERE id=?", (json.dumps(result), row['id']))
        self.db.commit()
        self.workers = [threading.Thread(target=self._work, name=f"check-jobs-{i}", daemon=True)
                        for i in range(self.scheduler.max_active)]
        self.worker = self.workers[0]
        self.listener = None
        self.acceptor = None
        self.connection_slots = threading.BoundedSemaphore(16)
        from .mirror_retention import MirrorRetirement
        self.mirror_retirement = MirrorRetirement(self.root, self.stop)

    @staticmethod
    def _stop_identity(identity):
        pid = identity.get("pid")
        if type(pid) is not int or pid <= 1:
            raise RuntimeError("invalid recorded compiler identity")
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            try:
                if ic._process_identity(pid) != identity:
                    return
            except FileNotFoundError:
                return
            signal.pidfd_send_signal(fd, signal.SIGKILL)
            if not select.select([fd], [], [], 10)[0]:
                raise RuntimeError("previous compiler has not exited; admission remains closed")
        finally:
            os.close(fd)

    def _remember(self, proc):
        # A setuid bwrap briefly makes /proc/<pid>/ns/pid unreadable during
        # exec. Keep ownership of this unreaped Popen child while waiting for
        # the complete identity; never substitute a weaker identity. Once
        # poll reaps it, return without reading a potentially reused PID.
        deadline = time.monotonic() + 0.5
        while True:
            if proc.poll() is not None:
                return None
            try:
                identity = ic._process_identity(proc.pid)
                break
            except (PermissionError, FileNotFoundError) as error:
                if proc.poll() is not None:
                    return None
                remaining = deadline - time.monotonic()
                if not isinstance(error, PermissionError) or remaining <= 0:
                    raise
                time.sleep(min(0.01, remaining))
        self.scheduler.track_process(proc.pid)
        with self.lock:
            self.db.execute("INSERT OR IGNORE INTO processes VALUES (?)", (json.dumps(identity, sort_keys=True),))
            self.db.commit()
        return identity

    def _forget(self, identity):
        self.scheduler.track_process(identity["pid"], remove=True)
        with self.lock:
            self.db.execute("DELETE FROM processes WHERE identity=?", (json.dumps(identity, sort_keys=True),))
            self.db.commit()

    def _capability(self, token):
        if not isinstance(token, str) or not 16 <= len(token) <= 128:
            raise ValueError("missing workspace capability")
        owner = hashlib.sha256(token.encode()).hexdigest()
        try:
            cap = json.loads((self.root / "capabilities" / (owner + ".json")).read_text())
            if not capability_live(cap):
                raise ValueError("expired workspace capability; this agent attempt has ended")
        except (OSError, KeyError, TypeError):
            raise ValueError("unknown or expired workspace capability") from None
        return owner, cap

    @staticmethod
    def _validate(req):
        extra = req.keys() - {"action", "token", "request_id", "kind", "target", "timeout"}
        if extra:
            raise ValueError("unsupported check fields: " + ", ".join(sorted(extra)))
        kind, target = req.get("kind"), req.get("target")
        if kind not in {"build", "incremental", "scratch", "file", "local_closure", "tool_info"} or not isinstance(target, str):
            raise ValueError("expected build/incremental with a node, file with Tablet/*.lean, or scratch with a Lean file under " + ", ".join(SCRATCH_ROOTS))
        if kind == "tool_info":
            if target not in {tool + '/' + arg for tool in ('lean', 'lake') for arg in ('--version', '--help', '-h')} | {'lean/--print-prefix', 'elan/which-lean'}:
                raise ValueError("unsupported tool metadata query")
        elif kind in {"build", "incremental", "local_closure"}:
            target = ic._parse_target(target)
        else:
            target = input_path(target, kind)
        timeout = req.get("timeout", 1800)
        if type(timeout) is not int or not 1 <= timeout <= 3600:
            raise ValueError("timeout must be an integer from 1 to 3600 seconds")
        request_id = req.get("request_id")
        if not isinstance(request_id, str) or re.fullmatch(r"[a-zA-Z0-9_-]{1,96}", request_id) is None:
            raise ValueError("request_id must contain 1–96 letters, digits, underscores or hyphens")
        return dict(kind=kind, target=target, timeout=timeout)

    def _collect_ended_attempts(self):
        expired = set()
        for row in self.db.execute("SELECT DISTINCT owner FROM jobs"):
            key = row[0]
            try:
                cap = json.loads((self.root / "capabilities" / (key + ".json")).read_text())
                live = capability_live(cap)
            except (OSError, ValueError, KeyError):
                live = False
            if not live:
                expired.add(key)
        for key in expired:
            if any(claim[2] == key for claim in self.claimed.values()):
                continue  # A cancelled job may still own a queue ticket/lease.
            pending = self.db.execute("SELECT count(*) FROM jobs WHERE owner=? AND state IN ('queued','running')", (key,)).fetchone()[0]
            if pending:
                continue
            self.db.execute("DELETE FROM requests WHERE owner=?", (key,))
            self.db.execute("DELETE FROM jobs WHERE owner=?", (key,))
            (self.root / "capabilities" / (key + ".json")).unlink(missing_ok=True)
        self.db.commit()
        self._collect_snapshots()

    def _collect_snapshots(self):
        # These are advisory execution inputs, not sealed candidates or
        # authoritative evidence. Terminal APIs/dedup need only their retained
        # digest/results. Preserve current work, latest result/failure per
        # owner, and every exceptional outcome needed for recovery.
        referenced = set()
        latest, failed = set(), set()
        for row in self.db.execute("SELECT id,owner,snapshot,state FROM jobs ORDER BY submitted DESC,id DESC"):
            owner, state = row['owner'], row['state']
            keep = (state not in TERMINAL or row['id'] in self.claimed
                    or state in {'infrastructure_error', 'timed_out', 'resource_exhausted', 'interrupted'})
            if state in TERMINAL and owner not in latest:
                latest.add(owner)
                keep = True
            if state == 'failed' and owner not in failed:
                failed.add(owner)
                keep = True
            if keep:
                referenced.add(row['snapshot'])
        for path in (self.root / "snapshots").glob("*.json"):
            if path.stem not in referenced:
                path.unlink()

    def _collect_mirror_builds(self):
        # Follow admission -> DB lock order. Reap acknowledgements, not a dead
        # token alone, establish that cached outputs have no compiler readers.
        # Be conservative while any compiler lease is active; the next submit
        # or release can collect these reconstructible outputs when idle.
        with self.scheduler.condition, self.lock:
            if self.scheduler.cleanup_failed or self.scheduler.active:
                return
            protected = set()
            for path in (self.root / 'capabilities').glob('*.json'):
                try:
                    cap = json.loads(path.read_text())
                    if capability_live(cap):
                        protected.add(hashlib.sha256(cap['repo'].encode()).hexdigest())
                except (OSError, ValueError, KeyError, TypeError):
                    # An unreadable binding is not permission to discard data.
                    return
            for row in self.db.execute('SELECT id,request,state FROM jobs'):
                if row['state'] not in TERMINAL or row['id'] in self.claimed:
                    protected.add(hashlib.sha256(json.loads(row['request'])['repo'].encode()).hexdigest())
            for repo, session in self.sessions.items():
                if session.broker is not None:
                    protected.add(hashlib.sha256(repo.encode()).hexdigest())
            for directory in (self.root / 'workspaces').glob('*'):
                key = directory.name.removesuffix('-tool-info')
                if not re.fullmatch('[a-f0-9]{64}', key) or key in protected:
                    continue
                def invalidate():
                    for repo, session in self.sessions.items():
                        if hashlib.sha256(repo.encode()).hexdigest() == key:
                            session.materialized_dependencies = None
                if self.mirror_retirement.detach(directory, invalidate):
                    continue
                build = directory / 'repo/.lake/build'
                if any(p.is_symlink() for p in [build, *build.parents]):
                    continue
                if build.is_dir():
                    # A failed removal may already have deleted some outputs.
                    # Revoke the receipt before touching any dependency bytes.
                    for repo, session in self.sessions.items():
                        if hashlib.sha256(repo.encode()).hexdigest() == key:
                            session.materialized_dependencies = None
                    shutil.rmtree(build)

    def release(self, token):
        """Revoke one channel and acknowledge only after its compiler cleanup.

        This is intentionally usable with an already revoked token. It can
        cancel only that token's jobs and confers no checking authority.
        Never take the admission lock while holding the database lock: warm
        eviction takes them in the opposite order when recording a process exit.
        """
        with self.lock:
            revoke_workspace(self.root.parent.parent, token)
            owner = hashlib.sha256(token.encode()).hexdigest()
            rows = self.db.execute("SELECT id,state FROM jobs WHERE owner=?", (owner,)).fetchall()
            for row in rows:
                if row['state'] not in TERMINAL:
                    self.cancelled.add(row['id'])
                    if row['state'] == 'queued' and row['id'] not in self.claimed:
                        self._finish(row['id'], 'cancelled', dict(message='compiler capability revoked before execution'))
            busy = any(claim[2] == owner for claim in self.claimed.values())
            busy |= bool(self.db.execute("SELECT 1 FROM jobs WHERE owner=? AND state IN ('queued','running') LIMIT 1", (owner,)).fetchone())
            self.wake.set()
        if busy:
            return dict(drained=False)
        self.scheduler.evict_idle(owner)
        with self.scheduler.condition:
            if self.scheduler.cleanup_failed:
                raise RuntimeError('compiler cleanup incomplete; retain the worker slot and restart the checker to reconcile recorded processes')
            busy = any(r['owner'] == owner for r in self.scheduler.active.values()) or owner in self.scheduler.residents
        if not busy:
            self._collect_mirror_builds()
        return dict(drained=not busy)

    def handle(self, req):
        action = req.get("action")
        if action == 'release':
            if req.keys() - {'action', 'token'}:
                raise ValueError('release accepts only this workspace capability')
            return self.release(req.get('token'))
        if action == 'submit':
            with self.lock:
                self._capability(req.get('token'))
            self._collect_mirror_builds()
        with self.lock:
            # Revocation and submit share this lock. A request which reached
            # the listener before release cannot enqueue after the drain ack.
            owner, cap = self._capability(req.get("token"))
            if action in {"status", "cancel"}:
                job = self.db.execute("SELECT * FROM jobs WHERE id=? AND owner=?", (req.get("job_id"), owner)).fetchone()
                if job is None:
                    raise ValueError("job not found in this workspace capability")
                if action == "cancel" and job["state"] not in TERMINAL:
                    self.cancelled.add(job["id"])
                    if job["state"] == "queued":
                        self._finish(job["id"], "cancelled", dict(message="cancelled before execution"))
                    self.wake.set()
                return self._reply(self.db.execute("SELECT * FROM jobs WHERE id=?", (job["id"],)).fetchone(), cap)
            if action != "submit":
                raise ValueError("expected submit, status or cancel")
            self._collect_ended_attempts()
            spec = self._validate(req)
            payload = json.dumps(spec, sort_keys=True)
            prior = self.db.execute("SELECT payload,job FROM requests WHERE owner=? AND request_id=?", (owner, req["request_id"])).fetchone()
            if prior:
                if prior["payload"] != payload:
                    raise ValueError("request_id was already used for different check parameters")
                return self._reply(self.db.execute("SELECT * FROM jobs WHERE id=?", (prior["job"],)).fetchone(), cap)
            pending = self.db.execute("SELECT count(*) FROM jobs WHERE state IN ('queued','running')").fetchone()[0]
            # Capture once under this service's submission lock. Compiler inputs
            # are exactly these bytes, including edits made after submission.
            capture_started = time.monotonic()
            snapshot, data = capture(Path(cap["repo"]), metadata_only=spec["kind"] == "tool_info")
            capture_ms = (time.monotonic() - capture_started) * 1000
            inputs = json.loads(data)
            rel = f"Tablet/{spec['target']}.lean" if spec["kind"] in {"build", "incremental", "local_closure"} else spec["target"]
            if spec["kind"] == "tool_info" and "lean-toolchain" not in inputs:
                raise ValueError("tool metadata query requires an explicit lean-toolchain")
            if spec["kind"] != "tool_info" and rel not in inputs:
                raise ValueError(f"check input does not exist or is not a regular file: {rel}")
            dedup = hashlib.sha256((self.implementation + payload + snapshot).encode()).hexdigest()
            prior = self.db.execute("SELECT * FROM jobs WHERE owner=? AND dedup=? AND state IN ('queued','running','succeeded','failed') ORDER BY submitted DESC LIMIT 1", (owner, dedup)).fetchone()
            if prior is None:
                if pending >= 32:
                    raise ValueError("checker queue is full (32 jobs); wait for an existing job before submitting more")
                if self.db.execute("SELECT count(*) FROM jobs").fetchone()[0] >= 4096:
                    raise ValueError("checker job history is full; ask the supervisor to archive completed attempts")
                job_id = uuid.uuid4().hex
                snapshots = self.root / "snapshots"
                snapshots.mkdir(exist_ok=True)
                path = snapshots / (snapshot + ".json")
                if not path.exists():
                    stored = sum(p.stat().st_size for p in snapshots.glob("*.json"))
                    if stored + len(data) > 2 * 1024 ** 3:
                        raise ValueError("checker snapshot storage is full (2 GiB); ask the supervisor to archive completed attempts")
                    temp = path.with_suffix(".tmp")
                    with temp.open("wb") as stream:
                        stream.write(data)
                        stream.flush()
                        os.fsync(stream.fileno())
                    temp.replace(path)
                    directory_fd = os.open(snapshots, os.O_RDONLY | os.O_DIRECTORY)
                    try:
                        os.fsync(directory_fd)
                    finally:
                        os.close(directory_fd)
                spec.update(repo=cap["repo"], scope=cap["scope"], capability=owner, capture_ms=capture_ms,
                            priority=cap.get("priority", "background" if cap["scope"].startswith("sc-") else "interactive"),
                            owner_identity=cap["owner"], expires=cap["expires"])
                self.db.execute("INSERT INTO jobs (id,owner,dedup,request,snapshot,state,submitted) VALUES (?,?,?,?,?,'queued',?)",
                                (job_id, owner, dedup, json.dumps(spec), snapshot, time.time()))
            else:
                job_id = prior["id"]
            if self.db.execute("SELECT count(*) FROM requests").fetchone()[0] >= 16384:
                self.db.rollback()
                raise ValueError("too many request IDs; reconnect using an existing job ID")
            self.db.execute("INSERT INTO requests VALUES (?,?,?,?)", (owner, req["request_id"], payload, job_id))
            self.db.commit()
            self.wake.set()
            return self._reply(self.db.execute("SELECT * FROM jobs WHERE id=?", (job_id,)).fetchone(), cap)

    def _reply(self, row, capability):
        from .provider_progress import decorate
        reply = self._public(row)
        return decorate(reply, capability, json.loads(row['request'])['kind'])

    def _public(self, row):
        value = {"job_id": row["id"], "state": row["state"], "input_sha256": row["snapshot"],
                 "submitted_at": row["submitted"], "started_at": row["started"], "finished_at": row["finished"]}
        if row["state"] == "queued":
            value["waiting_reason"] = self.wait_reasons.get(row["id"], "waiting for a compiler slot")
        if row["result"]:
            value["result"] = json.loads(row["result"])
            if row["state"] in {"succeeded", "failed"}:
                try:
                    spec = json.loads(row["request"])
                    value["current_inputs_match"] = capture(Path(spec["repo"]), metadata_only=spec["kind"] == "tool_info")[0] == row["snapshot"]
                except (OSError, ValueError):
                    value["current_inputs_match"] = False
        return value

    def _finish(self, job_id, state, result):
        with self.lock:
            before = self.db.execute("SELECT state FROM jobs WHERE id=?", (job_id,)).fetchone()
            if before is None or before["state"] in TERMINAL:
                return
            self.wait_reasons.pop(job_id, None)
            self.db.execute("UPDATE jobs SET state=?,finished=?,result=? WHERE id=?", (state, time.time(), json.dumps(result), job_id))
            self.db.commit()
            self.cancelled.discard(job_id)
            row = self.db.execute("SELECT * FROM jobs WHERE id=?", (job_id,)).fetchone()
            spec = json.loads(row["request"])
            key = self._memory_key(spec["kind"], spec["target"], result.get("memory_operation"))
            with self.peaks_lock:
                self.memory_peaks[key] = max(result.get("peak_tree_rss_bytes") or 0, self.memory_peaks.get(key, 0))
            event = dict(ts=time.time(), backend="managed_execution", node=spec["target"],
                         kind=spec["kind"], priority=spec.get("priority", "interactive"),
                         reservation_bytes=result.get("reservation_bytes"),
                         memory_operation=result.get("memory_operation"),
                         memory_sample_complete=result.get("memory_sample_complete"),
                         memory_attempt_count=result.get('memory_attempt_count'),
                         memory_discarded_execution_ms=result.get('memory_discarded_execution_ms'),
                         execution_limit_bytes=result.get('execution_limit_bytes'),
                         capacity_blocked=result.get('capacity_blocked'),
                         resource_rejected=result.get('resource_rejected'),
                         memory_hard_attempt_count=result.get('memory_hard_attempt_count'),
                         memory_pressure_requeues=result.get('memory_pressure_requeues'),
                         memory_pressure_execution_ms=result.get('memory_pressure_execution_ms'),
                         job_id=job_id, verdict=state, caller=spec["scope"],
                         input_sha256=row["snapshot"], implementation=self.implementation,
                         elapsed_ms=result.get("execution_ms", max(0.0, (row["finished"] - (row["started"] or row["finished"])) * 1000)),
                         queue_ms=result.get("queue_ms", ((row["started"] or row["finished"]) - row["submitted"]) * 1000),
                         execution_backend=result.get("backend"),
                         peak_tree_rss_bytes=result.get("peak_tree_rss_bytes"),
                         capture_ms=spec.get("capture_ms"), phases=result.get("phases"),
                         memory_metric="sampled process-tree RSS; shared pages may be counted twice")
            try:
                with (self.root / "checks.jsonl").open("a") as log:
                    log.write(json.dumps(event) + "\n")
            except OSError:
                pass  # Measurement failure cannot change a check result.

    def _check_stop(self, job, spec, deadline, guard=None, proc=None):
        with self.lock:
            row = self.db.execute("SELECT state FROM jobs WHERE id=?", (job["id"],)).fetchone()
            already_cancelled = row is not None and row["state"] == "cancelled"
        if self.stop.is_set() or already_cancelled or job["id"] in self.cancelled:
            raise JobStopped("cancelled", "check cancelled")
        if time.monotonic() >= deadline:
            raise JobStopped("timed_out", f"check exceeded its {spec['timeout']} second execution limit")
        if not self._live_spec(spec):
            raise JobStopped("cancelled", "agent attempt ended or its compiler capability was revoked")
        if proc is not None:
            self.scheduler.check_carrier(proc)
        if guard is not None and proc is not None:
            failure = self.scheduler.memory_stop(guard, proc.pid)
            if failure:
                if self.scheduler.active[threading.get_ident()].get('controlled'):
                    raise failure
                raise JobStopped("resource_exhausted", failure.reason)

    def _live_spec(self, spec):
        # Capturing a job does not extend a revoked capability's lifetime.
        # Check the original binding as well as owner liveness: cancellation
        # must work before a still-live agent/acceptance launcher is reaped.
        try:
            cap = json.loads((self.root / "capabilities" / (spec["capability"] + ".json")).read_text())
            return (cap["repo"] == spec["repo"] and cap["scope"] == spec["scope"]
                    and cap["owner"] == spec["owner_identity"] and cap["expires"] == spec["expires"]
                    and capability_live(cap))
        except (OSError, ValueError, KeyError, TypeError):
            return False

    def _evict(self, session):
        if session.broker is not None:
            session.broker._teardown_server()
        if session.broker_owner is not None:
            self.scheduler.drop_residency(session.broker_owner)
        session.broker = session.broker_owner = None
        session.file_mode = None

    def _expire_idle(self):
        with self.lock:
            sessions = list(self.sessions.values())
        for session in sessions:
            spec = session.resident_spec
            if spec is None:
                continue
            available = system_available_bytes()
            if (not self._live_spec(spec) or time.monotonic() - session.last_used > 300
                    or (available is not None and available < self.scheduler.floor_bytes + 2 * GIB)):
                self.scheduler.evict_idle(spec["capability"])

    def _take_job(self):
        # Claim a workspace before leaving the DB lock. Different capabilities
        # for the same checkout must still have exactly one mutable builder.
        with self.lock:
            rows = self.db.execute("SELECT * FROM jobs WHERE state='queued' ORDER BY submitted").fetchall()
            rows.sort(key=lambda row: json.loads(row["request"]).get("priority") == "background")
            background_claimed = any(v[1] == "background" for v in self.claimed.values())
            for job in rows:
                spec = json.loads(job["request"])
                retry = json.loads(job['result'] or '{}').get('memory_retry', {})
                if retry.get('retry_not_before', 0) > time.time():
                    continue
                if job["id"] in self.claimed or any(v[0] == spec["repo"] for v in self.claimed.values()):
                    continue
                if spec.get("priority") == "background" and background_claimed:
                    continue
                self.claimed[job["id"]] = (spec["repo"], spec.get("priority", "interactive"), job['owner'])
                return job, spec, self.sessions.setdefault(spec["repo"], WorkspaceSession())
        return None

    @staticmethod
    def _memory_key(kind, target, operation=None):
        # Old measurements have no cohort. Keep them for cold/default work;
        # only an explicitly host-classified reused-dependency job is separate.
        if operation == "advisory:" + kind + ":dependencies_reused":
            return kind, target, "dependencies_reused"
        return kind, target

    def _estimate(self, spec, operation=None):
        if spec["kind"] == "tool_info":
            return 128 * 1024 * 1024
        with self.peaks_lock:  # never the database lock: this runs under the admission lock
            peak = self.memory_peaks.get(self._memory_key(spec["kind"], spec["target"], operation))
        learned = self.scheduler.estimate(operation or "advisory:" + spec["kind"])
        if self.scheduler.optimistic and memory_retry.eligible(operation):
            return learned  # Historical tails set recovery floors/ceilings, not every initial reservation.
        return max(learned, 6 * GIB, peak * 1.25 + 2 * GIB) if peak else learned

    def _captured_inputs(self, spec, job):
        key = hashlib.sha256(spec["repo"].encode()).hexdigest()
        if spec["kind"] == "tool_info":
            key += '-tool-info'
        repo = self.root / "workspaces" / key / "repo"
        snapshot = (self.root / "snapshots" / (job["snapshot"] + ".json")).read_bytes()
        incoming = json.loads(snapshot)
        previous = repo.parent / "input-files.json"
        old = json.loads(previous.read_text()) if previous.exists() else []
        differences = {p for p in set(old) | set(incoming)
                       if p not in incoming or not (repo / p).is_file()
                       or base64.b64encode((repo / p).read_bytes()).decode() != incoming[p]}
        dependency_inputs = None
        if spec["kind"] in {"scratch", "file"}:
            target_text = base64.b64decode(incoming[spec["target"]]).decode()
            dependency_inputs = (spec["target"], tuple(target_text.splitlines(keepends=True)
                [:ic._import_block_end_line(target_text)]),
                tuple(ic._direct_tablet_imports_in_text(target_text)))
        return repo, snapshot, incoming, differences, dependency_inputs

    @staticmethod
    def _dependencies_reused(session, differences, dependency_inputs):
        return (dependency_inputs is not None
                and session.materialized_dependencies == dependency_inputs
                and differences <= {dependency_inputs[0]})

    def _admission_inputs(self, spec, job, session):
        operation = "advisory:" + spec["kind"]
        if spec["kind"] not in {"file", "scratch"}:
            return operation, None
        captured = self._captured_inputs(spec, job)
        if self._dependencies_reused(session, *captured[-2:]):
            operation += ":dependencies_reused"
        # Cold/unknown work retains its existing operation and measurements.
        return operation, captured

    @contextmanager
    def _admitted(self, spec, job, session, waiting):
        # The workspace claim stays held while memory admission waits. Check
        # actual captured inputs again after waiting; a changed cohort releases
        # its lease before requesting a new one, never upgrading inside it.
        while True:
            operation, _ = self._admission_inputs(spec, job, session)
            self.active_retries.pop(job.get('id') if isinstance(job, dict) else job['id'], None)
            prior = json.loads(dict(job).get('result') or '{}').get('memory_retry')
            retry = None
            if self.scheduler.optimistic and (self.scheduler.available_memory or memory_retry.eligible(operation) or prior):
                retry_operation = prior['operation'] if prior else operation
                key = hashlib.sha256(json.dumps([job['owner'], job['snapshot'], spec['kind'],
                    spec['target'], self.implementation, retry_operation]).encode()).hexdigest()
                if self.scheduler.available_memory:
                    key = self._recovery_key(spec, job)
                    retry_operation = 'advisory:' + spec['kind']
                retry = serial_recovery.state(self.scheduler, key, retry_operation,
                    self._estimate(spec, operation))
                self.active_retries[job['id']] = retry
                if isinstance(retry, serial_recovery.SerialRetry):
                    retry.initialize(spec['timeout'], targets=[spec['target']])
                retry.remaining(spec['timeout'])
            with self.scheduler.acquire(job["owner"],
                    priority=2 if spec.get("priority") == "background" else 1,
                    estimate_bytes=(retry.data['reservation_bytes'] if retry else lambda: self._estimate(spec, operation)),
                    limit_bytes=retry.data['limit_bytes'] if retry else 0,
                    recovery=retry if isinstance(retry, serial_recovery.SerialRetry) else None,
                    exclusive=bool(isinstance(retry, serial_recovery.SerialRetry) and retry.data['serial']),
                    controlled=retry is not None and (self.scheduler.available_memory or memory_retry.eligible(operation)), operation=operation,
                    cancelled=lambda: self._check_stop(job, spec, float("inf")), waiting=waiting):
                current, captured = self._admission_inputs(spec, job, session)
                if current != operation:
                    continue
                yield operation, captured
                return

    def _recovery_key(self, spec, job):
        return serial_recovery.digest(['advisory', job['snapshot'], spec['kind'], spec['target'], self.implementation])

    @contextmanager
    def _cleanup_failed_session(self, session):
        try:
            yield
        except BaseException:
            self._evict(session)
            raise

    def _work(self):
        while not self.stop.is_set():
            self._expire_idle()
            chosen = self._take_job()
            if chosen is None:
                self.wake.wait(0.25)
                self.wake.clear()
                continue
            job, spec, session = chosen
            recovery_lock = None
            def waiting(reason):
                with self.lock:
                    self.wait_reasons[job["id"]] = reason
            try:
                if self.scheduler.available_memory:
                    recovery_lock = serial_recovery.acquire_lock(self.scheduler, self._recovery_key(spec, job),
                        'advisory:' + spec['kind'], spec['timeout'],
                        lambda: self._check_stop(job, spec, float('inf')))
                with self._admitted(spec, job, session, waiting) as (operation, captured), self._cleanup_failed_session(session):
                    estimate = self.scheduler.active[threading.get_ident()]["estimate"]
                    with self.lock:
                        self._check_stop(job, spec, float("inf"))
                        self.db.execute("UPDATE jobs SET state='running',started=? WHERE id=?", (time.time(), job["id"]))
                        self.db.commit()
                        self.wait_reasons.pop(job["id"], None)
                    start = time.monotonic()
                    retry = self.active_retries.get(job['id'])
                    remaining = retry.remaining(spec['timeout']) if retry else spec['timeout']
                    if retry:
                        retry.begin()
                        self.scheduler.active[threading.get_ident()]['memory_attempt'] = retry.data['launch_count']
                    if self.execute_override:
                        result = self.execute_override(spec, job)
                    else:
                        result = self._execute(spec, job, start + remaining, session, captured=captured)
                    for stream in ("stdout", "stderr"):
                        if stream in result:
                            key = hashlib.sha256(spec["repo"].encode()).hexdigest()
                            mirror = self.root / "workspaces" / key / "repo"
                            result[stream] = result[stream].replace(str(mirror), spec["repo"])
                    if sum(len(result.get(s, "").encode()) for s in ("stdout", "stderr")) > 4 * 1024 * 1024:
                        raise JobStopped("infrastructure_error", "compiler output exceeded the 4 MiB limit")
                    # Only the host's terminal LSP diagnostics path may finish
                    # a failed check's memory sample. Exit 1 from an arbitrary
                    # compiler/fallback is not evidence of completed work.
                    diagnostics_complete = result.pop("_diagnostics_complete", False) is True
                    memory_complete = (result.get("returncode") == 0 or
                        (diagnostics_complete and result.get("backend") == "incremental"
                         and result.get("returncode") == 1))
                    # Output rejection is infrastructure failure, even when the
                    # compiler emitted a successful-looking terminal result.
                    self.scheduler.observe_peak(result.get("peak_tree_rss_bytes") or 0,
                                                completed=memory_complete)
                    discarded = retry.data['discarded_seconds'] if retry else 0
                    result.update(memory_sample_complete=memory_complete, queue_ms=max(0,
                                  time.time() - job["submitted"] - (time.monotonic() - start) - discarded) * 1000,
                                  execution_ms=(time.monotonic() - start + discarded) * 1000,
                                  memory_attempt_count=retry.data['launch_count'] if retry else 1,
                                  memory_hard_attempt_count=retry.data['attempt_count'] if retry else 0,
                                  memory_pressure_requeues=retry.data['pressure_requeues'] if retry else 0,
                                  memory_pressure_execution_ms=retry.data['pressure_execution_seconds'] * 1000 if retry else 0,
                                  memory_discarded_execution_ms=discarded * 1000,
                                  execution_limit_bytes=retry.data['limit_bytes'] if retry else 0,
                                  reservation_bytes=estimate, memory_operation=operation)
                    state = "succeeded" if result["returncode"] == 0 else "failed"
                    if result["returncode"] < 0:
                        state = "infrastructure_error"
                        result["message"] = f"compiler terminated by signal {-result['returncode']}"
                    self.scheduler.complete_execution()
                # In particular, a serial session and its cgroup must drain
                # before success is durable or visible to the worker.
                if retry:
                    retry.succeeded()
                self._finish(job["id"], state, result)
            except memory_retry.MemoryStopped as exc:
                retry = self.active_retries[job['id']]
                # Failed-session cleanup and the outer admission context have
                # already drained the namespace and released its lease.
                if self.scheduler.cleanup_failed:
                    self._finish(job['id'], 'infrastructure_error', dict(message='compiler cleanup incomplete'))
                else:
                    try:
                        if self.scheduler.available_memory:
                            # The original captured input stays immutable.
                            # Interrupted cold writers leave no reusable local
                            # build or warm-session dependency receipt.
                            session.materialized_dependencies = None
                            key = hashlib.sha256(spec['repo'].encode()).hexdigest()
                            if spec['kind'] == 'tool_info':
                                key += '-tool-info'
                            build = self.root / 'workspaces' / key / 'repo/.lake/build'
                            if any(path.is_symlink() for path in (build, *build.parents)):
                                raise RuntimeError('interrupted advisory build path is symlinked')
                            if build.exists():
                                shutil.rmtree(build)
                        retry.stopped(exc, time.monotonic() - start)
                        if exc.cause == 'host_pressure':
                            retry.data['retry_not_before'] = time.time() + retry.pressure_delay()
                        with self.lock:
                            self.db.execute("UPDATE jobs SET state='queued',result=? WHERE id=?", (
                                json.dumps(dict(memory_retry=retry.data)), job['id']))
                            self.db.commit()
                    except Exception as error:
                        self._finish(job['id'], 'infrastructure_error', dict(message=str(error)))
            except memory_retry.ResourceOutcome as exc:
                if serial_recovery.is_capacity_wait(exc.detail):
                    with self.lock:
                        data = dict(exc.detail, retry_not_before=time.time() + 5)
                        self.db.execute("UPDATE jobs SET state='queued',result=? WHERE id=?", (
                            json.dumps(dict(memory_retry=data)), job['id']))
                        self.db.commit()
                        self.wait_reasons[job['id']] = 'waiting for memory capacity'
                else:
                    self._finish(job['id'], 'resource_exhausted', dict(
                        message=str(exc), resource_rejected=exc.detail))
            except memory_retry.ExecutionBudgetExpired as exc:
                self.active_retries[job['id']].succeeded()
                self._finish(job['id'], 'timed_out', dict(message=str(exc)))
            except JobStopped as exc:
                if session.broker_owner:
                    self.scheduler.evict_idle(session.broker_owner)
                retry = self.active_retries.get(job['id'])
                if exc.state == 'timed_out' and isinstance(retry, serial_recovery.SerialRetry):
                    retry.data['in_flight'] = False
                    outcome = retry.unavailable('command_execution_timeout')
                    self._finish(job['id'], 'resource_exhausted', dict(message=str(outcome), resource_rejected=outcome.detail))
                    continue
                if exc.state == 'timed_out' and retry:
                    retry.succeeded()
                self._finish(job["id"], exc.state, dict(message=exc.message))
            except Exception as exc:
                if session.broker_owner:
                    self.scheduler.evict_idle(session.broker_owner)
                self._finish(job["id"], "infrastructure_error", dict(message=f"{type(exc).__name__}: {exc}"))
            finally:
                if recovery_lock:
                    recovery_lock.release()
                with self.lock:
                    self.claimed.pop(job["id"], None)
                    self.active_retries.pop(job['id'], None)
                self.wake.set()

    def start(self):
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.path.unlink(missing_ok=True)
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        with ic._socket_address(self.path) as address:
            self.listener.bind(address)
        self.path.chmod(0o600)
        self.listener.listen(16)
        self.listener.settimeout(0.5)
        self.mirror_retirement.start()
        for worker in self.workers:
            worker.start()
        self.acceptor = threading.Thread(target=self._accept, name="check-job-listener", daemon=True)
        self.acceptor.start()

    def _accept(self):
        while not self.stop.is_set():
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            if not self.connection_slots.acquire(blocking=False):
                conn.close()
                continue
            threading.Thread(target=self._connection, args=(conn,), daemon=True).start()

    def _connection(self, conn):
        try:
            with conn:
                conn.settimeout(10)
                _pid, uid, _gid = struct.unpack("3i", conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                if uid != os.geteuid():
                    return
                header = ic._recv_exactly(conn, 4)
                if header is None:
                    return
                size = struct.unpack(">I", header)[0]
                if not 0 < size < 4096:
                    return
                try:
                    req = json.loads(ic._recv_exactly(conn, size))
                    if not isinstance(req, dict):
                        raise ValueError("expected a request object")
                    result = self.handle(req)
                except (ValueError, OSError, TypeError, RuntimeError) as exc:
                    result = {"error": str(exc)}
                data = json.dumps(result).encode()
                conn.sendall(struct.pack(">I", len(data)) + data)
        except OSError:
            pass  # The job belongs to the service, not the connection.
        finally:
            self.connection_slots.release()

    def shutdown(self):
        self.stop.set()
        self.wake.set()
        self.mirror_retirement.wake.set()
        with self.lock:
            for row in self.db.execute("SELECT id FROM jobs WHERE state='queued'").fetchall():
                self._finish(row["id"], "cancelled", dict(message="checker stopped before execution"))
        if self.listener:
            self.listener.close()
        deadline = time.monotonic() + 15
        for worker in self.workers:
            if worker.is_alive():
                worker.join(timeout=max(0, deadline-time.monotonic()))
        if any(worker.is_alive() for worker in self.workers):
            raise RuntimeError("compiler job did not stop; admission has not been released")
        for session in self.sessions.values():
            if session.broker_owner:
                self.scheduler.evict_idle(session.broker_owner)
        self.path.unlink(missing_ok=True)

    def _execute(self, spec, job, deadline, session, *, captured=None):
        from trellis.host_runtime import worker_elan_home
        prepare_started = time.monotonic()
        phases = {}
        source = Path(spec["repo"])
        tool_info = spec["kind"] == "tool_info"
        if tool_info:
            # Fixed metadata arguments never read Lake configuration or build
            # user code. Preserve the ordinary mirror and warm session.
            session = WorkspaceSession()
        repo, snapshot, incoming, differences, dependency_inputs = (
            captured if captured is not None else self._captured_inputs(spec, job))
        file_check = spec["kind"] in {"scratch", "file"}
        target_rel = spec["target"] if file_check else f"Tablet/{spec['target']}.lean"
        dependencies_reused = self._dependencies_reused(session, differences, dependency_inputs)
        if not dependencies_reused:
            # Invalidate before changing the mirror, including failed or
            # interrupted attempts to switch its source/configuration epoch.
            session.materialized_dependencies = None
        # A different, unopened target may import a module checked only in
        # memory. Materialize its dependencies before the read-only LSP server
        # asks Lake to set up that file; a warm workspace alone is insufficient.
        uri = (repo / target_rel).as_uri()
        warm_body_edit = (session.broker_owner == job["owner"]
                          and session.broker is not None
                          and session.file_mode == file_check
                          and uri in session.broker._open_docs
                          and differences <= {target_rel})
        if file_check and warm_body_edit:
            before = (repo / target_rel).read_text()
            after = base64.b64decode(incoming[target_rel]).decode()
            def header(text):
                return text.splitlines(keepends=True)[:ic._import_block_end_line(text)]
            warm_body_edit = (header(before) == header(after)
                              and ic._direct_tablet_imports_in_text(before) == ic._direct_tablet_imports_in_text(after))
        if not warm_body_edit:
            self._evict(session)
        if tool_info:
            repo.mkdir(parents=True, exist_ok=True)
            (repo/'lean-toolchain').write_bytes(base64.b64decode(incoming['lean-toolchain']))
        else:
            prepare_mirror(source, repo, snapshot)
        phases["prepare_ms"] = (time.monotonic() - prepare_started) * 1000
        phases["warm_session"] = warm_body_edit
        guard = MemoryGuard(self.scheduler.execution_limit(),
                            self.scheduler.execution_floor())
        env = dict(os.environ)
        env["PATH"] = str(worker_elan_home() / "bin") + os.pathsep + env.get("PATH", "")
        for name in ("TRELLIS_CHECK_JOBS_TOKEN", "TRELLIS_CHECK_JOBS_SOCKET", "TRELLIS_CHECKER_TOKEN", "TRELLIS_CHECKER_SOCKET"):
            env.pop(name, None)
        last_sample = [0.0]
        def check_process(proc):
            now = time.monotonic()
            sample = now - last_sample[0] >= 1.0
            if sample:
                last_sample[0] = now
            self._check_stop(job, spec, deadline, guard if sample else None, proc)
        session.current_check = check_process
        self._check_stop(job, spec, deadline)

        def run(args, executable="lake"):
            import tempfile
            # Writable compiler invocations can change build artifacts. Only
            # a successful dependency build below issues a new receipt.
            session.materialized_dependencies = None
            self._evict(session)
            command = self.scheduler.process_command(compiler_command(repo, [executable, *args],
                lean_threads=str(self.scheduler.lean_threads())))
            with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
                process = subprocess.Popen(command, cwd=repo, env=env, stdout=out, stderr=err)
                identity = None
                try:
                    identity = self._remember(process)
                    while process.poll() is None:
                        session.current_check(process)
                        if out.tell() + err.tell() > 4 * 1024 * 1024:
                            raise JobStopped("infrastructure_error", "compiler output exceeded the 4 MiB limit")
                        try:
                            process.wait(timeout=0.25)
                        except subprocess.TimeoutExpired:
                            pass
                    self._check_stop(job, spec, deadline, guard, process)
                    if out.tell() + err.tell() > 4 * 1024 * 1024:
                        raise JobStopped("infrastructure_error", "compiler output exceeded the 4 MiB limit")
                    out.seek(0); err.seek(0)
                    return dict(returncode=process.returncode, stdout=out.read().decode("utf-8", "replace"),
                                stderr=err.read().decode("utf-8", "replace"), backend="lake",
                                peak_tree_rss_bytes=guard.peak_bytes)
                finally:
                    # bwrap's parent-death/PID namespace boundary owns every
                    # compiler descendant; reap before releasing admission.
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=10)
                    if identity:
                        self._forget(identity)

        if tool_info:
            tool, argument = spec['target'].split('/', 1)
            if tool == 'elan':
                # Resolve exactly the captured selection, never a host default
                # or an agent-supplied command/environment override.
                env['ELAN_TOOLCHAIN'] = base64.b64decode(incoming['lean-toolchain']).decode().strip()
                result = run(['which', 'lean'], executable=tool)
                result['toolchain'] = env['ELAN_TOOLCHAIN']
            else:
                result = run([argument], executable=tool)
            result['backend'] = 'tool_info'
            return result
        if spec["kind"] == "local_closure":
            result = run(["build", "Tablet." + spec["target"]])
            if result["returncode"]:
                return result
            script = Path(__file__).resolve().parents[2] / "scripts/lean_local_closure.lean"
            return run(["env", "lean", "--run", str(script), spec["target"], "--principal=" + spec["target"]])
        giant = ic.is_giant_node(repo / target_rel)[0]
        if spec["kind"] == "build" or (giant and not file_check):
            return run(["build", "Tablet." + spec["target"]])
        phases["dependencies_reused"] = dependencies_reused
        if not warm_body_edit and not dependencies_reused:
            dependency_started = time.monotonic()
            dependencies = ic._direct_tablet_imports_in_text((repo / target_rel).read_text())
            if dependencies:
                result = run(["build", *("Tablet." + node for node in dependencies)])
                if result["returncode"]:
                    return result
            session.materialized_dependencies = dependency_inputs
            phases["dependencies_ms"] = (time.monotonic() - dependency_started) * 1000
        if giant:
            # `lake env lean FILE` does not build imports. Large file checks
            # need the same captured dependency preparation as the LSP path,
            # including rebuilding oleans after dependency source edits.
            result = run(["env", "lean", target_rel])
            phases["warm_session"] = False
            result["phases"] = phases
            return result
        service = self

        class ManagedLeanServer(ic._LeanServer):
            def __init__(self, workspace):
                launch = "ulimit -s unlimited 2>/dev/null || true; exec lake env lean --server"
                if file_check:
                    # `lake env lean FILE` does not apply package/module
                    # options or plugins from `lake setup-file`. Retain that
                    # interface: Lake supplies LEAN_PATH, then the server
                    # uses its documented no-Lake fallback for file setup.
                    # The service has already materialized every dependency.
                    launch = ("ulimit -s unlimited 2>/dev/null || true; exec lake env /bin/sh -c "
                              "'export LAKE=/trellis-managed-no-lake-setup; exec lean --server'")
                command = compiler_command(workspace, ["/bin/sh", "-c", launch], readonly_build=True,
                    lean_threads=str(service.scheduler.lean_threads()))
                command = service.scheduler.process_command(command)
                super().__init__(workspace, server_cmd=command, server_env=env, setup_stack=False)
                self.identity = None
                try:
                    self.identity = service._remember(self.proc)
                except BaseException:
                    # The broker cannot own a server whose constructor failed.
                    # Reap it here before the job releases compiler admission.
                    self.shutdown()
                    raise

            def drain(self, timeout):
                session.current_check(self.proc)
                return super().drain(min(timeout, 0.25))

            def shutdown(self):
                # Use the namespace lifetime, not didClose/exit as evidence
                # of freed memory. Killing the namespace also stops children
                # that Lean's watchdog has not yet waited for.
                if self.proc.poll() is None:
                    self.proc.kill()
                self.proc.wait(timeout=10)
                if self.identity is not None:
                    service._forget(self.identity)

        if session.broker is None:
            session.broker = ic._Broker(repo, server_factory=ManagedLeanServer, strict_teardown=True)
            session.broker_owner = job["owner"]
            session.file_mode = file_check
        compiler_started = time.monotonic()
        result = (session.broker.handle_file(repo / target_rel) if file_check
                  else session.broker.handle_request(dict(node=spec["target"])))
        phases["elaboration_ms"] = (time.monotonic() - compiler_started) * 1000
        self._check_stop(job, spec, deadline, guard, session.broker._srv.proc if session.broker._srv else None)
        if result.get("verdict") not in {"ok", "fail"}:
            reason = str(result.get("reason", "incremental engine unavailable"))
            result = run(["env", "lean", target_rel] if file_check else ["build", "Tablet." + spec["target"]])
            engine = "Lean file check" if file_check else "Lake build"
            result["stderr"] = f"check-job: using managed {engine} ({reason}).\n" + result["stderr"]
            result["phases"] = phases
            return result
        session.last_used = time.monotonic()
        session.resident_spec = spec
        self.scheduler.retain(job["owner"], lambda: self._evict(session))
        return dict(returncode=0 if result["verdict"] == "ok" else 1,
                    stdout="\n".join([*result.get("lines", []), *result.get("sorry", []),
                                        *([f"incremental-check: Tablet.{spec['target']}: OK."] if result["verdict"] == "ok" and not file_check else [])]) + "\n",
                    stderr="", backend="incremental", phases=phases, peak_tree_rss_bytes=guard.peak_bytes,
                    _diagnostics_complete=True)

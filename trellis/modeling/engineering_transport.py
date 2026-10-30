"""Staged engineering files on the existing preparation request/executor path."""
from __future__ import annotations

from dataclasses import dataclass, field, replace
from contextlib import ExitStack, contextmanager, nullcontext
from contextvars import ContextVar
import copy
import signal
import threading
import hashlib
import json
import os
from pathlib import Path
import re
import uuid
import sys
import subprocess

from trellis.modeling import engineering as eng, history, performance
from trellis.modeling.observations import read_json, write_json
from trellis.paper_implementation import evidence_path


_OWNER = ContextVar("engineering_request_owner", default=None)
_TAG_FIELDS = {"attempt", "request_sha256", "basis_sha256", "import_sha256", "outcome_sha256"}


def _read(path):
    from trellis.git_trust import read_engineering_json
    return read_engineering_json(path, limit=64 * 1024 * 1024)


def _sha(raw):
    return hashlib.sha256(raw).hexdigest()


@contextmanager
def _boundary():
    try:
        yield
    except (ValueError, OSError):
        raise RuntimeError("Engineering delivery boundary invalid") from None


@dataclass
class _RequestOwner:
    root: Path
    state: dict
    thread: int
    sequence: int
    task: dict
    attempt: Path | None = None
    reserved: Path | None = None
    request: object = None
    request_sha: str = ""
    basis_sha: str = ""
    dispatched: bool = False
    committed: dict | None = None
    publication: dict | None = None
    prior_publication: dict | None = None
    directories: ExitStack = field(default_factory=ExitStack)
    roots: dict = field(default_factory=dict)
    native: dict | None = None
    executions: dict = field(default_factory=dict)
    starting: bool = False
    cancelled: bool = False
    cleanup_unknown: bool = False

    def stop_executions(self):
        """Close this assignment before its original directory contexts leave."""
        for process in self.executions.values():
            path = process.root / 'process.json'
            # start_or_adopt may have been interrupted before its return. The
            # retained Popen handle, not absence of process.json, owns that gap.
            if not path.exists() and process.child is not None:
                # The launcher installs its termination handler before publishing
                # identity. Stop the retained child even if publication failed.
                if process.child.poll() is None:
                    process.child.terminate()
                try:
                    process.child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.child.kill()
                    process.child.wait(timeout=5)
            if not path.exists():
                raise RuntimeError('Engineering launcher closure is unknown')
            record = _read(path)[0]
            process.stop_and_reap(record)

    def bind_view(self, view, supplied):
        from trellis.agents.engineering_exec import tree_identity
        from trellis.agents.host_files import safe_directory
        with _boundary():
            if self.roots or any(_matches(name, (".trellis",)) for name in supplied):
                raise ValueError("Reserved delivery input")
            _, fd = self.directories.enter_context(safe_directory(view, create=False))
            info = os.fstat(fd)
            self.roots[view] = (fd, info.st_dev, info.st_ino)
            initial = {name: entry["sha256"] for name, entry in tree_identity(view).items()}
            if initial != supplied:
                raise ValueError("Supplied delivery changed")
            self.check_roots()
            os.mkdir(".trellis", mode=0o700, dir_fd=fd)
            self.check_roots()
            _, bookkeeping = self.directories.enter_context(safe_directory(view / ".trellis", create=False))
            info = os.fstat(bookkeeping)
            self.roots[view / ".trellis"] = (bookkeeping, info.st_dev, info.st_ino)
            self.check_roots()

    def check_roots(self):
        from trellis.agents.host_files import safe_directory
        with _boundary():
            if not self.roots:
                raise ValueError("Delivery roots are unbound")
            for path, (held, device, inode) in self.roots.items():
                original = os.fstat(held)
                with safe_directory(path, create=False) as (_, opened):
                    current = os.fstat(opened)
                if ((original.st_dev, original.st_ino) != (device, inode)
                        or (current.st_dev, current.st_ino) != (device, inode)):
                    raise ValueError("Delivery root changed")

    def inventory(self):
        from trellis.agents.engineering_exec import tree_identity
        with _boundary():
            view = self.reserved / "view"
            if set(self.roots) != {view, view / ".trellis"}:
                raise ValueError("Delivery roots are incomplete")
            self.check_roots()
            entries = tree_identity(view, ignored=[".trellis"])
            self.check_roots()
            return {name: entry["sha256"] for name, entry in entries.items()}

    def register(self, attempt, request):
        if (self.attempt is not None or not self.dispatched or attempt != self.reserved
                or request.cycle != self.sequence or request.work_dir.parent != attempt):
            raise ValueError("Engineering request ownership differs")
        saved, raw = _read(attempt / "request.json")
        expected, basis_raw = _read(attempt / "basis.json")
        if eng.digest(saved) != eng.digest(request.to_dict()) or expected != basis(self.root, self.state):
            raise ValueError("Engineering request publication differs")
        self.attempt, self.request = attempt, request
        self.request_sha, self.basis_sha = _sha(raw), _sha(basis_raw)

    def current(self, root, attempt, raw):
        return (self.root == root and self.thread == threading.get_ident()
                and self.attempt == attempt and self.sequence == self.state["sequence"]
                and self.request_sha == _sha(raw) and self.committed is None)

    def reconcile(self):
        if self.native is not None:
            return _native_reconcile(self)
        if self.publication is not None:
            saved, _ = _read(self.root / ".trellis/modeling/state.json")
            if saved == self.publication:
                self.state.clear()
                self.state.update(saved)
                self.committed = saved
            elif saved != self.prior_publication:
                raise ValueError("Engineering state publication is inconsistent")


@contextmanager
def request_owner(root, state, halt):
    """Own the existing Driver step through durable outcome publication."""
    if _OWNER.get() is not None or threading.current_thread() is not threading.main_thread():
        raise ValueError("Engineering requires exclusive main-thread request ownership")
    owner = _RequestOwner(root, state, threading.get_ident(), state["sequence"] + 1,
                          copy.deepcopy(state["next"]))
    token = _OWNER.set(owner)
    previous = {}
    cancelled = False
    cleaning = False

    def cancel(signum, frame):
        nonlocal cancelled
        first = not cancelled
        cancelled = True
        owner.cancelled = True
        if first and not cleaning and not owner.starting:
            raise KeyboardInterrupt("Engineering request cancelled")

    handled = False

    def record_failure():
        nonlocal handled, cleaning
        if handled:
            return
        handled = cleaning = True
        safe_state_save = True
        try:
            owner.stop_executions()
        except BaseException:
            owner.cleanup_unknown = True
            safe_state_save = False
        if not owner.cleanup_unknown:
            try:
                owner.reconcile()
            except BaseException:
                safe_state_save = False
        if owner.native is not None and owner.native.get('consumed_committed'):
            # A checkpoint consumed this assignment. Retain the original error;
            # projection repair must never relabel completed provider execution.
            return
        evidence = owner.attempt or root / eng.STORE / "deliveries"
        if owner.request is not None:
            try:
                from trellis.agent_wrapper.executor import engineering_execution_failure
                evidence = engineering_execution_failure(owner.request).evidence
            except BaseException:
                pass
        try:
            halt(evidence, save_state=safe_state_save)
        except BaseException:
            pass

    try:
        for sig in (signal.SIGINT, signal.SIGTERM):
            previous[sig] = signal.signal(sig, cancel)
        try:
            yield owner
        except BaseException:
            record_failure()
            raise
    finally:
        cleaning = True
        restoration_error = None
        try:
            try:
                if not owner.cleanup_unknown:
                    owner.stop_executions()
                    owner.directories.__exit__(*sys.exc_info())
            except BaseException as error:
                owner.cleanup_unknown = True
                restoration_error = error
            for sig, handler in previous.items():
                try:
                    signal.signal(sig, handler)
                except BaseException as error:
                    restoration_error = restoration_error or error
            if not handled and (cancelled or restoration_error is not None):
                record_failure()
                if restoration_error is not None:
                    raise restoration_error
                raise KeyboardInterrupt("Engineering request cancelled")
        finally:
            if not owner.cleanup_unknown:
                _OWNER.reset(token)
            # Unknown cleanup keeps this owner strongly referenced by the
            # ContextVar; ExitStack/GC cannot release its original pins.


def _dispatch_owner(root, state, task):
    owner = _OWNER.get()
    if (owner is None or owner.root != root or owner.state is not state
            or owner.thread != threading.get_ident() or owner.sequence != state["sequence"]
            or owner.task != task or owner.dispatched):
        raise ValueError("Engineering dispatch requires its fresh request owner")
    return owner


def publish_outcome(root, state):
    """Publish consumption on the normal handled-outcome path only."""
    owner = _OWNER.get()
    if owner is None or owner.root != root or owner.state is not state or owner.thread != threading.get_ident():
        raise ValueError("Engineering publication requires its request owner")
    if owner.committed is not None:
        if state != owner.committed:
            raise ValueError("Engineering consumed outcome changed")
        return
    if not owner.dispatched:
        write_json(root / ".trellis/modeling/state.json", state)
        return  # An existing local refresh may finish without dispatch.
    if owner.attempt is None or set(owner.roots) != {owner.attempt / "view", owner.attempt / "view/.trellis"}:
        raise RuntimeError("Engineering delivery boundary invalid")
    row = state["history"][-1]
    task = {k: v for k, v in owner.task.items() if k != "remaining"}
    if (row["sequence"] != owner.sequence or row["stage"] != "engineering"
            or row["task"] != task or "engineering_delivery" in row):
        raise ValueError("Engineering outcome identity differs")
    _, request_raw = _read(owner.attempt / "request.json")
    _, basis_raw = _read(owner.attempt / "basis.json")
    if _sha(request_raw) != owner.request_sha or _sha(basis_raw) != owner.basis_sha:
        raise ValueError("Engineering retained request changed")
    try:
        _, imported = _read(owner.attempt / "imported.json")
    except FileNotFoundError:
        imported = None
    saved = copy.deepcopy(state)
    saved["history"][-1]["engineering_delivery"] = dict(
        attempt=owner.attempt.relative_to(root).as_posix(), request_sha256=owner.request_sha,
        basis_sha256=owner.basis_sha, import_sha256=_sha(imported) if imported is not None else None,
        outcome_sha256=eng.digest(row))
    owner.prior_publication, _ = _read(root / ".trellis/modeling/state.json")
    owner.publication = saved
    try:
        owner.check_roots()
        write_json(root / ".trellis/modeling/state.json", saved)
    except BaseException:
        try:
            owner.reconcile()
        except BaseException:
            pass
        raise
    else:
        owner.reconcile()
    if owner.committed is None:
        raise ValueError("Engineering outcome publication is incomplete")


def check_unresolved_attempts(root: Path) -> None:
    """Read retained execution evidence before delivery, independent of state saves.

    The scan holds on exhaustion of 4096 entries or 64 MiB; it never skips evidence.
    Successful execution also requires exact saved controller consumption.
    """
    if read_json(root / 'config.json').get('engineering_protocol') == 'native-v1':
        return _native_unresolved(root)
    from trellis.agents.host_files import safe_directory
    from trellis.git_trust import read_engineering_json, validate_engineering_receipt
    from trellis.agent_wrapper.protocol import EngineeringExecutionFailure
    remaining_entries, remaining_bytes = 4096, 64 * 1024 * 1024
    evidence = root / eng.STORE / 'deliveries'

    def entries(path):
        nonlocal remaining_entries
        with safe_directory(path, create=False) as (_, fd), os.scandir(fd) as scan:
            names = []
            for entry in scan:
                remaining_entries -= 1
                if remaining_entries < 0:
                    raise ValueError('Engineering evidence scan exhausted')
                names.append(entry.name)
            return sorted(names)

    def read(path, limit=None):
        nonlocal remaining_bytes
        result, raw = read_engineering_json(path, limit=min(remaining_bytes, limit or remaining_bytes))
        remaining_bytes -= len(raw)
        return result, raw

    def receipt(path, identity, codes):
        value, _ = read(path, 65536)
        return validate_engineering_receipt(value, identity, codes)

    try:
        try:
            attempts = entries(evidence)
        except FileNotFoundError:
            return
        if not attempts:
            return
        saved, _ = read(root / ".trellis/modeling/state.json")
        rows = saved["history"]
        if not isinstance(rows, list) or len(rows) > remaining_entries:
            raise ValueError("Invalid engineering history")
        remaining_entries -= len(rows)
        consumed = {}
        for row in rows:
            if not isinstance(row, dict):
                raise ValueError("Invalid engineering history row")
            if "engineering_delivery" not in row:
                continue
            tag = row["engineering_delivery"]
            if (not isinstance(tag, dict) or set(tag) != _TAG_FIELDS
                    or not isinstance(tag["attempt"], str) or tag["attempt"] in consumed):
                raise ValueError("Invalid engineering history linkage")
            for field in _TAG_FIELDS - {"attempt"}:
                if field == "import_sha256" and tag[field] is None:
                    continue
                if not isinstance(tag[field], str) or not re.fullmatch(r"[0-9a-f]{64}", tag[field]):
                    raise ValueError("Invalid engineering history digest")
            consumed[tag["attempt"]] = row
        seen = set()
        for name in attempts:
            evidence = root / eng.STORE / 'deliveries' / name
            if not re.fullmatch(r'(0|[1-9][0-9]*)-[0-9a-f]{32}', name):
                raise ValueError('Invalid engineering attempt identity')
            names = entries(evidence)
            request, raw = read(evidence / 'request.json')
            sequence = int(name.split('-', 1)[0])
            home = Path(request['burst_home'])
            if (request.get('kind') != 'engineering' or request.get('burst_role') != 'engineering'
                    or type(request.get('cycle')) is not int or request['cycle'] != sequence
                    or not isinstance(request.get('request_id'), str)
                    or request.get('work_dir') != str(evidence / 'view')
                    or home.parent != evidence / 'burst-homes'):
                raise ValueError('Engineering request identity differs')
            if 'execution-failure.json' in names:
                failure = receipt(evidence / 'execution-failure.json',
                    hashlib.sha256(json.dumps(request, sort_keys=True).encode()).hexdigest(),
                    {'execution_unknown'})
                if failure.get('sequence') != sequence:
                    raise ValueError('Engineering failure sequence differs')
                raise ValueError('Engineering execution remains unresolved')
            expected, basis_raw = read(evidence / 'basis.json')
            owner = _OWNER.get()
            current = owner is not None and owner.current(root, evidence, raw)
            relative = evidence.relative_to(root).as_posix()
            if not current:
                row = consumed[relative]
                seen.add(relative)
                tag = row['engineering_delivery']
                outcome = {k: v for k, v in row.items() if k != 'engineering_delivery'}
                task = row['task']
                if (type(row['sequence']) is not int or row['sequence'] != sequence
                        or row['stage'] != 'engineering' or not isinstance(task, dict)
                        or task.get('purpose', 'performance') != request['request_id'].rsplit('-', 1)[-1]
                        or tag['request_sha256'] != _sha(raw) or tag['basis_sha256'] != _sha(basis_raw)
                        or tag['outcome_sha256'] != eng.digest(outcome)):
                    raise ValueError('Engineering consumed request differs')
                try:
                    imported, imported_raw = read(evidence / 'imported.json')
                except FileNotFoundError:
                    if tag['import_sha256'] is not None:
                        raise ValueError('Engineering import evidence missing')
                else:
                    if tag['import_sha256'] != _sha(imported_raw) or imported.get('basis') != expected:
                        raise ValueError('Engineering import evidence differs')
            admissions = home.parent / 'engineering-admissions'
            try:
                admitted = entries(admissions)
            except FileNotFoundError:
                if current:
                    continue
                raise ValueError('Engineering admission is missing')
            stems = set()
            for leaf in admitted:
                match = re.fullmatch(r'([0-9a-f]{32})(\.json|\.execution|-launcher\.sh)', leaf)
                if not match:
                    raise ValueError('Incomplete engineering admission publication')
                stems.add(match[1])
            payload = False
            for stem in sorted(stems):
                binding_path = admissions / (stem + '.json')
                binding, binding_raw = read(binding_path)
                identity = hashlib.sha256(binding_raw).hexdigest()
                bound = binding['request']
                if (not isinstance(bound, dict) or binding.get('schema') != 'engineering-launch-v1'
                        or binding.get('home') != str(home) or binding.get('work') != request['work_dir']):
                    raise ValueError('Engineering admission context differs')
                if bound.get('purpose') == 'provider_preflight':
                    if bound.get('sequence') is not None or not isinstance(bound.get('request'), list):
                        raise ValueError('Engineering preflight identity differs')
                elif (bound.get('request') != request or bound.get('sequence') != sequence
                        or bound.get('purpose') != request['request_id'].rsplit('-', 1)[-1]
                        or binding['files'][str(evidence / 'request.json')]['sha256'] != hashlib.sha256(raw).hexdigest()):
                    raise ValueError('Engineering admission request differs')
                payload = payload or bound.get('purpose') != 'provider_preflight'
                directory = admissions / (stem + '.execution')
                recorded = entries(directory)
                receipt(directory / 'ready.json', identity, {'recording_ready'})
                terminal = receipt(directory / 'launch.json', identity,
                    {'launcher_exited', 'spawn_refused', 'guard_refused'})
                if terminal['code'] != 'launcher_exited' or terminal['exit_code'] != 0:
                    raise ValueError('Engineering execution remains unresolved')
                if 'claimed' not in recorded or entries(directory / 'claimed'):
                    raise ValueError('Engineering execution claim differs')
                receipt(directory / 'started.json', identity, {'spawn_pending'})
            if not current and not payload:
                raise ValueError('Engineering payload evidence is missing')
        if set(consumed) != seen:
            raise ValueError('Engineering history names an absent request')
    except (OSError, ValueError, KeyError, TypeError, RecursionError):
        raise EngineeringExecutionFailure(evidence) from None


def writable(purpose: str, request: dict | None = None) -> tuple[str, ...]:
    if request is not None:
        return tuple(request['engineering']['output_paths'])
    return {"source_reading": ("reading.json",), "plan": ("plan.json",),
            "implementation": ("source", "plan.json"), "plan_review": (), "code_review": (),
            "performance": ("benchmarks/harness", "benchmarks/report.md"), "retry_review": ()}[purpose]


def _matches(name: str, paths) -> bool:
    return any(name == path or name.startswith(path + "/") for path in paths)


def basis(root: Path, state: dict) -> dict:
    return {"state": eng.digest(state), "input": eng.input_identity(root, state),
            "source": performance.current_basis(root), "guard": eng.input_guard(root),
            "harness": performance._inventory(root / "benchmarks/harness") if (root / "benchmarks/harness").exists() else None,
            "plan": eng.sha256(root / "engineering/plan.json") if (root / "engineering/plan.json").exists() else None}


def import_result(root: Path, state: dict, task: dict, attempt: Path, delivered: dict, expected: dict, result: dict,
                  *, staging: Path | None = None) -> None:
    """Validate all returned bytes before publication; preserve rollback evidence."""
    from trellis.modeling.driver import check_halt, AgentResponseError
    check_halt(root)
    if (root / ".trellis/modeling/pause").exists():
        raise AgentResponseError("Engineering output was canceled; staged work is preserved")
    if basis(root, state) != expected or read_json(root / ".trellis/modeling/state.json") != state:
        raise AgentResponseError("Engineering output is stale; staged work is preserved")
    view = attempt / "view"
    owner = _OWNER.get()
    if owner is None or owner.attempt != attempt or owner.state is not state:
        raise RuntimeError("Engineering delivery boundary invalid")
    if staging is not None and (owner.native is None or owner.native.get('turn_directory') != staging):
        raise RuntimeError('Engineering import requires its owned native turn')
    store = staging or attempt
    purpose = task.get("purpose", "performance")
    allowed = writable(purpose, owner.native['request'] if owner.native is not None else None)
    inventory = owner.inventory()
    # Logs and result envelopes remain in this attempt; they are never imports.
    changes = {name for name in set(inventory) | set(delivered) if inventory.get(name) != delivered.get(name)}
    if any(not _matches(name, (*allowed, *(('result.json',) if owner.native is None else ()))) for name in changes):
        raise AgentResponseError("Engineering output changed a supplied input or an unassigned file")
    staged = {}

    def stage(name):
        from trellis.agents.engineering_exec import open_regular
        if name not in inventory:
            raise AgentResponseError("Required engineering output is missing: " + name)
        if name not in staged:
            with _boundary():
                owner.check_roots()
                with open_regular(view / name) as stream:
                    before = os.fstat(stream.fileno())
                    raw = stream.read()
                    after = os.fstat(stream.fileno())
                if ((before.st_size, before.st_mtime_ns, before.st_ctime_ns)
                        != (after.st_size, after.st_mtime_ns, after.st_ctime_ns)
                        or before.st_mode != after.st_mode or _sha(raw) != inventory[name]
                        or owner.native is not None and before.st_mode & 0o777 != owner.native['sealed_modes'][name]):
                    raise ValueError("Returned bytes changed")
                owner.check_roots()
                saved = store / "import" / name
                history._put(saved, raw)
                saved.chmod(before.st_mode & 0o777)
                owner.check_roots()
                staged[name] = saved
        return staged[name]

    def read_plan(name):
        saved = stage(name)
        try:
            return eng.read_plan(saved)
        except (ValueError, TypeError):
            raise AgentResponseError("Returned engineering plan is malformed") from None

    mappings = []
    if purpose in {"source_reading", "plan"}:
        name = "reading.json" if purpose == "source_reading" else "plan.json"
        read_plan(name)
        mappings.append((name, "engineering/" + name))
    elif purpose == "implementation":
        proposal = read_plan("plan.json")
        if proposal != state["engineering"]["plan"]:
            state["engineering"]["proposal"] = proposal
            raise AgentResponseError("Proposed departures require independent plan review before adoption")
        if "source/Cargo.toml" not in inventory or "source/Cargo.lock" not in inventory:
            raise AgentResponseError("Implementation output requires Cargo.toml and Cargo.lock")
        mappings.append(("source", "work/crate"))
    elif purpose == "performance":
        mappings.append(("benchmarks/harness", "benchmarks/harness"))
        if result.get("action") == "accept":
            if result.get("report") != "benchmarks/report.md":
                raise AgentResponseError("Acceptance must name benchmarks/report.md")
            eng._text(history._read(stage(result["report"])).decode(), "Performance report")
            mappings.append(("benchmarks/report.md", "benchmarks/report.md"))
    planned = []
    for source, target in mappings:
        names = [name for name in inventory if _matches(name, (source,))]
        if not names:
            raise AgentResponseError("Required engineering output is empty: " + source)
        for name in names:
            relative = Path(name)
            if (set(relative.parts) & {".git", ".codex", ".agents", ".trellis"}
                    or source == "source" and set(relative.parts) & {"target", "__pycache__"}
                    or relative.suffix in {".lean", ".llbc", ".olean"}):
                raise AgentResponseError("Returned file is outside the engineering output contract")
            stage(name)
        planned.append((store / "import" / source, root / target))
    if owner.inventory() != inventory:
        raise RuntimeError("Engineering delivery boundary invalid")
    check_halt(root)
    if ((root / ".trellis/modeling/pause").exists() or basis(root, state) != expected
            or read_json(root / ".trellis/modeling/state.json") != state):
        raise AgentResponseError("Engineering output was canceled or became stale before publication")
    replaced = []
    try:
        for index, (source, target) in enumerate(planned):
            owner.check_roots()
            backup = store / "previous" / str(index)
            backup.parent.mkdir(exist_ok=True)
            performance._directory(target.parent)
            existed = target.exists()
            if target.is_symlink():
                raise ValueError("Engineering import target cannot be an alias")
            if existed:
                os.replace(target, backup)
            replaced.append((target, backup, existed))
            os.replace(source, target)
            owner.check_roots()
        owner.check_roots()
        write_json(store / "imported.json", {"basis": expected, "files": inventory, "mappings": mappings})
    except BaseException:
        for target, backup, existed in reversed(replaced):
            if target.exists():
                preserved = store / ("interrupted-" + uuid.uuid4().hex)
                os.replace(target, preserved)
            if existed:
                os.replace(backup, target)
        raise


def prepare_delivery(root: Path, state: dict, task: dict, *, native_request=None):
    from trellis.modeling.driver import (load_config, preparation_provider, _single_request_common,
        runtime_root, execute_agent_request, check_halt, AgentResponseError)
    from trellis.agent_wrapper.protocol import AgentLane, ArtifactSpec
    from trellis.agent_wrapper.executor import prepare_artifact_paths
    owner = _dispatch_owner(root, state, task)
    check_halt(root)
    owner.dispatched = True
    expected = basis(root, state)
    attempt = root / eng.STORE / "deliveries" / (str(state["sequence"]) + "-" + uuid.uuid4().hex)
    attempt.mkdir(parents=True, exist_ok=False)
    owner.reserved = attempt
    view = attempt / "view"
    delivery = eng.delivery(root, state, task, view)
    owner.bind_view(view, delivery["files"])
    purpose = task.get("purpose", "performance")
    allowed = writable(purpose, native_request)
    for name in allowed:
        path = view / name
        if not path.exists():
            if name in {"source", "benchmarks/harness"}:
                path.mkdir(parents=True)
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
    config = load_config(root / "trellis.config.json")
    config = replace(config, repo_path=view, sandbox=replace(config.sandbox, extra_writable_paths=allowed))
    inputs = read_json(root / "config.json")
    provider = preparation_provider(config, inputs, task["role"])
    if native_request is not None:
        profile = native_request['engineering']['profile']
        if (provider.provider, provider.model, provider.effort) != (profile['provider'], profile['model'], profile['reasoning_effort']):
            raise RuntimeError('Engineering provider differs from its native profile')
    if not config.sandbox.enabled or config.sandbox.backend != "bwrap" or provider.provider != "codex" or provider.extra_args:
        raise ValueError("Engineering delivery requires bwrap and Codex without context overrides")
    artifact_name = (f"trellis_engineering_{native_request['id']}_submission-1.json" if native_request else 'result.json')
    request = _single_request_common(config=config, runtime_root=runtime_root(root), home_runtime=attempt,
        request=native_request or {"kind": "engineering", "id": state["sequence"], "cycle": state["sequence"], "fresh_context": True},
        provider=provider, lane=AgentLane(kind="engineering"), kind_label=purpose, burst_role="engineering",
        prompt="", artifact=ArtifactSpec(canonical_name=artifact_name, kind=delivery["artifact_kind"], phase="engineering"))
    request = replace(request, state_dir=view / ".trellis", log_dir=view / ".trellis/logs")
    artifacts = prepare_artifact_paths(request.state_dir, view, artifact_name)
    prompt = (view / "prompt.md").read_text().replace("Write result.json.", '' if native_request else
        f"Write {artifacts.raw.relative_to(view)} and create {artifacts.done.relative_to(view)} when finished.")
    if native_request is not None:
        prompt = prompt.replace('accept|revise|review', 'accept|revise|reject').replace('Use review to', 'Use reject to')
    (view / "prompt.md").write_text(prompt)
    request = replace(request, prompt=prompt)
    delivered = owner.inventory()
    write_json(attempt / "request.json", request.to_dict())
    write_json(attempt / "basis.json", expected)
    owner.register(attempt, request)
    return request, artifacts, delivered, expected


def dispatch(root: Path, state: dict, task: dict) -> dict:
    from trellis.modeling.driver import execute_agent_request, check_halt, AgentResponseError
    request, artifacts, delivered, expected = prepare_delivery(root, state, task)
    attempt = request.work_dir.parent
    inputs = read_json(root / 'config.json')
    previous = os.environ.get("TRELLIS_ENGINEERING_CARGO")
    os.environ["TRELLIS_ENGINEERING_CARGO"] = inputs["tools"]["cargo"]
    try:
        from trellis.agent_wrapper.protocol import EngineeringExecutionFailure
        from trellis.agent_wrapper.executor import engineering_execution_failure
        try:
            response = execute_agent_request(request)
        except EngineeringExecutionFailure:
            raise
        except Exception:
            raise engineering_execution_failure(request) from None
    finally:
        if previous is None:
            os.environ.pop("TRELLIS_ENGINEERING_CARGO", None)
        else:
            os.environ["TRELLIS_ENGINEERING_CARGO"] = previous
    check_halt(root)
    if not response.ok:
        raise AgentResponseError("Engineering response failed validation; staged work is preserved")
    if not artifacts.done.is_file():
        raise AgentResponseError("Engineering output is incomplete; staged work is preserved")
    history._read(artifacts.done)
    import_result(root, state, task, attempt, delivered, expected, response.payload)
    return response.payload


def native_owner(request):
    owner = _OWNER.get()
    if (owner is None or owner.native is None or owner.cleanup_unknown or owner.thread != threading.get_ident()
            or owner.request is None or owner.native['request'] != request
            or owner.native['state']['state']['in_flight_request'] != request):
        raise RuntimeError('Native engineering requires its live original delivery owner')
    owner.check_roots()
    return owner


def native_process(request, execution):
    from trellis.dispatch.process import AttemptProcess
    owner = native_owner(request)
    if execution not in owner.executions:
        owner.executions[execution] = AttemptProcess(execution)
    return owner.executions[execution]


@contextmanager
def native_start(request):
    owner = native_owner(request)
    owner.starting = True
    try:
        yield
    finally:
        owner.starting = False
    if owner.cancelled:
        raise KeyboardInterrupt('Engineering request cancelled')


def native_turn(request, runtime):
    from trellis.runtime.kernel_cli import run_kernel_cli
    owner = native_owner(request)
    turn = run_kernel_cli(dict(action='engineering_ready_turn', state=owner.native['state']['state']))['turn']
    return dict(turn, native_runtime=str(runtime), engineering_delivery=str(owner.attempt),
                engineering_cargo=read_json(owner.root / 'config.json')['tools']['cargo'],
                feedback=owner.native['state']['state']['engineering']['feedback'] if turn['feedback_sha256'] else None)


def _native_bytes(path, *, optional=False):
    result = _native_file(path, optional=optional)
    return None if result is None else result[0]


def _native_file(path, *, optional=False):
    from trellis.agents.engineering_exec import open_regular
    try:
        with _boundary(), open_regular(path) as stream:
            before = os.fstat(stream.fileno())
            raw = stream.read(64 * 1024 * 1024 + 1)
            after = os.fstat(stream.fileno())
            if (len(raw) > 64 * 1024 * 1024 or
                    (before.st_size, before.st_mtime_ns, before.st_ctime_ns, before.st_mode)
                    != (after.st_size, after.st_mtime_ns, after.st_ctime_ns, after.st_mode)):
                raise ValueError('Engineering returned input changed or exceeds its read bound')
            return raw, before.st_mode & 0o777
    except RuntimeError as error:
        if optional and isinstance(error.__context__, FileNotFoundError):
            return None
        raise


def native_seal(request, attempt, turn, response):
    from trellis.agent_wrapper.executor import prepare_artifact_paths
    owner = native_owner(request)
    inventory = owner.inventory()
    files, modes = {}, {}
    for name, expected in inventory.items():
        path = owner.request.work_dir / name
        raw, mode = _native_file(path)
        if _sha(raw) != expected:
            raise RuntimeError('Engineering files changed while sealing')
        files['view/' + name] = raw
        modes[name] = mode
    name = f"trellis_engineering_{request['id']}_submission-{attempt['turn']}.json"
    paths = prepare_artifact_paths(owner.request.state_dir, owner.request.work_dir, name)
    for name, path in (('raw.json', paths.raw), ('done', paths.done)):
        raw = _native_bytes(path, optional=True)
        if raw is not None:
            files[name] = raw
    if owner.inventory() != inventory:
        raise RuntimeError('Engineering files changed while sealing')
    reference = history._seal(turn / 'candidate', 'engineering-return',
        dict(request=request, turn=attempt['turn'], completion=attempt['phase']['completion'],
             response=response, inventory=inventory, modes=modes), files)
    return reference['id']


def _native_sealed(owner, request, attempt, turn):
    sealed = history._verified(turn / 'candidate')
    if len(sealed) != 1 or sealed[0][0]['id'] != attempt['phase']['candidate_sha256']:
        raise RuntimeError('Native engineering candidate changed before checking')
    _, record = sealed[0]
    metadata = record['metadata']
    files = {entry['path']: history._read(turn / 'candidate/blobs' / entry['sha256']) for entry in record['files']}
    if (metadata['request'] != request or metadata['turn'] != attempt['turn']
            or owner.inventory() != metadata['inventory']):
        raise RuntimeError('Native engineering sealed delivery changed')
    for name in metadata['inventory']:
        raw, mode = _native_file(owner.request.work_dir / name)
        if raw != files['view/' + name] or mode != metadata['modes'][name]:
            raise RuntimeError('Native engineering sealed file or mode changed')
    from trellis.agent_wrapper.executor import prepare_artifact_paths
    paths = prepare_artifact_paths(owner.request.state_dir, owner.request.work_dir,
        f"trellis_engineering_{request['id']}_submission-{attempt['turn']}.json")
    if any(_native_bytes(path, optional=True) != files.get(name)
           for name, path in (('raw.json', paths.raw), ('done', paths.done))):
        raise RuntimeError('Native engineering sealed artifact changed')
    return metadata, files


def native_check(request, attempt, turn):
    from trellis.modeling.driver import AgentResponseError
    from trellis.runtime.kernel_cli import run_kernel_cli
    owner = native_owner(request)
    owner.native['turn_directory'] = turn
    metadata, files = _native_sealed(owner, request, attempt, turn)
    owner.native['sealed_modes'] = metadata['modes']
    purpose = request['engineering']['purpose']
    checked = dict(request_id=request['id'], cycle=request['cycle'], request_sha256=eng.digest(request),
        turn=attempt['turn'], candidate_sha256=attempt['phase']['candidate_sha256'],
        basis_sha256=request['engineering']['basis_sha256'])
    host = dict(request=request, attempt=owner.attempt.relative_to(owner.root).as_posix(),
                sealed_candidate=checked['candidate_sha256'], turn=attempt['turn'],
                request_sha256=owner.request_sha, basis_sha256=owner.basis_sha,
                turn_path=str(turn))
    malformed = lambda path, rule: dict(outcome='malformed', diagnostics=[dict(path=path, rule=rule)])
    record_name = request['engineering']['artifact_contract']['record_path']
    try:
        raw = files.get('raw.json', b'').decode('utf-8')
        record_text = files.get('view/' + record_name, b'').decode('utf-8') if record_name else None
    except UnicodeError:
        validation = dict(ok=False, diagnostics=[dict(path='artifact', rule='type')])
    else:
        validation = run_kernel_cli(dict(action='check_engineering_artifact_text', purpose=purpose,
            raw_text=raw, record_text=record_text))['output']
    changes = {name for name in set(metadata['inventory']) | set(owner.native['delivered'])
               if metadata['inventory'].get(name) != owner.native['delivered'].get(name)}
    if any(not _matches(name, writable(purpose, request)) for name in changes):
        outcome = malformed('returned_files', 'stale')
    elif not validation['ok']:
        outcome = dict(outcome='malformed', diagnostics=validation['diagnostics'])
    elif 'done' not in files:
        outcome = malformed('artifact', 'required')
    else:
        data = validation['data']
        proposal = json.loads(record_text) if record_text else None
        try:
            import_result(owner.root, owner.state, owner.task, owner.attempt, owner.native['delivered'],
                          owner.native['basis'], data, staging=turn)
        except AgentResponseError:
            outcome = malformed('returned_files', 'stale')
        else:
            outcome = _native_host_outcome(owner, request, checked, data, proposal, turn)
            owner.native['basis'] = basis(owner.root, owner.state)
    checked.update(outcome=outcome, host_receipt_sha256=eng.native_record(owner.root, host))
    eng.native_record(owner.root, checked)
    return dict(kind='engineering_checked', checked=checked)


def _native_candidate(root, native, record):
    payload, _, _ = performance._basis(root)
    files = payload['implementation_files']
    if 'Cargo.lock' not in files:
        raise ValueError('Implementation requires its dependency lock')
    tools = eng.digest(payload['tools'])
    if tools != native['subject']['tools_sha256']:
        raise RuntimeError('Engineering tools changed during source checking')
    value = dict(source_sha256=eng.digest(dict(files=files, modes=payload['implementation_modes'])),
        build_sha256=eng.digest({key: payload[key] for key in ('configuration_sha256', 'work_configuration')}),
        lock_sha256=files['Cargo.lock'], tools_sha256=tools, plan_sha256=native['plan_sha256'])
    candidate_basis = eng.digest([value[key] for key in ('source_sha256', 'build_sha256', 'lock_sha256', 'tools_sha256', 'plan_sha256')])
    receipt = eng.native_record(root, record)
    value['checks'] = [dict(argv=command['argv'], exit_code=command['exit_code'], basis_sha256=candidate_basis,
                           receipt_sha256=receipt) for command in record['commands']]
    return value


def _native_host_outcome(owner, request, checked, data, proposal, turn):
    root, state = owner.root, owner.state
    native = owner.native['state']['state']['engineering']
    purpose = request['engineering']['purpose']
    if purpose in {'source_reading', 'plan'}:
        eng.native_record(root, proposal)
        return dict(outcome='record', summary=data['summary'], record=proposal)
    if purpose in {'plan_review', 'code_review', 'retry_review'}:
        if purpose == 'code_review':
            record = eng.native_read(root, native['candidate']['checks'][0]['receipt_sha256'])
            eng.validate_check(root, record['id'], record['record_sha256'])
            if _native_candidate(root, native, record) != native['candidate']:
                raise RuntimeError('Code review candidate changed')
        return dict(outcome='review', decision='reject' if data['decision'] == 'review' else data['decision'], reason=data['reason'])
    operation_identity = dict(request_sha256=checked['request_sha256'], candidate_sha256=checked['candidate_sha256'], turn=checked['turn'])
    if purpose == 'implementation':
        record = eng.check(root, copy.deepcopy(state), operation=turn / 'cargo-operation.json', identity=operation_identity)
        if not eng._check_passed(record):
            return dict(outcome='malformed', diagnostics=[dict(path='source', rule='failed_check')])
        eng.validate_check(root, record['id'], record['record_sha256'])
        value = _native_candidate(root, native, record)
        return dict(outcome='implementation', summary=data['summary'], candidate=value)
    if purpose != 'performance':
        raise RuntimeError('Unsupported native engineering purpose')
    eng.current_approval(root, state)
    if data['action'] == 'measure':
        try:
            performance._commands(data)
        except ValueError:
            return dict(outcome='malformed', diagnostics=[dict(path='commands', rule='type')])
        record = performance.run_measurement(root, state, data, operation=turn / 'measurement-operation.json', identity=operation_identity)
        retained_state = copy.deepcopy(state)
        eng.retain(root, retained_state, record)
        receipt = eng.native_record(root, dict(run_id=record['id'], record_sha256=record['record_sha256'],
            retained_candidate=retained_state['engineering']['candidates'][record['id']]))
        return dict(outcome='measure', reason=data['reason'], measurement=dict(candidate_sha256=eng.digest(native['candidate']),
            reviewed_candidate_sha256=native['current_reviewed_candidate_sha256'], harness_sha256=record['harness_sha256'],
            record_sha256=receipt, exit_codes=[c['exit_code'] if not c.get('timeout') and not c.get('output_limit_exceeded')
                and not c.get('execution_error') else -1 for c in record['commands']]))
    if data['action'] in {'improve', 'fail'}:
        return dict(outcome=data['action'], **{key: value for key, value in data.items() if key != 'action'})
    return _native_selection(owner, native, data)


def _native_selection(owner, native, data):
    from trellis.modeling import performance_loop
    refs = {eng.native_read(owner.root, ref)['run_id']: ref for ref in native['measurements']}
    selected = [refs[name] for name in data['evidence']]
    if not selected:
        raise ValueError('Selection requires a measured candidate')
    owner.directories.enter_context(eng.select(owner.root, owner.state, data['evidence'],
        settle=lambda: 'committed' if owner.native.get('consumed_committed') else _native_reconcile(owner)))
    state = owner.state
    state['performance'].update(basis=performance.current_basis(owner.root),
                                audit_sha256=performance_loop._audit_hash(state))
    approval = performance_loop.acceptance_record(owner.root, state, data, data['report'])
    write_json(owner.root / performance_loop.ACCEPTED, approval)
    state['performance']['approval'] = approval
    eng.freeze(owner.root, state, transition=False)
    baseline = state['engineering']['baseline']
    receipt = eng.native_record(owner.root, baseline)
    return dict(outcome='accept', reason=data['reason'], baseline=dict(
        reviewed_candidate_sha256=native['measurements'][selected[0]]['reviewed_candidate_sha256'],
        harness_sha256=approval['harness_sha256'], assessment_sha256=approval['report_sha256'],
        measurement_sha256=selected, selection_receipt_sha256=receipt))


def _native_reconcile(owner):
    """Only a recovered native checkpoint can settle a terminal selection."""
    native = owner.native
    if not native.get('terminal_started'):
        return 'uncommitted'
    if not native.get('quiescent'):
        return 'uncertain'
    current = eng.native_load(owner.root)
    consumed = current['state']['engineering']['last_consumption']
    if consumed == native['consumption']:
        native['consumed_committed'] = True
        native['state'] = current
        projected = eng.native_project(owner.root, owner.state, current)
        owner.state.clear()
        owner.state.update(projected)
        owner.committed = copy.deepcopy(projected)
        return 'committed'
    if current == native['predecessor']:
        return 'uncommitted'
    return 'uncertain'


def native_execution_guard(root):
    """An orphan assignment must close before native journal recovery."""
    from trellis.dispatch.process import alive, _engineering_recipe
    from trellis.modeling.driver import runtime_root
    owner = _OWNER.get()
    if owner is not None and owner.root == root:
        if owner.cleanup_unknown:
            raise RuntimeError('Engineering assignment closure is unknown')
        return
    executions = (runtime_root(root) / 'worker-attempts').glob('*/turns/*/execution')
    for number, execution in enumerate(executions):
        if number >= 4096:
            raise RuntimeError('Engineering execution scan exceeded its bound')
        recipe = execution / 'recipe.json'
        try:
            recipe.lstat()
        except FileNotFoundError:
            continue
        if not _engineering_recipe(_read(recipe)[0]):
            continue
        try:
            record = _read(execution / 'process.json')[0]
            if alive(record) or _read(execution / 'cleanup.json')[0] != record:
                raise ValueError('Assignment remains live')
        except (OSError, ValueError, TypeError):
            raise RuntimeError('Engineering assignment closure is unknown') from None


def _native_unresolved(root):
    from trellis.agent_wrapper.protocol import EngineeringExecutionFailure
    from trellis.agents.host_files import safe_directory
    evidence = root / eng.STORE / 'deliveries'
    try:
        current = eng.native_load(root)
        owner = _OWNER.get()
        live = owner is not None and owner.root == root and owner.native is not None
        if current['state']['in_flight_request'] is not None and not live:
            raise ValueError('Native assignment lost its original owner')
        projection = eng.native_project(root, read_json(root / '.trellis/modeling/state.json'), current)
        consumed = {row['engineering_attempt']: row for row in projection['history']}
        seen = set()
        try:
            with safe_directory(evidence, create=False) as (_, fd), os.scandir(fd) as entries:
                names = []
                for entry in entries:
                    names.append(entry.name)
                    if len(names) > 4096:
                        raise ValueError('Native delivery scan exceeded its bound')
        except FileNotFoundError:
            names = []
        for name in names:
            if not re.fullmatch(r'[1-9][0-9]*-[0-9a-f]{32}', name):
                raise ValueError('Native delivery identity differs')
            attempt = evidence / name
            if live and owner.reserved == attempt:
                continue
            relative = attempt.relative_to(root).as_posix()
            row = consumed[relative]
            checked = eng.native_read(root, row['native_consumption']['checked_sha256'])
            host = eng.native_read(root, checked['host_receipt_sha256'])
            request, raw = _read(attempt / 'request.json')
            _, basis_raw = _read(attempt / 'basis.json')
            if (_sha(raw) != host['request_sha256'] or _sha(basis_raw) != host['basis_sha256']
                    or request['cycle'] != checked['request_id']):
                raise ValueError('Native consumed delivery changed')
            _native_consumed_turns(root, attempt, host, checked)
            # First failure evidence remains a hold even after a zero child exit.
            with safe_directory(attempt, create=False) as (_, fd):
                try:
                    os.stat('execution-failure.json', dir_fd=fd, follow_symlinks=False)
                except FileNotFoundError:
                    pass
                else:
                    raise ValueError('Native execution remains unresolved')
            seen.add(relative)
        if set(consumed) != seen:
            raise ValueError('Native consumption names an absent delivery')
    except (OSError, ValueError, KeyError, TypeError, RuntimeError):
        raise EngineeringExecutionFailure(evidence) from None


def _native_consumed_turns(root, delivery, host, checked):
    from trellis.agents import engineering_context
    from trellis.agents.engineering_exec import file_identity
    from trellis.agent_wrapper.protocol import SingleAgentRequest
    from trellis.agent_wrapper import provider_clock
    from trellis.dispatch import candidate
    from trellis.dispatch.process import alive
    from trellis.modeling.driver import runtime_root
    terminal_turn = Path(host['turn_path'])
    directory = terminal_turn.parent.parent
    saved = _read(terminal_turn / 'execution/engineering-request.json')[0]
    if (directory.parent != runtime_root(root) / 'worker-attempts'
            or not re.fullmatch(str(checked['request_id']) + r'-[0-9a-f]{16}', directory.name)
            or terminal_turn != directory / 'turns' / str(checked['turn'])
            or host['request'] != saved['submission']['engineering_request']
            or saved['request']['work_dir'] != str(delivery / 'view')
            or saved['request']['burst_home'] != _read(delivery / 'request.json')[0]['burst_home']
            or any(file_identity(Path(name)) != identity for name, identity in saved['checker_files'].items())):
        raise ValueError('Native consumed turn binding differs')
    session = _read(directory / 'runtime/worker-session.json')[0]
    for ordinal in range(1, checked['turn'] + 1):
        turn = directory / 'turns' / str(ordinal)
        execution = turn / 'execution'
        record = _read(execution / 'process.json')[0]
        if alive(record) or _read(execution / 'cleanup.json')[0] != record:
            raise ValueError('Native consumed launcher has not closed')
        response, response_raw = _read(execution / 'response.json')
        result = _read(execution / 'result.json')[0]
        if result != dict(returncode=0, response_sha256=_sha(response_raw)):
            raise ValueError('Native consumed launcher result differs')
        seals = history._verified(turn / 'candidate')
        if len(seals) != 1:
            raise ValueError('Native consumed candidate is missing')
        reference, seal = seals[0]
        metadata = seal['metadata']
        completion = metadata['completion']
        if (metadata['request'] != host['request'] or metadata['turn'] != ordinal
                or metadata['response'] != response or response != _read(turn / 'delivery.json')[0]
                or completion['process'] != record or completion['session'] != session
                or completion['turn'] != ordinal or completion['response_sha256'] != candidate.digest(candidate.canonical_json(response))
                or ordinal == checked['turn'] and reference['id'] != checked['candidate_sha256']):
            raise ValueError('Native consumed sealed completion differs')
        retained = _read(directory / f'runtime/engineering-session-{ordinal}.json')[0]
        if (retained['request_sha256'] != checked['request_sha256'] or retained['turn'] != ordinal
                or retained['session'] != session or retained['execution_record'] != str(execution / 'clock')
                or any(file_identity(Path(name)) != identity for name, identity in retained['files'].items())):
            raise ValueError('Native consumed session evidence differs')
        finished, exited = provider_clock.read(execution / 'clock', 'finished'), provider_clock.read(execution / 'clock', 'exited')
        if (not finished or not exited or finished['disposition'] != 'completed'
                or exited['returncode'] != 0 or finished != response['provider_execution']
                or finished['thread_id'] != session['thread_id']):
            raise ValueError('Native consumed provider clock differs')
    single = SingleAgentRequest.from_dict(saved['request'])
    # Reuse the existing exact retained-session validator without granting a turn.
    engineering_context.retained_session(single, dict(turn=checked['turn'] + 1, session=session,
        engineering_request=host['request'], engineering_attempt=dict(contract_sha256=checked['request_sha256'])))


def native_event(owner, event, *, guard=None):
    from trellis.modeling.driver import REPO, runtime_root
    from trellis.runtime.kernel_cli import run_kernel_cli
    payload = dict(action='engineering_runtime_event', root=str(runtime_root(owner.root)),
        source_root=str(REPO), expected_state_sha256=owner.native['state']['state_sha256'], event=event)
    if guard is not None:
        payload['guard'] = guard
    current = run_kernel_cli(payload, **(dict(owned=True, pass_fds=tuple(p['fd'] for p in guard['roots']),
                                             timeout_seconds=120) if guard else {}))
    owner.native['state'] = current
    return current


def native_consume(owner, event, *, terminal_scope=nullcontext):
    from trellis.acceptance_identity import record_deployment
    from trellis.modeling.driver import REPO
    from trellis.runtime.kernel_cli import kernel_cli_command
    checked = event['checked']
    current = eng.native_load(owner.root)
    if current != owner.native['state']:
        raise RuntimeError('Native engineering predecessor or original inputs changed')
    _native_sealed(owner, owner.native['request'], current['state']['worker_attempt']['attempt'],
                   owner.native['turn_directory'])
    outcome = checked['outcome']
    purpose = owner.native['request']['engineering']['purpose']
    if outcome['outcome'] == 'record':
        path = 'reading.json' if purpose == 'source_reading' else 'plan.json'
        if eng.read_plan(owner.root / 'engineering' / path) != outcome['record']:
            raise RuntimeError('Native engineering imported record changed')
    candidate = (outcome['candidate'] if outcome['outcome'] == 'implementation' else
        current['state']['engineering']['reviewed'][outcome['baseline']['reviewed_candidate_sha256']]['candidate']
        if outcome['outcome'] == 'accept' else current['state']['engineering']['candidate']
        if purpose in {'code_review', 'performance'} else None)
    if candidate is not None:
        record = eng.native_read(owner.root, candidate['checks'][0]['receipt_sha256'])
        eng.validate_check(owner.root, record['id'], record['record_sha256'])
        context = dict(current['state']['engineering'], plan_sha256=candidate['plan_sha256'])
        if _native_candidate(owner.root, context, record) != candidate:
            raise RuntimeError('Native engineering checked source changed before consumption')
    command = kernel_cli_command()
    if len(command) != 1 or not Path(command[0]).is_absolute():
        raise RuntimeError('Native consumption requires a pinned kernel executable')
    binding = record_deployment(REPO, Path(command[0]), owner.attempt / 'checkpoint-build.json')
    history._put(owner.attempt / 'checkpoint-build-bound.json', eng.canonical_json(binding))
    owner.check_roots()
    roots = [dict(path=str(path), fd=fd, device=device, inode=inode)
             for path, (fd, device, inode) in owner.roots.items()]
    guard = {key: checked[key] for key in ('request_id', 'turn', 'request_sha256', 'candidate_sha256')}
    guard.update(checked_sha256=eng.digest(checked), roots=roots)
    previous = owner.native['state']['state']['engineering']['last_consumption']
    consumed = {key: guard[key] for key in ('request_id', 'turn', 'request_sha256', 'candidate_sha256', 'checked_sha256')}
    consumed['predecessor_sha256'] = eng.digest(previous) if previous is not None else None
    eng.native_record(owner.root, consumed)
    eng.native_stage_evidence(owner.root)
    owner.native.update(predecessor=owner.native['state'], consumption=consumed, quiescent=False, terminal_started=True)
    invoked = False
    try:
        with terminal_scope():
            invoked = True
            native_event(owner, event, guard=guard)
    except BaseException as error:
        owner.native['quiescent'] = not invoked or getattr(error, 'kernel_quiescent', False)
        try:
            _native_reconcile(owner)
        except BaseException:
            pass
        raise
    else:
        owner.native['quiescent'] = True
    if _native_reconcile(owner) != 'committed':
        raise RuntimeError('Native engineering consumption is unresolved')
    write_json(owner.root / '.trellis/modeling/state.json', owner.state)

#!/usr/bin/env python3
"""Host-side bridge for the pre-bootstrap source-adaptation protocol.

The Rust kernel owns state transitions.  This module owns only transport:
freezing candidate trees, invoking the pinned checked executor, transactional
agent bursts, and moving canonical artifacts between agent-writeable and
authority-only directories.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import threading
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Callable, Iterator

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.pv_goal_binding import build_goal_binding_report
from scripts.pv_prose_goal import parse_goal
from scripts.pv_source_tree import (
    canonical_json,
    git_checkout_digest,
    readonly_closure_digest,
    source_tree_manifest,
    tagged_hash,
    validate_source_partition_manifest,
)
from trellis.adapters import ProviderConfig
from trellis.agent_wrapper.executor import execute_agent_request, prepare_artifact_paths
from trellis.burst_home import seed_burst_home
from trellis.agent_wrapper.protocol import AgentLane, ArtifactSpec, SingleAgentRequest
from trellis.checking import write_phase0_candidate_script
from trellis.config import SandboxConfig


BROKER_CALL_SCHEMA = "trellis-phase0-broker-call/v1"
BROKER_RESULT_SCHEMA = "trellis-phase0-broker-result/v1"
WORKER_DRAFT_SCHEMA = "trellis-phase0-worker-draft/v1"
# Mirrors `prompt_contract_version()` in kernel/src/request_contracts.rs; the
# kernel refuses a genesis that pins any other value, so keep the two in step
# (tests/test_pv_phase0.py checks this against the kernel source).
PHASE0_PROMPT_CONTRACT_VERSION = 64
WORKER_RESPONSE_SCHEMA = "trellis-phase0-worker-response/v1"
AUDIT_RESULT_SCHEMA = "trellis-phase0-audit-result/v1"
AUDIT_DRAFT_SCHEMA = "trellis-phase0-audit-draft/v1"
ZERO = "0" * 64
OPAM_SWITCH_ENVIRONMENT_KEYS = {
    "OPAM_SWITCH_PREFIX",
    "OPAMSWITCH",
    "CAML_LD_LIBRARY_PATH",
    "OCAML_TOPLEVEL_PATH",
    "OCAMLTOP_INCLUDE_PATH",
}
OPAM_SWITCH_PATH_KEYS = OPAM_SWITCH_ENVIRONMENT_KEYS - {"OPAMSWITCH"}
# Every burst of a lane reuses one set of log names, so the next burst of the
# same lane truncates the transcript of the one before it. Each finished burst
# is copied under `agent-output/logs/history/` and this many sets are kept.
PHASE0_LOG_HISTORY_SETS = 200
# A worker's interactive self-checks leave only receipt digests behind, so
# nobody can later tell whether a faithful restructuring was available and
# rejected. Each interactive candidate tree is copied next to the retained
# burst logs and this many are kept, oldest evicted. Like those logs this is
# an auditor's record inside the agent's own attempt, never authority.
PHASE0_INTERACTIVE_CANDIDATE_SETS = 50


class Phase0BridgeError(RuntimeError):
    pass


class Phase0EnvironmentalFailure(Phase0BridgeError):
    """Recorded checker execution that is non-semantic and separately retried."""


class Phase0DraftRejected(Phase0BridgeError):
    """An agent delivery the bridge or kernel refused. Worker-attributable:
    the same request is re-dispatched with the reason appended so the agent
    can correct it, within the transport retry bound."""


class Phase0KernelRejection(Phase0BridgeError):
    """The kernel refused a command. Deterministic: re-dispatching the same
    request cannot change the answer, so the driver stops at once instead of
    spending three identical agent bursts on it."""


def _environmental_checker_failure(receipt: dict[str, Any], output: Any) -> bool:
    if (
        receipt.get("timed_out") is True
        or receipt.get("stdout_truncated") is True
        or receipt.get("stderr_truncated") is True
        or receipt.get("exit_code") in {None, 137, 143}
    ):
        return True
    if not isinstance(output, dict):
        return False
    stages = output.get("stages")
    if isinstance(stages, list) and any(
        isinstance(stage, dict)
        and (
            stage.get("timed_out") is True
            or stage.get("stdout_truncated") is True
            or stage.get("stderr_truncated") is True
            or stage.get("exit_code") in {137, 143}
        )
        for stage in stages
    ):
        return True
    reason = str(output.get("reason", "")).lower()
    return any(
        marker in reason
        for marker in ("sandbox unavailable", "resource exhaustion", "out of memory")
    )


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise Phase0BridgeError(f"duplicate JSON member: {key}")
        value[key] = item
    return value


def _read_json(path: Path) -> dict[str, Any]:
    try:
        raw = path.read_bytes()
        value = json.loads(raw, object_pairs_hook=_strict_object)
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise Phase0BridgeError(f"cannot read JSON {path}: {exc}") from exc
    if not isinstance(value, dict) or canonical_json(value) != raw:
        raise Phase0BridgeError(f"{path} is not a canonical JSON object")
    return value


def _read_agent_json(path: Path) -> dict[str, Any]:
    """Read an agent delivery: one strict JSON object (no duplicate members),
    in any formatting. The bridge canonicalizes and seals what it reads, so
    the agent is not held to byte-canonical output the contract never asked
    for."""
    try:
        value = json.loads(path.read_bytes(), object_pairs_hook=_strict_object)
    except (OSError, UnicodeError, json.JSONDecodeError, Phase0BridgeError) as exc:
        raise Phase0DraftRejected(f"delivery {path.name} is not valid JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise Phase0DraftRejected(f"delivery {path.name} must be one JSON object")
    return value


def _write_new(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        with path.open("xb") as handle:
            handle.write(canonical_json(value))
            handle.flush()
            os.fsync(handle.fileno())
    except FileExistsError as exc:
        raise Phase0BridgeError(f"refusing to overwrite authority artifact {path}") from exc


def _write_replace(path: Path, value: dict[str, Any]) -> None:
    """Atomically refresh a mutable authority-owned projection."""
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{secrets.token_hex(8)}.next")
    try:
        _write_new(temporary, value)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _self_digest(value: dict[str, Any], field: str) -> str:
    unsigned = dict(value)
    unsigned.pop(field, None)
    return tagged_hash("raw-artifact", canonical_json(unsigned))


def _sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@contextmanager
def _temporary_environment(values: dict[str, str]) -> Iterator[None]:
    prior = {key: os.environ.get(key) for key in values}
    os.environ.update(values)
    try:
        yield
    finally:
        for key, value in prior.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def _copy_tree(source: Path, destination: Path) -> None:
    if destination.exists():
        raise Phase0BridgeError(f"snapshot destination already exists: {destination}")
    shutil.copytree(source, destination, symlinks=True, copy_function=shutil.copy2)
    # The canonical manifest rejects links and special files and therefore also
    # certifies that the copy has exactly the admitted source-tree surface.
    if source_tree_manifest(source) != source_tree_manifest(destination):
        shutil.rmtree(destination, ignore_errors=True)
        raise Phase0BridgeError("tree snapshot changed canonical paths or bytes")


class KernelPhase0:
    """Atomic CLI adapter; state output is replaced only after validation."""

    def __init__(self, executable: Path, state: Path, context: Path, authority: Path):
        self.executable = executable.resolve()
        self.state = state.resolve()
        self.context = context.resolve()
        self.authority = authority.resolve()
        self.sequence = 0
        self.interactive_discovery_receipts: list[dict[str, Any]] = []
        self._sync_projections()

    def _sync_projections(self) -> None:
        context = _read_json(self.context)
        state = _read_json(self.state)
        _write_replace(Path(context["ledger_path"]), state["ledger"])
        report = state.get("goal_binding_report")
        if isinstance(report, dict):
            _write_replace(Path(context["goal_binding_report_path"]), report)
        else:
            Path(context["goal_binding_report_path"]).unlink(missing_ok=True)

    def _run(self, arguments: list[str]) -> dict[str, Any]:
        completed = subprocess.run(
            [str(self.executable), *arguments],
            cwd=self.authority,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if completed.returncode != 0:
            detail = completed.stderr.decode("utf-8", errors="replace").strip()
            raise Phase0KernelRejection(
                f"kernel command {arguments[0]} failed ({completed.returncode}): {detail}"
            )
        try:
            output = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            raise Phase0BridgeError("kernel command returned malformed JSON") from exc
        if not isinstance(output, dict):
            raise Phase0BridgeError("kernel command returned a non-object")
        return output

    def check(self) -> dict[str, Any]:
        return self._run(["phase0-check", str(self.state), str(self.context)])

    def next(self) -> dict[str, Any]:
        return self._run(["phase0-next", str(self.state), str(self.context)])

    def apply(self, event: dict[str, Any]) -> dict[str, Any]:
        self.sequence += 1
        event_path = self.authority / "events" / f"{self.sequence:06d}.json"
        next_state = self.authority / f"state.next.{self.sequence:06d}.json"
        _write_new(event_path, event)
        output = self._run(
            ["phase0-apply", str(self.state), str(self.context), str(event_path), str(next_state)]
        )
        os.replace(next_state, self.state)
        self._sync_projections()
        return output

    def finalize(self, sealed_path: Path) -> dict[str, Any]:
        self.sequence += 1
        next_state = self.authority / f"state.next.{self.sequence:06d}.json"
        output = self._run(
            [
                "phase0-finalize",
                str(self.state),
                str(self.context),
                str(sealed_path),
                str(next_state),
            ]
        )
        os.replace(next_state, self.state)
        self._sync_projections()
        return output


class Phase0CheckerBroker:
    """Construct checked requests from trusted pins and freeze every input."""

    def __init__(
        self,
        *,
        kernel_cli: Path,
        checker_runner: Path,
        working_directory: Path,
        request_template: Path,
        authority_root: Path,
        state_path: Path,
        candidate_path: Path,
        interactive_candidate_root: Path | None = None,
    ):
        self.kernel_cli = kernel_cli.resolve()
        self.checker_runner = checker_runner.resolve()
        self.working_directory = working_directory.resolve()
        self.template = _read_json(request_template)
        self.authority = authority_root.resolve()
        self.state_path = state_path.resolve()
        self.candidate_path = candidate_path.resolve()
        self.interactive_candidates = (
            interactive_candidate_root.resolve()
            if interactive_candidate_root is not None else None
        )
        self.sequence = 0
        self.receipts = self.authority / "receipts"
        self.runs = self.authority / "checker-runs"
        self.freezes = self.authority / "checker-inputs"
        for path in (self.receipts, self.runs, self.freezes):
            path.mkdir(parents=True, exist_ok=True)

    def _state(self) -> dict[str, Any]:
        return _read_json(self.state_path)

    def check(
        self,
        kind: str = "candidate",
        *,
        candidate: Path | None = None,
        reverted_entry_id: str | None = None,
    ) -> dict[str, Any]:
        if kind not in {"candidate", "ratification_replay", "entry_ablation"}:
            raise Phase0BridgeError(f"unsupported checker kind {kind}")
        if (kind == "entry_ablation") != (reverted_entry_id is not None):
            raise Phase0BridgeError("an ablation check needs exactly one reverted entry id")
        state = self._state()
        self.sequence += 1
        check_id = f"phase0-{state['generation']}-{kind}-{self.sequence}"
        freeze = self.freezes / check_id
        _copy_tree((candidate or self.candidate_path).resolve(), freeze)
        manifest = source_tree_manifest(freeze)
        request = dict(self.template)
        # No caller-controlled executable, toolchain, environment, or output
        # paths survive this overwrite.  They come only from the pinned
        # authority template and current kernel state.
        request.update(
            {
                "schema": "trellis-pv-phase0-checker-request/v1",
                "phase0_genesis_sha256": state["genesis"]["genesis_sha256"],
                "check_id": check_id,
                "check_kind": kind,
                "ledger_generation": state["generation"],
                "ledger_root_sha256": state["ledger"]["ledger_entries_root"],
                "candidate_root": str(freeze),
                "candidate_manifest": manifest,
                "candidate_tree_sha256": manifest["source_tree_sha256"],
                "goal_path": self.template["goal_path"],
                "goal_sha256": state["genesis"]["goal_sha256"],
                "reverted_entry_id": reverted_entry_id,
                "runner_path": str(self.checker_runner),
                "runner_sha256": state["genesis"]["components"]["checker_runner"]["sha256"],
                "extractor_sha256": state["genesis"]["components"]["extractor"]["sha256"],
                "output_root": str(self.runs),
                "run_id": check_id,
            }
        )
        # Binding is a function of the canonical output directory identity.
        request["output_root_binding_sha256"] = _sha(str(self.runs).encode("utf-8"))
        request_path = self.authority / "checker-requests" / f"{check_id}.json"
        receipt_path = self.receipts / f"{check_id}.json"
        _write_new(request_path, request)
        completed = subprocess.run(
            [
                str(self.kernel_cli),
                "execute-phase0-checker",
                str(request_path),
                str(self.checker_runner),
                str(self.working_directory),
                str(receipt_path),
            ],
            cwd=self.working_directory,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if completed.returncode != 0:
            detail = completed.stderr.decode("utf-8", errors="replace").strip()
            raise Phase0BridgeError(f"checked executor failed ({completed.returncode}): {detail}")
        receipt = _read_json(receipt_path)
        output = receipt.get("parsed_stdout")
        if _environmental_checker_failure(receipt, output):
            self.cleanup({"frozen_tree": str(freeze), "run_root": str(self.runs / check_id)})
            raise Phase0EnvironmentalFailure(
                f"recorded environmental checker failure in {receipt_path}"
            )
        if not isinstance(output, dict):
            raise Phase0BridgeError("checked executor receipt has no structured checker output")
        return {
            "receipt": receipt,
            "receipt_path": str(receipt_path),
            "output": output,
            "frozen_tree": str(freeze),
            "run_root": str(self.runs / check_id),
        }

    def retain_interactive_candidate(self, result: dict[str, Any]) -> Path | None:
        """Copy an interactive check's candidate tree next to the burst logs.

        The receipt keeps only digests of what the worker tried, so a tree it
        checked and then abandoned leaves nothing an auditor can read. These
        copies are what a later reader compares against the ledgered edit to
        see whether a faithful restructuring was available. Bounded, oldest
        evicted; best effort, so retaining never fails a check."""
        root = self.interactive_candidates
        tree = result.get("frozen_tree")
        if root is None or not isinstance(tree, str) or not tree:
            return None
        try:
            root.mkdir(parents=True, exist_ok=True)
            existing = sorted(
                (path for path in root.iterdir() if path.is_dir()),
                key=_burst_log_set_order,
            )
            counter = _burst_log_set_order(existing[-1])[0] + 1 if existing else 1
            for stale in existing[
                : max(0, len(existing) + 1 - PHASE0_INTERACTIVE_CANDIDATE_SETS)
            ]:
                shutil.rmtree(stale, ignore_errors=True)
            destination = root / f"{counter:06d}-{_log_name_prefix(str(result['output'].get('check_id', '')))}"
            shutil.rmtree(destination, ignore_errors=True)
            destination.mkdir(parents=True)
            _copy_tree(Path(tree), destination / "candidate")
            _write_replace(
                destination / "check.json",
                {
                    "schema": "trellis-phase0-interactive-candidate/v1",
                    "check_id": str(result["output"].get("check_id", "")),
                    "status": str(result["output"].get("status", "")),
                    "first_failing_stage": result["output"].get("first_failing_stage"),
                    "candidate_tree_sha256": str(
                        result["output"].get("candidate_tree_sha256", "")
                    ),
                    "receipt_sha256": str(result["receipt"].get("receipt_sha256", "")),
                },
            )
            return destination
        except (OSError, KeyError, TypeError, ValueError) as error:
            print(
                f"phase0: could not retain the interactive candidate tree: {error}",
                file=sys.stderr,
                flush=True,
            )
            return None

    @staticmethod
    def cleanup(result: dict[str, Any]) -> None:
        """Remove bulky check trees after their receipt/output were consumed."""
        for field in ("frozen_tree", "run_root"):
            raw = result.get(field)
            if isinstance(raw, str) and raw:
                shutil.rmtree(raw, ignore_errors=True)

    @staticmethod
    def diagnostic(result: dict[str, Any]) -> dict[str, Any]:
        output = result["output"]
        stages = output.get("stages") if isinstance(output.get("stages"), list) else []
        failed = next(
            (stage for stage in stages if isinstance(stage, dict) and stage.get("status") == "failed"),
            None,
        )
        return {
            "schema": BROKER_RESULT_SCHEMA,
            "status": output.get("status"),
            "check_id": output.get("check_id"),
            "first_failing_stage": output.get("first_failing_stage"),
            "reason": output.get("reason"),
            "failed_stage": failed,
            "receipt_sha256": result["receipt"].get("receipt_sha256"),
        }

    @contextmanager
    def serve_worker(self, socket_path: Path) -> Iterator[dict[str, str]]:
        self.interactive_discovery_receipts = []
        token = secrets.token_hex(32)
        socket_path.parent.mkdir(parents=True, exist_ok=True)
        socket_path.unlink(missing_ok=True)
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(socket_path))
        os.chmod(socket_path, 0o600)
        server.listen(4)
        stopped = threading.Event()

        def run() -> None:
            while not stopped.is_set():
                try:
                    server.settimeout(0.2)
                    client, _ = server.accept()
                except TimeoutError:
                    continue
                except OSError:
                    return
                with client:
                    try:
                        raw = b""
                        while not raw.endswith(b"\n") and len(raw) <= 65536:
                            chunk = client.recv(4096)
                            if not chunk:
                                break
                            raw += chunk
                        call = json.loads(raw, object_pairs_hook=_strict_object)
                        if call != {
                            "schema": BROKER_CALL_SCHEMA,
                            "operation": "candidate",
                            "token": token,
                        }:
                            raise Phase0BridgeError("unauthorized or malformed broker call")
                        checked = self.check("candidate")
                        response = self.diagnostic(checked)
                        if checked["output"].get("status") == "failed":
                            self.interactive_discovery_receipts.append(checked["receipt"])
                        self.retain_interactive_candidate(checked)
                        self.cleanup(checked)
                    except Exception as exc:  # returned as transport data, never authority
                        response = {
                            "schema": BROKER_RESULT_SCHEMA,
                            "status": "broker_error",
                            "reason": str(exc),
                        }
                    try:
                        client.sendall(canonical_json(response))
                    except OSError as exc:
                        # The worker hung up (timeout, ctrl-c, crash) before
                        # reading its answer. That is its loss, not the
                        # broker's: the thread must survive so the next
                        # check.py call in this burst is served instead of
                        # hanging until the client's own timeout.
                        print(f"phase0: broker client disconnected before reply: {exc}",
                              file=sys.stderr, flush=True)

        thread = threading.Thread(target=run, name="phase0-checker-broker", daemon=True)
        thread.start()
        try:
            yield {
                "TRELLIS_PHASE0_CHECKER_SOCKET": str(socket_path),
                "TRELLIS_PHASE0_CHECKER_TOKEN": token,
            }
        finally:
            stopped.set()
            server.close()
            thread.join(timeout=2)
            socket_path.unlink(missing_ok=True)



# --- Revision history -------------------------------------------------------
#
# Worker bursts run on fresh provider threads, so a worker sees only the
# current candidate tree, the current ledger and the last authoritative
# failure.  The bridge therefore derives a short account of what earlier
# accepted generations already tried, and what the checker said afterwards,
# from kernel state alone.  It is transport, not authority: nothing here
# enters the contract bytes or any kernel transition.

REVISION_HISTORY_HEADER = (
    "Revision history, derived by the framework from kernel state (oldest first, "
    "most recent last). Each item is one accepted generation: the ledger entries it "
    "changed and the first failing stage's diagnostic afterwards. A variant listed "
    "here with the same diagnostic afterwards did not resolve it."
)

_ANSI_ESCAPE = re.compile(
    r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[@-Z\\-_]"
)
# cargo and rustc open a diagnostic with `error:` or `error[E....]:`.
_TOOL_ERROR_LINE = re.compile(r"^error(\[|:|\s|$)")


def _generation_key(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str):
        try:
            return int(value.strip())
        except ValueError:
            return None
    return None


def _decode_stage_stream(stage: dict[str, Any], key: str) -> str:
    raw = stage.get(key)
    if not isinstance(raw, str) or not raw:
        return ""
    try:
        return base64.b64decode(raw).decode("utf-8", "replace")
    except (ValueError, TypeError):
        return ""


def _terminal_lines(text: str) -> list[str]:
    """Lines as a terminal would leave them: escape sequences removed and each
    carriage-return progress overwrite collapsed to its final segment."""
    return [
        line.split("\r")[-1]
        for line in _ANSI_ESCAPE.sub("", text).splitlines()
    ]


def extract_stage_diagnostic(
    stage: dict[str, Any], *, max_lines: int = 12, max_bytes: int = 2000
) -> list[str]:
    """The diagnostic lines of one checker stage: extractor `[Error]` lines with
    the `Source:` line that follows each, and cargo/rustc `error` lines."""
    kept: list[str] = []
    for key in ("stdout_base64", "stderr_base64"):
        expect_source = False
        for line in _terminal_lines(_decode_stage_stream(stage, key)):
            text = line.strip()
            if not text:
                expect_source = False
                continue
            if text.startswith("[Error]"):
                kept.append(text)
                expect_source = True
                continue
            if expect_source and (text.startswith("Source:") or text.startswith("-->")):
                kept.append(text)
                expect_source = False
                continue
            expect_source = False
            if _TOOL_ERROR_LINE.match(text):
                # cargo/rustc print the location on the next `-->` line.
                kept.append(text)
                expect_source = True
    # `max_lines` counts diagnostics, not lines: a location line that follows
    # an error line belongs to that diagnostic and costs only bytes.
    bounded: list[str] = []
    used = 0
    diagnostics = 0
    for line in kept:
        is_location = line.startswith("Source:") or line.startswith("-->")
        if not is_location:
            if diagnostics >= max_lines:
                break
            diagnostics += 1
        width = len(line.encode("utf-8")) + 1
        if used + width > max_bytes:
            break
        bounded.append(line)
        used += width
    return bounded


def _ledger_entries(record: dict[str, Any]) -> list[dict[str, Any]]:
    ledger = record.get("ledger")
    if not isinstance(ledger, dict):
        return []
    entries = ledger.get("entries")
    if not isinstance(entries, list):
        return []
    return [entry for entry in entries if isinstance(entry, dict)]


def _entry_span(entry: dict[str, Any]) -> dict[str, Any]:
    span = entry.get("before_span")
    return span if isinstance(span, dict) else {}


def _entry_identity(entry: dict[str, Any]) -> tuple[str, str, str, str]:
    span = _entry_span(entry)
    return (
        str(entry.get("file")),
        str(entry.get("operation")),
        str(span.get("start")),
        str(span.get("end")),
    )


def _entry_patch(entry: dict[str, Any]) -> dict[str, Any]:
    patch = entry.get("patch")
    return patch if isinstance(patch, dict) else {}


def _patch_fingerprint(entry: dict[str, Any]) -> tuple[str, str]:
    patch = _entry_patch(entry)
    values: list[str] = []
    for digest_key, bytes_key in (
        ("inserted_sha256", "inserted_base64"),
        ("removed_sha256", "removed_base64"),
    ):
        digest = patch.get(digest_key)
        if isinstance(digest, str) and digest:
            values.append(digest)
        else:
            # An unsealed row carries no digest; the encoded bytes stand in.
            payload = patch.get(bytes_key)
            values.append(
                hashlib.sha256(payload.encode("utf-8")).hexdigest()
                if isinstance(payload, str)
                else ""
            )
    return (values[0], values[1])


def _generation_changes(
    current: list[dict[str, Any]], previous: list[dict[str, Any]]
) -> list[tuple[tuple[str, str, str, str], dict[str, Any], str]]:
    earlier = {_entry_identity(entry): entry for entry in previous}
    changes: list[tuple[tuple[str, str, str, str], dict[str, Any], str]] = []
    live: set[tuple[str, str, str, str]] = set()
    for entry in current:
        identity = _entry_identity(entry)
        live.add(identity)
        before = earlier.get(identity)
        if before is None:
            changes.append((identity, entry, "new"))
        elif _patch_fingerprint(before) != _patch_fingerprint(entry):
            changes.append((identity, entry, "replaced"))
    for identity, entry in earlier.items():
        if identity not in live:
            changes.append((identity, entry, "dropped"))
    changes.sort(key=lambda change: (change[0], change[2]))
    return changes


def _authoritative_failures(state: dict[str, Any]) -> dict[int, dict[str, Any]]:
    receipts: dict[str, dict[str, Any]] = {}
    stored = state.get("checker_receipts")
    for record in stored if isinstance(stored, list) else []:
        if not isinstance(record, dict):
            continue
        digest = record.get("receipt_sha256")
        receipt = record.get("receipt")
        if isinstance(digest, str) and isinstance(receipt, dict):
            receipts[digest] = receipt
    latest: dict[int, tuple[int, str]] = {}
    events = state.get("event_history")
    for position, event in enumerate(events if isinstance(events, list) else []):
        if not isinstance(event, dict) or event.get("kind") != "checker_failure":
            continue
        generation = _generation_key(event.get("generation"))
        digest = event.get("evidence_sha256")
        if generation is None or not isinstance(digest, str):
            continue
        sequence = event.get("sequence")
        order = sequence if isinstance(sequence, int) and not isinstance(sequence, bool) else position
        if generation not in latest or order >= latest[generation][0]:
            latest[generation] = (order, digest)
    resolved: dict[int, dict[str, Any]] = {}
    for generation, (_order, digest) in latest.items():
        receipt = receipts.get(digest)
        if receipt is not None:
            resolved[generation] = receipt
    return resolved


def _failing_stage(receipt: dict[str, Any]) -> tuple[str, dict[str, Any] | None]:
    parsed = receipt.get("parsed_stdout")
    if not isinstance(parsed, dict):
        return ("", None)
    name = parsed.get("first_failing_stage")
    name = name if isinstance(name, str) else ""
    stages = parsed.get("stages")
    named = [
        stage
        for stage in (stages if isinstance(stages, list) else [])
        if isinstance(stage, dict) and stage.get("name") == name
    ]
    if not named:
        return (name, None)
    for stage in named:
        if stage.get("status") == "failed":
            return (name, stage)
    return (name, named[0])


def _receipt_reason_lines(receipt: dict[str, Any], *, max_bytes: int = 2000) -> list[str]:
    """The runner's own `reason` for a failure, for stages whose diagnostic is
    not a tool stream (packaging checks, tree gates)."""
    parsed = receipt.get("parsed_stdout")
    reason = parsed.get("reason") if isinstance(parsed, dict) else None
    if not isinstance(reason, str):
        return []
    lines = [line.strip() for line in _terminal_lines(reason) if line.strip()]
    kept: list[str] = []
    used = 0
    for line in lines[:4]:
        width = len(line.encode("utf-8")) + 1
        if used + width > max_bytes:
            break
        kept.append(line)
        used += width
    return kept


def _diagnostic_texts(receipt: dict[str, Any] | None) -> list[str]:
    """The failing stage's diagnostic lines without their location lines, in
    order, uncapped enough to compare two generations' error sets."""
    if receipt is None:
        return []
    _name, stage = _failing_stage(receipt)
    lines = extract_stage_diagnostic(stage, max_lines=400, max_bytes=65536) if stage is not None else []
    if not lines:
        lines = _receipt_reason_lines(receipt)
    return [line for line in lines if not (line.startswith("Source:") or line.startswith("-->"))]


def _failure_lines(
    receipt: dict[str, Any] | None, same_texts_as: int | None = None
) -> list[str]:
    if receipt is None:
        return ["  afterwards: no failure receipt for this generation."]
    name, stage = _failing_stage(receipt)
    label = name or "unnamed"
    diagnostic = extract_stage_diagnostic(stage) if stage is not None else []
    if not diagnostic:
        diagnostic = _receipt_reason_lines(receipt)
    if not diagnostic:
        return [f"  afterwards: stage {label} failed with no line the framework recognises."]
    lines = [f"  afterwards: stage {label} failed"] + [f"    {line}" for line in diagnostic]
    if same_texts_as is not None:
        # An edit that only moves a diagnostic to another location leaves
        # the error texts unchanged; the location lines alone differ.
        lines.append(
            f"  the diagnostic texts are the same as after generation {same_texts_as}; "
            "only locations differ"
        )
    return lines


def _inserted_text(entry: dict[str, Any], max_inserted_bytes: int) -> str:
    payload = _entry_patch(entry).get("inserted_base64")
    if not isinstance(payload, str) or not payload:
        return ""
    try:
        data = base64.b64decode(payload)
    except (ValueError, TypeError):
        return ""
    if not data:
        return ""
    if len(data) <= max_inserted_bytes:
        return data.decode("utf-8", "replace")
    head = data[:max_inserted_bytes].decode("utf-8", "replace")
    return f"{head}\u2026 [truncated, {len(data)} bytes]"


def _render_generation(
    generation: int,
    changes: list[tuple[tuple[str, str, str, str], dict[str, Any], str]],
    receipt: dict[str, Any] | None,
    max_inserted_bytes: int,
    same_texts_as: int | None = None,
) -> str:
    lines = [f"generation {generation}"]
    for _identity, entry, kind in changes:
        span = _entry_span(entry)
        lines.append(
            f"  {kind} {entry.get('file')} {entry.get('operation')} "
            f"before_span {span.get('start')}..{span.get('end')}"
        )
        if kind == "dropped" or entry.get("operation") == "delete":
            continue
        inserted = _inserted_text(entry, max_inserted_bytes)
        if inserted:
            lines.append("    inserted:")
            lines.extend(f"      {line}" for line in inserted.splitlines())
    lines.extend(_failure_lines(receipt, same_texts_as))
    return "\n".join(lines)


def render_revision_history(
    state: dict[str, Any],
    *,
    max_generations: int = 12,
    max_bytes: int = 32768,
    max_inserted_bytes: int = 1200,
) -> str | None:
    """A bounded, deterministic account of the accepted generations so far.

    Returns None when the kernel has accepted no generation that changed the
    ledger, so the caller appends nothing.
    """
    history = state.get("generation_history")
    records = [record for record in history if isinstance(history, list) and isinstance(record, dict)] if isinstance(history, list) else []
    # Kernel state keeps the history of earlier generations only; the
    # current generation's ledger is `state["ledger"]`, so it is appended as
    # the newest record when it is not already recorded.
    current_generation = _generation_key(state.get("generation"))
    current_ledger = state.get("ledger")
    if current_generation is not None and isinstance(current_ledger, dict):
        recorded = {
            _generation_key(record.get("generation")) for record in records
        }
        if current_generation not in recorded:
            records = [*records, {"generation": current_generation, "ledger": current_ledger}]
    if not records:
        return None
    ordered = sorted(
        enumerate(records),
        key=lambda pair: (
            _generation_key(pair[1].get("generation"))
            if _generation_key(pair[1].get("generation")) is not None
            else pair[0],
            pair[0],
        ),
    )
    failures = _authoritative_failures(state)
    items: list[str] = []
    previous: list[dict[str, Any]] = []
    previous_failure: tuple[int, list[str]] | None = None
    for position, record in ordered:
        current = _ledger_entries(record)
        changes = _generation_changes(current, previous)
        previous = current
        if not changes:
            continue
        generation = _generation_key(record.get("generation"))
        generation = position if generation is None else generation
        receipt = failures.get(generation)
        # Compared as a multiset: extractors print diagnostics in varying order.
        texts = sorted(_diagnostic_texts(receipt))
        same_texts_as = None
        if texts and previous_failure is not None and previous_failure[1] == texts:
            same_texts_as = previous_failure[0]
        if receipt is not None:
            previous_failure = (generation, texts)
        items.append(
            _render_generation(generation, changes, receipt, max_inserted_bytes, same_texts_as)
        )
    if not items:
        return None
    items = items[-max_generations:] if max_generations > 0 else items[-1:]

    def assemble(chosen: list[str]) -> str:
        return "\n\n".join([REVISION_HISTORY_HEADER, *chosen])

    note = assemble(items)
    while len(note.encode("utf-8")) > max_bytes and len(items) > 1:
        items = items[1:]
        note = assemble(items)
    encoded = note.encode("utf-8")
    if len(encoded) > max_bytes:
        note = encoded[:max_bytes].decode("utf-8", "ignore")
    return note


def _log_name_prefix(value: str) -> str:
    """The log-file prefix the agent wrapper derives from an artifact prefix."""
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", str(value)).strip("._-")[:80]


def _burst_log_set_order(path: Path) -> tuple[int, str]:
    counter, _, _ = path.name.partition("-")
    return (int(counter) if counter.isdigit() else 0, path.name)


def _burst_log_sets(logs: Path) -> tuple[Path, list[Path]]:
    history = logs / "history"
    history.mkdir(parents=True, exist_ok=True)
    return history, sorted(
        (path for path in history.iterdir() if path.is_dir()), key=_burst_log_set_order
    )


def _new_burst_log_set(logs: Path, request_id: str) -> Path:
    """Allocate this burst's retained log set and drop the oldest sets over
    the retention bound."""
    history, existing = _burst_log_sets(logs)
    counter = _burst_log_set_order(existing[-1])[0] + 1 if existing else 1
    for stale in existing[: max(0, len(existing) + 1 - PHASE0_LOG_HISTORY_SETS)]:
        shutil.rmtree(stale, ignore_errors=True)
    destination = history / f"{counter:06d}-{_log_name_prefix(request_id)}"
    destination.mkdir(parents=True, exist_ok=True)
    return destination


def _burst_log_set_for(logs: Path, request_id: str, *, claim: str) -> Path:
    """The set of the burst that just ran for this request, or a fresh set
    when no dispatch allocated one. A set that already holds `claim` belongs
    to an earlier burst of the same request, so it is never reused."""
    suffix = f"-{_log_name_prefix(request_id)}"
    _history, existing = _burst_log_sets(logs)
    for path in reversed(existing):
        if path.name.endswith(suffix):
            return path if not (path / claim).exists() else _new_burst_log_set(logs, request_id)
    return _new_burst_log_set(logs, request_id)


class ExistingLaneDispatcher:
    """Bind Phase 0 to the existing wrapper's worker and reviewer identities."""

    def __init__(self, attempt_root: Path, genesis: dict[str, Any]):
        self.attempt = attempt_root.resolve()
        self.genesis = genesis
        self.output = self.attempt / "agent-output"
        self.output.mkdir(parents=True, exist_ok=True)
        write_phase0_candidate_script(self.attempt)
        # Homes are seeded per dispatch (see `_dispatch`), exactly as the
        # campaign bridge does, so provider credentials refreshed on the
        # host between bursts reach the next burst.
        (self.attempt / "burst-homes").mkdir(parents=True, exist_ok=True)

    @staticmethod
    def _provider(binding: dict[str, Any]) -> ProviderConfig:
        return ProviderConfig(
            provider=binding["provider"],
            model=binding["model"],
            effort=binding["effort"],
            extra_args=list(binding["extra_args"]),
            fallback_models=list(binding["fallback_models"]),
        )

    def _dispatch(
        self,
        *,
        request: dict[str, Any],
        binding: dict[str, Any],
        fragment: str,
        role: str,
        environment: dict[str, str],
        persistent: bool,
        rejection: str | None = None,
        revision_history: str | None = None,
    ) -> dict[str, Any]:
        artifact_name = f"{request['request_id']}.json"
        artifact = ArtifactSpec(canonical_name=artifact_name, kind="phase0-raw")
        paths = prepare_artifact_paths(self.output, self.attempt, artifact_name)
        paths.raw.parent.mkdir(parents=True, exist_ok=True)
        fragment_path = Path(__file__).resolve().parents[1] / "trellis" / "prompt_fragments" / fragment
        role_contract = fragment_path.read_text(encoding="utf-8").strip()
        prompt = (
            role_contract
            + "\n\nThe kernel-generated contract follows. Preserve its exact bindings.\n"
            + canonical_json(request).decode("utf-8")
            + f"\nWrite the response JSON object to {paths.raw} (any formatting) "
            + f"and then create {paths.done}."
        )
        if revision_history and persistent and role == "source_adaptation_worker":
            # Bridge-authored context for the worker's fresh provider thread:
            # what earlier accepted generations changed and what the checker
            # said afterwards.  Derived from kernel state, appended after the
            # contract bytes, which stay exactly as the kernel emitted them.
            prompt += "\n\n" + revision_history
        if rejection:
            # Bridge-authored feedback for a re-dispatch of the same request:
            # the contract bytes above are unchanged, so the kernel binding
            # holds; only the reason the previous delivery was refused is added.
            prompt += (
                "\n\nYour previous delivery for this request was rejected: "
                + rejection
                + "\nThe candidate tree was restored to its state before that attempt, so "
                + "re-apply your file edits as well as correcting the response, then deliver "
                + "to the same paths."
            )
        lane_id = str(binding["lane_id"])
        # The sandbox binds this fake home instead of the operator's real
        # one, so the provider CLI's state and credentials (`~/.codex`,
        # `~/.claude`, ...) must be hard-linked in here or every burst
        # fails with 401 before it starts; `persistent=True` keeps the
        # worker's rollouts so its session can resume across hand-backs.
        burst_home = seed_burst_home(
            self.attempt, "worker" if persistent else "reviewer", persistent=True
        )
        single = SingleAgentRequest(
            request_id=request["request_id"],
            cycle=int(request["generation"]),
            kind="phase0-source-adaptation",
            burst_role=role,
            provider=self._provider(binding),
            prompt=prompt,
            work_dir=self.attempt,
            state_dir=self.output,
            session_name=f"phase0-{role}-{lane_id}",
            session_scope=(
                f"source_adaptation:worker:{binding['binding_sha256']}"
                if persistent
                else f"source_adaptation:audit:{request['request_sha256']}"
            ),
            lane=AgentLane(kind=lane_id),
            timeout_seconds=7200.0 if persistent else 3600.0,
            startup_timeout_seconds=3600.0,
            burst_home=burst_home,
            log_dir=self.output / "logs",
            fresh=not persistent,
            artifact=artifact,
            artifact_prefix=f"phase0-{lane_id}",
            sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        )
        paths.raw.unlink(missing_ok=True)
        paths.done.unlink(missing_ok=True)
        try:
            with _temporary_environment(environment):
                response = execute_agent_request(single, validate_artifact=False)
        finally:
            # This burst's set is retained as it ends, before the next burst
            # of the lane overwrites the live names, so the transcript of a
            # delivery that is about to be refused survives the re-dispatch.
            # A burst whose transport raised is retained too.
            self._retain_burst_logs(prefix=single.artifact_prefix, request_id=request["request_id"])
        if not response.ok:
            raise Phase0BridgeError(f"agent transport failed: {response.error}")
        return _read_agent_json(paths.raw)

    def _retain_burst_logs(self, *, prefix: str, request_id: str) -> Path | None:
        """Copy the burst's log set into `logs/history/<counter>-<request_id>/`.

        The live names under `logs/` are unchanged, so the `.started`/`.exit`
        markers a monitor watches stay at their predictable path for the burst
        that is running. Symlinked logs are copied by content, since the file
        they point at is truncated by the next burst. Best effort: a log copy
        never fails a dispatch."""
        logs = self.output / "logs"
        prefix = _log_name_prefix(prefix)
        try:
            destination = _new_burst_log_set(logs, request_id)
            if not logs.is_dir():
                return destination
            for entry in sorted(logs.iterdir()):
                name = entry.name
                if name != prefix and not name.startswith((f"{prefix}.", f"{prefix}-")):
                    continue
                if entry.is_file():
                    shutil.copyfile(entry, destination / name)
            return destination
        except OSError as error:
            print(f"phase0: could not retain the burst log set: {error}", file=sys.stderr, flush=True)
            return None

    def worker(
        self,
        request: dict[str, Any],
        broker_environment: dict[str, str],
        rejection: str | None = None,
        revision_history: str | None = None,
    ) -> dict[str, Any]:
        environment = {"TRELLIS_SANDBOX_EXTRA_RO_PATHS": "", **broker_environment}
        return self._dispatch(
            request=request,
            binding=self.genesis["bindings"]["worker"],
            fragment=request["prompt_fragments"][0],
            role="source_adaptation_worker",
            environment=environment,
            persistent=True,
            rejection=rejection,
            revision_history=revision_history,
        )

    def reviewer(self, request: dict[str, Any]) -> dict[str, Any]:
        fragment = (
            "pv/source_adaptation/verifier/deviation/05_role.md"
            if request["scenario"] == "seam_repair"
            else "pv/source_adaptation/verifier/correspondence/05_role.md"
        )
        readonly = request["read_only_paths"]
        extra = os.pathsep.join(str(value) for value in readonly.values())
        return self._dispatch(
            request=request,
            binding=request["lane"],
            fragment=fragment,
            role="source_adaptation_reviewer",
            environment={"TRELLIS_SANDBOX_EXTRA_RO_PATHS": extra},
            persistent=False,
        )


def _diff_digest(entry: dict[str, Any]) -> str:
    # Mirrors the kernel's `adaptation_diff_digest`: identity is the change
    # to the uploaded file, not the derived after-span or whole-file digests.
    core = {
        "before_span": entry["before_span"],
        "file": entry["file"],
        "operation": entry["operation"],
        "patch": entry["patch"],
    }
    return tagged_hash("adaptation-ledger-row", canonical_json(core))


_HEX64 = frozenset("0123456789abcdef")


def _is_hex64(value: Any) -> bool:
    return isinstance(value, str) and len(value) == 64 and set(value) <= _HEX64


def _check_entry_shapes(entry: dict[str, Any]) -> None:
    """Reject a draft entry whose field shapes do not match the contract, with
    a reason the worker can act on, before any field is dereferenced."""
    where = f"entry for {entry.get('file')!r}"
    if not isinstance(entry.get("file"), str) or not entry["file"]:
        raise Phase0DraftRejected(f"{where}: file must be a non-empty relative path string")
    if entry.get("operation") not in ("add", "delete", "modify"):
        raise Phase0DraftRejected(f"{where}: operation must be one of add, delete, modify")
    for name in ("before_span", "after_span"):
        span = entry.get(name)
        if (
            not isinstance(span, dict) or set(span) != {"start", "end"}
            or not all(isinstance(span[k], int) and not isinstance(span[k], bool) and span[k] >= 0 for k in ("start", "end"))
            or span["start"] > span["end"]
        ):
            raise Phase0DraftRejected(
                f"{where}: {name} must be an object {{\"start\": int, \"end\": int}} of byte offsets with start <= end"
            )
    for name in ("before_sha256", "after_sha256"):
        value = entry.get(name)
        if value is not None and not _is_hex64(value):
            raise Phase0DraftRejected(f"{where}: {name} must be null or 64 lowercase hex characters")
    if not isinstance(entry.get("behavior_preservation_claim"), str) or not entry["behavior_preservation_claim"].strip():
        raise Phase0DraftRejected(f"{where}: behavior_preservation_claim must be a non-empty string")
    if entry.get("seam_class") not in ("i", "ii", "iii", "packaging_shim"):
        raise Phase0DraftRejected(f"{where}: seam_class must be one of i, ii, iii, packaging_shim")
    citation = entry.get("citation")
    citation_shapes = {"language_guarantee": "statement", "upstream_reference": "reference"}
    text_field = citation_shapes.get(citation.get("kind")) if isinstance(citation, dict) else None
    if (
        text_field is None or set(citation) != {"kind", text_field}
        or not isinstance(citation[text_field], str) or not citation[text_field].strip()
    ):
        raise Phase0DraftRejected(
            f"{where}: citation must be {{\"kind\": \"language_guarantee\", \"statement\": text}} "
            f"or {{\"kind\": \"upstream_reference\", \"reference\": text}}"
        )
    if not isinstance(entry.get("meaning_change"), bool):
        raise Phase0DraftRejected(f"{where}: meaning_change must be a boolean")
    quote = entry.get("quoted_error")
    if isinstance(quote, dict) and set(quote) == {"tool", "receipt_sha256", "stage_index", "stream", "byte_offset", "quote"}:
        if (
            not isinstance(quote["tool"], str) or not _is_hex64(quote["receipt_sha256"])
            or not isinstance(quote["stage_index"], int) or isinstance(quote["stage_index"], bool) or quote["stage_index"] < 0
            or quote["stream"] not in ("stderr", "stdout")
            or not isinstance(quote["byte_offset"], int) or isinstance(quote["byte_offset"], bool) or quote["byte_offset"] < 0
            or not isinstance(quote["quote"], str) or not quote["quote"]
        ):
            raise Phase0DraftRejected(
                f"{where}: quoted_error needs tool (string), receipt_sha256 (64 hex), stage_index (int >= 0), "
                "stream (stderr|stdout), byte_offset (int >= 0), quote (non-empty exact text)"
            )
    patch = entry.get("patch")
    if isinstance(patch, dict) and not all(isinstance(patch.get(k), str) for k in ("removed_base64", "inserted_base64")):
        raise Phase0DraftRejected(f"{where}: patch.removed_base64 and patch.inserted_base64 must be base64 strings")


def _receipt_stream_bytes(receipt: dict[str, Any], stage_index: int, stream: str) -> bytes | None:
    stages = (receipt.get("parsed_stdout") or {}).get("stages")
    if not isinstance(stages, list) or not (0 <= stage_index < len(stages)):
        return None
    encoded = stages[stage_index].get(f"{stream}_base64")
    if not isinstance(encoded, str):
        return None
    try:
        return base64.b64decode(encoded, validate=True)
    except (TypeError, ValueError):
        return None


def _check_quote_is_a_complete_line(
    entry: dict[str, Any], receipts: dict[str, dict[str, Any]]
) -> None:
    """The kernel proves an ablation reproduces a diagnostic by whole-line
    equality against the cited stream, so a quote that is only part of a line
    can never be reproduced. Refuse it here, naming the complete line."""
    quote = entry["quoted_error"]
    receipt = receipts.get(str(quote.get("receipt_sha256")))
    if receipt is None:
        return  # existence and exactness are the kernel's to judge
    stages = (receipt.get("parsed_stdout") or {}).get("stages") or []
    stage_index = int(quote["stage_index"])
    if 0 <= stage_index < len(stages) and isinstance(stages[stage_index].get("tool"), str):
        # The tool name is a fact of the cited stage, not something the worker
        # should have to transcribe; the kernel refuses any mismatch.
        quote["tool"] = stages[stage_index]["tool"]
    data = _receipt_stream_bytes(receipt, stage_index, str(quote["stream"]))
    if data is None:
        return
    offset = int(quote["byte_offset"])
    text = str(quote["quote"]).encode("utf-8")
    where = f"entry for {entry.get('file')!r}"
    if data[offset:offset + len(text)] != text:
        # The quote text is the exact evidence; its offset is derivable. Snap
        # to the nearest occurrence of the quote as a complete line, if any.
        candidates = []
        start = 0
        while True:
            found = data.find(text, start)
            if found < 0:
                break
            line_start = found == 0 or data[found - 1:found] == b"\n"
            after = data[found + len(text):found + len(text) + 1]
            if line_start and after in (b"", b"\n", b"\r"):
                candidates.append(found)
            start = found + 1
        if not candidates:
            raise Phase0DraftRejected(
                f"{where}: quoted_error.quote does not occur as a complete line of the receipt's "
                f"{quote['stream']}; the text at byte_offset {offset} is {data[offset:offset + len(text)][:80]!r}"
            )
        snapped = min(candidates, key=lambda c: abs(c - offset))
        print(
            f"phase0: snapped quoted_error.byte_offset {offset} -> {snapped} for {entry.get('file')!r} "
            "(quote found as a complete line there)", file=sys.stderr, flush=True,
        )
        quote["byte_offset"] = offset = snapped
    line_start = data.rfind(b"\n", 0, offset) + 1
    line_end = data.find(b"\n", offset)
    line = data[line_start: len(data) if line_end < 0 else line_end].rstrip(b"\r")
    if offset != line_start or text != line:
        raise Phase0DraftRejected(
            f"{where}: quoted_error.quote must be one complete line of the receipt's {quote['stream']} "
            f"starting at its byte_offset; the line at offset {line_start} is: {line.decode('utf-8', 'replace')}"
        )


def _recompute_derived_fields(entries: list[dict[str, Any]], state: dict[str, Any], candidate: Path) -> None:
    """Fill every entry's derived fields the way the kernel's replay expects
    them: whole-file before/after digests (upload manifest and candidate
    tree) and, for modifies, the after-span rebased over all earlier spans of
    the same file. The worker supplies only the original-file span and the
    patch; these fields are computed_by_framework."""
    unadapted_files = {
        f["relative_path"]: f["sha256_of_raw_bytes"]
        for f in state["genesis"]["unadapted_source_manifest"]["files"]
    }
    by_file: dict[str, list[dict[str, Any]]] = {}
    for entry in entries:
        by_file.setdefault(entry["file"], []).append(entry)
    for file, rows in by_file.items():
        candidate_file = candidate / file
        after_digest = _sha(candidate_file.read_bytes()) if candidate_file.is_file() else None
        operations = {row["operation"] for row in rows}
        if len(operations) != 1:
            raise Phase0DraftRejected(f"entries for {file!r} mix operations {sorted(operations)}")
        operation = operations.pop()
        if operation == "add":
            for row in rows:
                inserted = base64.b64decode(row["patch"]["inserted_base64"])
                row["before_sha256"], row["after_sha256"] = None, _sha(inserted)
                row["after_span"] = {"start": 0, "end": len(inserted)}
            continue
        if file not in unadapted_files:
            raise Phase0DraftRejected(f"entry for {file!r}: {operation} needs the file in the upload")
        if operation == "delete":
            for row in rows:
                row["before_sha256"], row["after_sha256"] = unadapted_files[file], None
                row["after_span"] = {"start": 0, "end": 0}
            continue
        if after_digest is None:
            raise Phase0DraftRejected(f"entry for {file!r}: a modify needs the file in the candidate")
        rows.sort(key=lambda row: row["before_span"]["start"])
        shift = 0
        previous_end = 0
        for row in rows:
            start, end = row["before_span"]["start"], row["before_span"]["end"]
            if start < previous_end:
                raise Phase0DraftRejected(
                    f"entry for {file!r}: before_span {start}..{end} overlaps an earlier entry's span; "
                    "before_span is measured in the uploaded file"
                )
            inserted = base64.b64decode(row["patch"]["inserted_base64"])
            removed = base64.b64decode(row["patch"]["removed_base64"])
            row["after_span"] = {"start": start + shift, "end": start + shift + len(inserted)}
            row["before_sha256"], row["after_sha256"] = unadapted_files[file], after_digest
            shift += len(inserted) - len(removed)
            previous_end = end


def _replay_file(original: bytes, rows: list[dict[str, Any]]) -> tuple[bytes | None, str | None]:
    """Replay modify rows (sorted by before_span) the way the kernel does.
    Returns (output, None) or (None, reason)."""
    out = bytearray()
    cursor = 0
    for row in sorted(rows, key=lambda r: r["before_span"]["start"]):
        start, end = row["before_span"]["start"], row["before_span"]["end"]
        if start < cursor or end > len(original):
            return None, f"before_span {start}..{end} overlaps an earlier span or exceeds the uploaded file ({len(original)} bytes)"
        removed = base64.b64decode(row["patch"]["removed_base64"])
        inserted = base64.b64decode(row["patch"]["inserted_base64"])
        if original[start:end] != removed:
            return None, (
                f"before_span {start}..{end} of the uploaded file holds {original[start:end][:80]!r}, "
                f"not the declared removed bytes {removed[:80]!r}"
            )
        out += original[cursor:start]
        out += inserted
        cursor = end
    out += original[cursor:]
    return bytes(out), None


def _candidate_offset_to_original(offset: int, prior_rows: list[dict[str, Any]]) -> int | None:
    """Map a byte offset of the current candidate file back to the uploaded
    file through the already-accepted rows of that file (None if it falls
    inside inserted text, which has no original position)."""
    shift = 0
    for row in sorted(prior_rows, key=lambda r: r["before_span"]["start"]):
        start, end = row["before_span"]["start"], row["before_span"]["end"]
        inserted_len = len(base64.b64decode(row["patch"]["inserted_base64"]))
        cand_start = start + shift
        if offset < cand_start:
            return offset - shift
        if offset < cand_start + inserted_len:
            return None if offset != cand_start else start
        shift += inserted_len - (end - start)
    return offset - shift


def _reject_edits_absent_from_the_candidate(
    entries: list[dict[str, Any]], candidate: Path
) -> None:
    """An add or delete entry describes a file the candidate tree must already
    carry (or no longer carry). The kernel would refuse the whole draft with
    the ledger/tree path-set mismatch; reject here instead, naming the file and
    what the candidate actually holds."""
    for row in entries:
        file = row["file"]
        target = candidate / file
        if row["operation"] == "add":
            inserted = base64.b64decode(row["patch"]["inserted_base64"])
            if not target.is_file():
                raise Phase0DraftRejected(
                    f"entry for {file!r}: the add is described but the edit was not made in the "
                    f"candidate tree; write {file} under this request's candidate_tree_path, then deliver"
                )
            current = target.read_bytes()
            if current != inserted:
                raise Phase0DraftRejected(
                    f"entry for {file!r}: the add declares {len(inserted)} inserted bytes but the "
                    f"candidate tree holds {len(current)} bytes starting {current[:80]!r}; the file in "
                    "the candidate tree must be exactly the inserted bytes"
                )
        elif row["operation"] == "delete":
            if target.exists():
                raise Phase0DraftRejected(
                    f"entry for {file!r}: the delete is described but the edit was not made in the "
                    f"candidate tree; remove {file} under this request's candidate_tree_path, then deliver"
                )


def _simulate_replay_or_reject(
    entries: list[dict[str, Any]], unadapted: Path, candidate: Path, *, new_row_ids: set[int]
) -> list[dict[str, Any]]:
    """Run the kernel's per-file replay ahead of time. A new row whose span
    was measured in the candidate instead of the uploaded file is translated
    when that interpretation reproduces the candidate exactly; anything else
    is refused with the bytes actually found, so the worker can correct it."""
    _reject_edits_absent_from_the_candidate(entries, candidate)
    translated_rows: list[dict[str, Any]] = []
    by_file: dict[str, list[dict[str, Any]]] = {}
    for row in entries:
        if row["operation"] == "modify":
            by_file.setdefault(row["file"], []).append(row)
    for file, rows in by_file.items():
        original_path, candidate_path = unadapted / file, candidate / file
        if not original_path.is_file() or not candidate_path.is_file():
            continue
        original, current = original_path.read_bytes(), candidate_path.read_bytes()
        output, reason = _replay_file(original, rows)
        if output == current:
            continue
        new_rows = [r for r in rows if id(r) in new_row_ids]
        prior_rows = [r for r in rows if id(r) not in new_row_ids]
        translated = False
        if new_rows:
            attempt = []
            for row in new_rows:
                start = _candidate_offset_to_original(row["before_span"]["start"], prior_rows)
                if start is None:
                    break
                removed_len = len(base64.b64decode(row["patch"]["removed_base64"]))
                attempt.append((row, {"start": start, "end": start + removed_len}))
            if len(attempt) == len(new_rows):
                saved = [(row, dict(row["before_span"])) for row, _ in attempt]
                for row, span in attempt:
                    row["before_span"] = span
                output2, _ = _replay_file(original, rows)
                if output2 == current:
                    translated = True
                    translated_rows.extend(row for row, _ in attempt)
                    print(
                        f"phase0: translated before_span of {len(attempt)} new {file} row(s) from candidate to "
                        "uploaded-file coordinates (replay now exact)", file=sys.stderr, flush=True,
                    )
                else:
                    for row, span in saved:
                        row["before_span"] = span
        if translated:
            continue
        if reason is None:
            # Removed bytes matched everywhere, yet the replay differs from the
            # candidate: the inserted text or its position is off.
            for row in sorted(rows, key=lambda r: r["before_span"]["start"]):
                inserted = base64.b64decode(row["patch"]["inserted_base64"])
                a = row["after_span"]
                if current[a["start"]:a["end"]] != inserted:
                    reason = (
                        f"after replaying all entries the candidate should hold the inserted bytes at "
                        f"{a['start']}..{a['end']} but holds {current[a['start']:a['end']][:80]!r}; "
                        "measure before_span in the uploaded file, or check the candidate edit matches the patch"
                    )
                    break
            reason = reason or "the replayed file differs from the candidate; an edit in the candidate is not described by any entry"
        raise Phase0DraftRejected(f"entries for {file!r} do not reproduce the candidate: {reason}")
    return translated_rows


def seal_worker_draft(
    draft: dict[str, Any], request: dict[str, Any], state: dict[str, Any], candidate: Path,
    discovery_receipts: list[dict[str, Any]] | None = None,
    unadapted: Path | None = None,
) -> dict[str, Any]:
    if set(draft) != {"schema", "request_id", "worker_thread_id", "entries"}:
        raise Phase0BridgeError("worker draft has missing or unknown fields")
    if draft["schema"] != WORKER_DRAFT_SCHEMA or draft["request_id"] != request["request_id"]:
        raise Phase0BridgeError("worker draft is stale")
    if not isinstance(draft["worker_thread_id"], str) or not draft["worker_thread_id"].strip():
        raise Phase0BridgeError("worker draft lacks a stable thread id")
    if not isinstance(draft["entries"], list):
        raise Phase0BridgeError("worker draft entries must be an array")
    generation = int(state["generation"]) + 1
    entries: list[dict[str, Any]] = []
    entry_fields = {
        "file", "operation", "before_span", "after_span", "before_sha256", "after_sha256",
        "patch", "quoted_error", "behavior_preservation_claim", "seam_class", "citation",
        "affected_targets", "meaning_change",
    }
    known_receipts: dict[str, dict[str, Any]] = {}
    # Kernel state first, and it wins.  Its receipts are the authoritative
    # bytes; the copy carried in the request is a prompt-sized VIEW of the
    # failing one, with the duplicated top-level stdout and the streams of
    # stages that passed removed (`inline_view` says so in the receipt).  A
    # quote resolved against that view would be checked against a stream the
    # kernel never captured, which is how a check goes silently inert.
    for receipt in state.get("checker_receipts", []) or []:
        # Kernel state stores receipts as evidence records: {receipt_sha256, receipt}.
        if isinstance(receipt, dict) and isinstance(receipt.get("receipt"), dict):
            receipt = {"receipt_sha256": receipt.get("receipt_sha256"), **receipt["receipt"]}
        if isinstance(receipt, dict) and isinstance(receipt.get("receipt_sha256"), str):
            known_receipts[receipt["receipt_sha256"]] = receipt
    for receipt in [request.get("checker_failure_receipt"), *(discovery_receipts or [])]:
        # The request carries the generation's own failure as an evidence
        # record, {receipt_sha256, receipt}, exactly as kernel state does.
        # Storing the wrapper leaves the quote checks with no stream to read,
        # and they return without checking anything.  These only fill gaps
        # kernel state does not already cover.
        if isinstance(receipt, dict) and isinstance(receipt.get("receipt"), dict):
            receipt = {"receipt_sha256": receipt.get("receipt_sha256"), **receipt["receipt"]}
        if isinstance(receipt, dict) and isinstance(receipt.get("receipt_sha256"), str):
            known_receipts.setdefault(receipt["receipt_sha256"], receipt)
    for raw in draft["entries"]:
        if not isinstance(raw, dict):
            raise Phase0BridgeError("worker draft contains a non-object entry")
        if set(raw) != entry_fields:
            raise Phase0BridgeError("worker draft entry has missing or unknown fields")
        entry = dict(raw)
        _check_entry_shapes(entry)
        patch = entry.get("patch")
        quote = entry.get("quoted_error")
        if not isinstance(patch, dict) or not isinstance(quote, dict):
            raise Phase0BridgeError("worker entry lacks patch or quoted checker error")
        if set(patch) != {"removed_base64", "inserted_base64"} or set(quote) != {
            "tool", "receipt_sha256", "stage_index", "stream", "byte_offset", "quote"
        }:
            raise Phase0BridgeError("worker patch or checker quote has unknown fields")
        try:
            removed = base64.b64decode(patch["removed_base64"], validate=True)
            inserted = base64.b64decode(patch["inserted_base64"], validate=True)
        except (KeyError, TypeError, ValueError) as exc:
            raise Phase0BridgeError("worker patch uses invalid base64") from exc
        _check_quote_is_a_complete_line(entry, known_receipts)
        patch["removed_sha256"] = _sha(removed)
        patch["inserted_sha256"] = _sha(inserted)
        quote["quote_sha256"] = _sha(str(quote.get("quote", "")).encode("utf-8"))
        quote["byte_length"] = len(str(quote.get("quote", "")).encode("utf-8"))
        entry["patch"] = patch
        entry["quoted_error"] = quote
        entry["discovery_failure_receipt_sha256"] = quote.get("receipt_sha256")
        entry["ablation_failure_receipt_sha256"] = ZERO
        entry["status"] = "draft"
        targets = entry.get("affected_targets")
        if not isinstance(targets, list) or not targets or not all(isinstance(t, str) for t in targets):
            raise Phase0DraftRejected(
                f"entry for {entry.get('file')!r}: affected_targets must be a non-empty list of GOAL target ids"
            )
        allowed = request.get("goal_targets")
        if isinstance(allowed, list):
            unknown = sorted(set(targets) - set(allowed))
            if unknown:
                raise Phase0DraftRejected(
                    f"entry for {entry.get('file')!r}: affected_targets names {unknown}, "
                    f"which are not in the request's goal_targets {sorted(allowed)}; use those ids exactly"
                )
        entry["affected_targets"] = sorted(set(targets), key=lambda x: x.encode("utf-8"))
        entry["diff_sha256"] = _diff_digest(entry)
        entry["id"] = f"phase0-{generation}-{entry['diff_sha256']}"
        entries.append(entry)
    # Every prior entry is carried forward (the ledger must account for the
    # whole candidate delta and the worker only describes what it changed),
    # except one the draft re-specifies: same file and same original span.
    # A second seam in an already-modified file is a new span, not a
    # replacement; whole-file digests are refreshed on carried modify entries
    # since the candidate file changed again.
    draft_entries = list(entries)
    respecified = {(entry["file"], entry["before_span"]["start"], entry["before_span"]["end"]) for entry in entries}
    for prior in state.get("ledger", {}).get("entries", []):
        key = (prior.get("file"), prior.get("before_span", {}).get("start"), prior.get("before_span", {}).get("end"))
        if key not in respecified:
            carried = dict(prior)
            # Entry ids embed the ledger generation (`phase0-<gen>-<diff>`);
            # the kernel recognizes a prior entry by its diff digest and
            # requires the id to match the ledger it now sits in.
            carried["id"] = f"phase0-{generation}-{carried['diff_sha256']}"
            entries.append(carried)
    _recompute_derived_fields(entries, state, candidate)
    if unadapted is not None:
        translated = _simulate_replay_or_reject(
            entries, unadapted, candidate, new_row_ids={id(e) for e in draft_entries}
        )
        for entry in translated:
            entry["diff_sha256"] = _diff_digest(entry)
            entry["id"] = f"phase0-{generation}-{entry['diff_sha256']}"
        _recompute_derived_fields(entries, state, candidate)
    entries.sort(
        key=lambda row: (
            row["file"].encode("utf-8"),
            row["before_span"]["start"],
            row["after_span"]["start"],
            row["operation"],
            row["id"].encode("utf-8"),
        )
    )
    stable = [
        {
            key: entry[key]
            for key in (
                "affected_targets", "after_sha256", "after_span", "before_sha256",
                "before_span", "behavior_preservation_claim", "citation", "diff_sha256",
                "discovery_failure_receipt_sha256", "file", "id", "meaning_change",
                "operation", "patch", "quoted_error", "seam_class",
            )
        }
        for entry in entries
    ]
    manifest = source_tree_manifest(candidate)
    ledger = {
        "schema": "trellis-phase0-adaptation-ledger/v1",
        "generation": generation,
        "unadapted_tree_sha256": state["genesis"]["unadapted_source_manifest"]["source_tree_sha256"],
        "adapted_tree_sha256": manifest["source_tree_sha256"],
        "goal_sha256": state["genesis"]["goal_sha256"],
        "entries": entries,
        "ledger_entries_root": tagged_hash("adaptation-ledger-row", canonical_json(stable)),
    }
    ledger["ledger_sha256"] = _self_digest(ledger, "ledger_sha256")
    # The worker identity the kernel tracks across generations is the
    # bridge's persistent lane, not the provider's per-burst thread (the
    # worker policy is always-fresh, so codex thread ids change every burst).
    worker_thread_id = state.get("worker_thread_id") or (
        f"phase0-worker-{str(request.get('worker_binding_sha256', ''))[:16] or 'lane'}"
    )
    response = {
        "schema": WORKER_RESPONSE_SCHEMA,
        "request_id": request["request_id"],
        "request_sha256": request["request_sha256"],
        "base_generation": state["generation"],
        "worker_thread_id": worker_thread_id,
        "candidate_manifest": manifest,
        "ledger": ledger,
        "discovery_receipts": list(discovery_receipts or []),
    }
    response["response_sha256"] = _self_digest(response, "response_sha256")
    return response


def seal_audit_result(draft: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    allowed = {
        "schema", "decision", "entry_verdicts", "whole_tree_verdict",
        "goal_binding_verdicts", "findings",
    }
    if set(draft) != allowed:
        raise Phase0BridgeError("audit draft has missing or unknown fields")
    if draft["schema"] != AUDIT_DRAFT_SCHEMA:
        raise Phase0BridgeError("audit draft has the wrong schema")
    result = dict(draft)
    result.pop("schema")
    result.update(
        {
            "schema": AUDIT_RESULT_SCHEMA,
            "request_id": request["request_id"],
            "request_sha256": request["request_sha256"],
            "scenario": request["scenario"],
            "generation": request["generation"],
            "genesis_sha256": request["genesis_sha256"],
            "semantic_bundle_sha256": request["semantic_bundle_sha256"],
            "ledger_sha256": request["ledger_sha256"],
            "lane_id": request["lane"]["lane_id"],
            "lane_binding_sha256": request["lane"]["binding_sha256"],
        }
    )
    result["entry_verdicts"] = sorted(result["entry_verdicts"], key=lambda row: row["entry_id"].encode("utf-8"))
    result["goal_binding_verdicts"] = sorted(
        result["goal_binding_verdicts"], key=lambda row: row["target_id"].encode("utf-8")
    )
    result["findings"] = sorted(
        result["findings"],
        key=lambda row: (
            row["code"].encode("utf-8"), str(row.get("entry_id") or "").encode("utf-8"),
            str(row.get("path") or "").encode("utf-8"), row.get("span_start") or 0,
            row.get("span_end") or 0,
        ),
    )
    result["result_sha256"] = _self_digest(result, "result_sha256")
    return result


def materialize_ablation(raw: Path, ledger: dict[str, Any], omit_id: str, destination: Path) -> None:
    """Materialize raw plus every exact patch except ``omit_id``."""
    _copy_tree(raw, destination)
    grouped: dict[str, list[dict[str, Any]]] = {}
    for entry in ledger["entries"]:
        if entry["id"] != omit_id:
            grouped.setdefault(entry["file"], []).append(entry)
    for relative, entries in grouped.items():
        path = destination / relative
        operation = entries[0]["operation"]
        if operation == "add":
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(base64.b64decode(entries[0]["patch"]["inserted_base64"]))
        elif operation == "delete":
            path.unlink()
        else:
            original = path.read_bytes()
            rebuilt = bytearray()
            cursor = 0
            for entry in sorted(entries, key=lambda row: row["before_span"]["start"]):
                start, end = entry["before_span"]["start"], entry["before_span"]["end"]
                rebuilt.extend(original[cursor:start])
                rebuilt.extend(base64.b64decode(entry["patch"]["inserted_base64"]))
                cursor = end
            rebuilt.extend(original[cursor:])
            path.write_bytes(bytes(rebuilt))


class Phase0Driver:
    """Deterministic orchestration with injectable worker and review lanes."""

    def __init__(
        self,
        *,
        kernel: KernelPhase0,
        broker: Phase0CheckerBroker,
        attempt_root: Path,
        worker: Callable[[dict[str, Any], dict[str, str]], dict[str, Any]],
        reviewer: Callable[[dict[str, Any]], dict[str, Any]],
    ):
        self.kernel = kernel
        self.broker = broker
        self.attempt = attempt_root.resolve()
        self.worker = worker
        self.reviewer = reviewer
        self.last_draft_rejection: str | None = None
        context = _read_json(kernel.context)
        self.candidate = Path(context["candidate_tree_path"]).resolve()
        self.raw = Path(context["unadapted_tree_path"]).resolve()
        self.goal_report_path = Path(context["goal_binding_report_path"]).resolve()
        self.goal_path = Path(broker.template["goal_path"]).resolve()
        self.agent_output = self.attempt / "agent-output"
        self.snapshots = kernel.authority / "worker-snapshots"
        self.sockets = kernel.authority / "sockets"
        self.worker_journal = kernel.authority / "active-worker-burst.json"
        for path in (self.agent_output, self.snapshots, self.sockets):
            path.mkdir(parents=True, exist_ok=True)
        if kernel.authority.is_relative_to(self.attempt):
            raise Phase0BridgeError("Phase-0 authority must be outside every agent write root")
        context = _read_json(kernel.context)
        for protected in (
            Path(context["ledger_path"]).resolve(),
            Path(context["receipt_store_path"]).resolve(),
            Path(context["goal_binding_report_path"]).resolve(),
        ):
            if protected.is_relative_to(self.attempt):
                raise Phase0BridgeError("Phase-0 evidence path is inside the agent attempt")
        if Path(context["receipt_store_path"]).resolve() != broker.receipts:
            raise Phase0BridgeError("validation context and checker receipt store differ")
        self.retained_final_result: dict[str, Any] | None = None
        state = _read_json(kernel.state)
        retained = state.get("extracted_model_path")
        if isinstance(retained, str) and retained:
            extracted = Path(retained).resolve()
            run_root = extracted.parent.parent
            self.retained_final_result = {
                "run_root": str(run_root),
                "frozen_tree": str(broker.freezes / run_root.name),
            }
        self._recover_interrupted_worker()

    def _release_retained_model(self) -> None:
        retained = getattr(self, "retained_final_result", None)
        if retained is not None:
            self.broker.cleanup(retained)
            self.retained_final_result = None

    def _retain_refused_draft(
        self, request: dict[str, Any], draft: Any, reason: str
    ) -> Path | None:
        """Keep a refused delivery next to that burst's retained logs.

        A bridge refusal writes no kernel event and the staged raw response is
        unlinked before the re-dispatch, so these are the only surviving bytes
        of what the worker actually delivered. Best effort: retaining them
        never changes the refusal."""
        try:
            destination = _burst_log_set_for(
                self.agent_output / "logs",
                str(request.get("request_id", "")),
                claim="refused-draft.json",
            )
            payload = {
                "schema": "trellis-phase0-refused-draft/v1",
                "request_id": str(request.get("request_id", "")),
                "reason": reason,
                "draft": draft,
            }
            body = json.dumps(payload, indent=2, sort_keys=True, default=str) + "\n"
            (destination / "refused-draft.json").write_text(body, encoding="utf-8")
            return destination
        except (OSError, TypeError, ValueError) as error:
            print(
                f"phase0: could not retain the refused draft: {error}", file=sys.stderr, flush=True
            )
            return None

    def _restore_snapshot(self, snapshot: Path) -> None:
        rejected = self.candidate.with_name(f"candidate.rejected.{secrets.token_hex(4)}")
        os.replace(self.candidate, rejected)
        try:
            _copy_tree(snapshot, self.candidate)
        finally:
            shutil.rmtree(rejected, ignore_errors=True)

    def _recover_interrupted_worker(self) -> None:
        if not self.worker_journal.exists():
            return
        journal = _read_json(self.worker_journal)
        if set(journal) != {
            "schema", "base_state_sha256", "candidate_path", "snapshot_path"
        } or journal["schema"] != "trellis-phase0-worker-journal/v1":
            raise Phase0BridgeError("active worker journal is malformed")
        snapshot = Path(journal["snapshot_path"]).resolve()
        if (
            Path(journal["candidate_path"]).resolve() != self.candidate
            or not snapshot.is_relative_to(self.snapshots)
            or not snapshot.is_dir()
        ):
            raise Phase0BridgeError("active worker journal escapes its authority paths")
        state = _read_json(self.kernel.state)
        if state["state_sha256"] == journal["base_state_sha256"]:
            self._restore_snapshot(snapshot)
        self.worker_journal.unlink()
        shutil.rmtree(snapshot, ignore_errors=True)

    def _worker_burst(self, request: dict[str, Any]) -> None:
        state = _read_json(self.kernel.state)
        snapshot = self.snapshots / f"generation-{state['generation']}-{secrets.token_hex(4)}"
        _copy_tree(self.candidate, snapshot)
        _write_replace(
            self.worker_journal,
            {
                "schema": "trellis-phase0-worker-journal/v1",
                "base_state_sha256": state["state_sha256"],
                "candidate_path": str(self.candidate),
                "snapshot_path": str(snapshot),
            },
        )
        draft: Any = None
        try:
            with self.broker.serve_worker(self.sockets / "phase0-checker.sock") as environment:
                extra: dict[str, Any] = {}
                if self.last_draft_rejection:
                    extra["rejection"] = self.last_draft_rejection
                history = render_revision_history(state)
                if history:
                    extra["revision_history"] = history
                draft = self.worker(request, environment, **extra)
            try:
                response = seal_worker_draft(
                    draft, request, state, self.candidate,
                    getattr(self.broker, "interactive_discovery_receipts", []),
                    unadapted=self.raw,
                )
            except Phase0BridgeError as exc:
                # Every seal failure is a defect of the delivered draft.
                raise Phase0DraftRejected(str(exc)) from exc
            except (TypeError, KeyError, ValueError, AttributeError) as exc:
                # A shape the checks above did not anticipate must still be
                # fed back to the worker, never crash the phase.
                raise Phase0DraftRejected(
                    f"worker draft has a malformed field ({type(exc).__name__}: {exc})"
                ) from exc
            try:
                self.kernel.apply(
                    {
                        "kind": "worker_revision",
                        "response": response,
                        "pre_burst_candidate_path": str(snapshot),
                    }
                )
            except Phase0KernelRejection as exc:
                # The kernel refused this revision; tell the worker why on
                # the re-dispatch instead of stopping the whole phase.
                raise Phase0DraftRejected(str(exc)) from exc
            self.last_draft_rejection = None
        except Phase0DraftRejected as exc:
            self.last_draft_rejection = str(exc)
            print(f"phase0: worker delivery rejected: {exc}", file=sys.stderr, flush=True)
            self._retain_refused_draft(request, draft, str(exc))
            self._restore_snapshot(snapshot)
            raise
        except Exception:
            self._restore_snapshot(snapshot)
            raise
        finally:
            self.worker_journal.unlink(missing_ok=True)
            shutil.rmtree(snapshot, ignore_errors=True)

    def _complete_checks(self, candidate_result: dict[str, Any]) -> None:
        final_result: dict[str, Any] | None = None
        try:
            final_result = self.broker.check("ratification_replay")
            if final_result["output"].get("status") != "passed":
                self.kernel.apply({"kind": "checker_failure", "receipt": final_result["receipt"]})
                return
            state = _read_json(self.kernel.state)
            ledger = state["ledger"]
            ablation_receipts: list[dict[str, Any]] = []
            completed_entries: list[dict[str, Any]] = []
            for entry in ledger["entries"]:
                ablated = self.kernel.authority / "ablations" / entry["id"]
                ablated.parent.mkdir(parents=True, exist_ok=True)
                materialize_ablation(self.raw, ledger, entry["id"], ablated)
                result: dict[str, Any] | None = None
                try:
                    result = self.broker.check(
                        "entry_ablation", candidate=ablated, reverted_entry_id=entry["id"]
                    )
                    receipt = result["receipt"]
                    ablation_receipts.append(receipt)
                    row = dict(entry)
                    row["ablation_failure_receipt_sha256"] = receipt["receipt_sha256"]
                    row["status"] = "audited"
                    completed_entries.append(row)
                finally:
                    shutil.rmtree(ablated, ignore_errors=True)
                    if result is not None:
                        self.broker.cleanup(result)
            completed = dict(ledger)
            completed["entries"] = completed_entries
            completed["ledger_sha256"] = _self_digest(completed, "ledger_sha256")
            run_id = final_result["output"]["check_id"]
            pv_tablet = (
                self.broker.runs
                / run_id
                / "extraction-writable"
                / "extracted"
                / "pv_tablet.json"
            )
            report = build_goal_binding_report(
                self.goal_path,
                pv_tablet,
                self.candidate,
                self.raw,
                completed,
                state["genesis"]["source_partition_manifest"],
                # The checker's own scan of the model it just produced: which
                # declarations it emitted without a definition. Reported per
                # GOAL target so a suppressed computation is visible where the
                # binding is read, not only in the receipt.
                final_result["output"].get("model_bodiless_declarations"),
            )
            transition = self.kernel.apply(
                {
                    "kind": "check_complete",
                    "candidate_success_receipt": candidate_result["receipt"],
                    "final_success_receipt": final_result["receipt"],
                    "ablation_receipts": ablation_receipts,
                    "completed_ledger": completed,
                    "goal_binding_report": report,
                    "extracted_model_path": str(pv_tablet.parent),
                }
            )
            if transition.get("stage") in {
                "seam_audit", "correspondence_audit"
            }:
                self.retained_final_result = final_result
                final_result = None
        finally:
            if final_result is not None:
                self.broker.cleanup(final_result)
            self.broker.cleanup(candidate_result)

    def step(self) -> dict[str, Any]:
        status = self.kernel.check()
        stage = status["stage"]
        if stage == "check":
            result = self.broker.check("candidate")
            if result["output"].get("status") == "passed":
                self._complete_checks(result)
            else:
                try:
                    self.kernel.apply({"kind": "checker_failure", "receipt": result["receipt"]})
                finally:
                    self.broker.cleanup(result)
        elif stage == "worker":
            envelope = self.kernel.next()
            self._worker_burst(envelope["request"])
            # An interactive pass never advances state.  The next loop always
            # performs an authoritative fresh freeze and supervisor check.
        elif stage in {"seam_audit", "correspondence_audit"}:
            envelope = self.kernel.next()
            if envelope["kind"] == "seal":
                sealed = self.kernel.authority / "sealed-generation.json"
                result = self.kernel.finalize(sealed)
                self._release_retained_model()
                return result
            request = envelope["request"]
            draft = self.reviewer(request)
            result = seal_audit_result(draft, request)
            self.kernel.apply({"kind": "audit_result", "request": request, "result": result})
            if self.kernel.check()["stage"] in {"worker", "incomplete"}:
                self._release_retained_model()
        return self.kernel.check()

    MAX_CONSECUTIVE_DRAFT_REJECTIONS = 5

    def run(self) -> dict[str, Any]:
        transport_retries = 0
        environmental_retries = 0
        draft_rejections = 0
        while True:
            status = self.kernel.check()
            if status["stage"] in {"sealed", "incomplete"}:
                self._release_retained_model()
                return status
            try:
                self.step()
                transport_retries = 0
                environmental_retries = 0
                draft_rejections = 0
            except Phase0KernelRejection:
                raise
            except Phase0DraftRejected as exc:
                # Worker-correctable: the same request goes back with the
                # reason. Bounded separately from transport failures, since
                # a worker that slips twice in a row is common and one that
                # cannot correct itself within five attempts is not going to.
                draft_rejections += 1
                print(
                    f"phase0: draft rejected, re-dispatching with reason "
                    f"({draft_rejections}/{self.MAX_CONSECUTIVE_DRAFT_REJECTIONS}): {exc}",
                    file=sys.stderr, flush=True,
                )
                if draft_rejections >= self.MAX_CONSECUTIVE_DRAFT_REJECTIONS:
                    raise
            except Phase0EnvironmentalFailure as exc:
                environmental_retries += 1
                print(
                    f"phase0: environmental checker failure ({environmental_retries}/3): {exc}",
                    file=sys.stderr, flush=True,
                )
                if environmental_retries >= 3:
                    raise
            except Phase0BridgeError as exc:
                transport_retries += 1
                # Never swallow a retried failure silently: the create log is
                # the only record the operator has while Phase 0 runs.
                print(
                    f"phase0: transport failure, re-dispatching ({transport_retries}/3): {exc}",
                    file=sys.stderr, flush=True,
                )
                if transport_retries >= 3:
                    raise


def dry_run_snapshot(attempt_root: Path, authority_root: Path) -> dict[str, Any]:
    """Machine-readable mount/write contract used by launcher integration."""
    attempt = attempt_root.resolve()
    authority = authority_root.resolve()
    return {
        "schema": "trellis-phase0-bridge-dry-run/v1",
        "worker": {
            "read_only": [str(attempt)],
            "writable": [
                str(attempt / "candidate"),
                str(attempt / "agent-output"),
                str(attempt / ".trellis" / "chats"),
            ],
            "checker_client": str(attempt / ".trellis" / "scripts" / "check.py"),
            "checker_rpc": str(authority / "sockets" / "phase0-checker.sock"),
            "persistent_home": str(attempt / "burst-homes" / "worker"),
            "direct_checker_tools": [],
        },
        "reviewer": {
            "read_only": [str(attempt), str(authority / "phase0")],
            "writable": [str(attempt / "agent-output"), str(attempt / ".trellis" / "chats")],
            "fresh": True,
        },
        "authority": {"root": str(authority), "agent_writable": False},
    }


def _pin_file(path: Path) -> dict[str, str]:
    resolved = path.resolve(strict=True)
    if not resolved.is_file() or resolved.is_symlink():
        raise Phase0BridgeError(f"pinned component is not a regular file: {resolved}")
    return {"path": str(resolved), "sha256": _sha(resolved.read_bytes())}


def _pin_tree(path: Path) -> dict[str, str]:
    resolved = path.resolve(strict=True)
    return {
        "path": str(resolved),
        "source_tree_sha256": readonly_closure_digest(resolved),
    }


def _checkout_tool(executable: Path) -> dict[str, Any]:
    resolved = executable.resolve(strict=True)
    completed = subprocess.run(
        ["git", "-C", str(resolved.parent), "rev-parse", "--show-toplevel"],
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode != 0:
        raise Phase0BridgeError(f"pinned tool is not in a Git checkout: {resolved}")
    root = Path(completed.stdout.strip()).resolve(strict=True)
    status = subprocess.run(
        ["git", "-C", str(root), "status", "--porcelain", "--untracked-files=all"],
        text=True,
        capture_output=True,
        check=False,
    )
    if status.returncode != 0 or status.stdout:
        raise Phase0BridgeError(f"pinned tool checkout is not clean: {root}")
    revision = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "HEAD"],
        text=True,
        capture_output=True,
        check=False,
    )
    if revision.returncode != 0 or len(revision.stdout.strip()) != 40:
        raise Phase0BridgeError(f"cannot resolve pinned tool revision: {root}")
    resolved_revision = revision.stdout.strip()
    return {
        "executable": _pin_file(resolved),
        "source": {
            "path": str(root),
            "source_tree_sha256": git_checkout_digest(root, resolved_revision),
        },
        "revision": resolved_revision,
    }


def _lane_binding(lane_id: str, value: dict[str, Any]) -> dict[str, Any]:
    binding = {
        "lane_id": lane_id,
        "provider": str(value.get("provider", "")),
        "model": str(value.get("model", "")),
        "effort": str(value.get("effort", "")),
        "extra_args": list(value.get("extra_args", [])),
        "fallback_models": list(value.get("fallback_models", [])),
        "binding_sha256": ZERO,
    }
    if not binding["provider"] or not binding["model"] or not binding["effort"]:
        raise Phase0BridgeError(f"lane {lane_id} lacks provider/model/effort")
    binding["binding_sha256"] = _self_digest(binding, "binding_sha256")
    return binding


def _load_phase0_bindings(config_path: Path) -> tuple[dict[str, Any], bool]:
    config = json.loads(config_path.read_bytes(), object_pairs_hook=_strict_object)
    if not isinstance(config, dict):
        raise Phase0BridgeError("Phase-0 lane config must be an object")
    worker = config.get("worker")
    reviewer = config.get("reviewer")
    verification = config.get("verification")
    correspondence = (
        verification.get("correspondence_agents")
        if isinstance(verification, dict)
        else None
    )
    if not isinstance(worker, dict) or not isinstance(reviewer, dict):
        raise Phase0BridgeError("Phase-0 lane config lacks worker or reviewer")
    if not isinstance(correspondence, list) or not correspondence or not all(
        isinstance(value, dict) for value in correspondence
    ):
        raise Phase0BridgeError("Phase-0 lane config lacks correspondence agents")
    phase0 = config.get("phase0")
    phase0_keys = {
        "allow_same_model_lanes",
        "adapting_worker",
        "seam_repair_auditor",
        "correspondence_agents",
    }
    if phase0 is not None and (
        not isinstance(phase0, dict)
        or not set(phase0).issubset(phase0_keys)
        or not isinstance(phase0.get("allow_same_model_lanes"), bool)
    ):
        raise Phase0BridgeError(
            "phase0 config has unknown fields or lacks boolean allow_same_model_lanes"
        )
    explicit_phase0 = isinstance(phase0, dict) and any(
        key in phase0
        for key in {
            "adapting_worker", "seam_repair_auditor", "correspondence_agents"
        }
    )
    if explicit_phase0:
        phase0_worker = phase0.get("adapting_worker")
        seam_repair = phase0.get("seam_repair_auditor")
        phase0_correspondence = phase0.get("correspondence_agents")
        if not isinstance(phase0_worker, dict) or not isinstance(seam_repair, dict):
            raise Phase0BridgeError(
                "explicit phase0 config lacks adapting_worker or seam_repair_auditor"
            )
        if (
            not isinstance(phase0_correspondence, list)
            or not phase0_correspondence
            or not all(isinstance(value, dict) for value in phase0_correspondence)
        ):
            raise Phase0BridgeError(
                "explicit phase0 config lacks correspondence_agents"
            )
    else:
        # Compatibility for existing hand-authored lane configs: before the PV
        # launcher exposed Phase-0 controls these roles inherited the in-run
        # worker, reviewer, and correspondence pool.
        phase0_worker = worker
        seam_repair = reviewer
        phase0_correspondence = correspondence
    bindings = {
        "worker": _lane_binding("worker", phase0_worker),
        "seam_repair": [_lane_binding("seam-repair-0", seam_repair)],
        "source_correspondence": [
            _lane_binding(f"source-correspondence-{index}", value)
            for index, value in enumerate(phase0_correspondence)
        ],
    }
    return bindings, bool(phase0 and phase0["allow_same_model_lanes"])


def _command_identity(command: list[str]) -> str:
    completed = subprocess.run(command, text=True, capture_output=True, check=False)
    if completed.returncode != 0:
        raise Phase0BridgeError(
            f"identity command failed ({completed.returncode}): {' '.join(command)}"
        )
    identity = (completed.stdout + completed.stderr).strip()
    if not identity:
        raise Phase0BridgeError(f"identity command returned no text: {' '.join(command)}")
    return identity


def _resolve_rust_tool(name: str) -> Path:
    raw = shutil.which(name)
    if raw is None:
        raise Phase0BridgeError(f"{name} must be available before Phase 0")
    candidate = Path(raw).resolve(strict=True)
    if candidate.name != "rustup":
        return candidate
    rustup = shutil.which("rustup")
    if rustup is None:
        raise Phase0BridgeError(f"the {name} proxy resolves to rustup, but rustup is unavailable")
    completed = subprocess.run(
        [rustup, "which", name], text=True, capture_output=True, check=False
    )
    if completed.returncode != 0 or not completed.stdout.strip():
        raise Phase0BridgeError(
            f"rustup could not resolve the pinned {name} executable: "
            + completed.stderr.strip()
        )
    resolved = Path(completed.stdout.strip()).resolve(strict=True)
    if not resolved.is_file() or resolved.is_symlink():
        raise Phase0BridgeError(f"rustup resolved {name} to a non-regular file: {resolved}")
    return resolved


def _charon_toolchain_root(charon: Path) -> Path:
    """Resolve exactly the compiler closure selected by Charon itself."""

    completed = subprocess.run(
        [str(charon.resolve(strict=True)), "toolchain-path"],
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode != 0 or not completed.stdout.strip():
        raise Phase0BridgeError(
            "Charon did not identify its selected Rust toolchain: "
            + completed.stderr.strip()
        )
    root = Path(completed.stdout.strip()).resolve(strict=True)
    if not root.is_dir() or root.is_symlink() or not (root / "bin/rustc").is_file():
        raise Phase0BridgeError("Charon's selected Rust toolchain is incomplete")
    return root


def _resolve_opam_switch_environment(
    opam_root: Path, opam_switch: str
) -> tuple[Path, dict[str, str]]:
    """Resolve the small OCaml switch environment once, outside the sandbox."""

    root = opam_root.resolve(strict=True)
    opam = shutil.which("opam")
    if opam is None:
        raise Phase0BridgeError("opam must be available before Phase 0")
    host_environment = dict(os.environ)
    host_environment["OPAMROOT"] = str(root)
    completed = subprocess.run(
        [opam, "env", f"--switch={opam_switch}", "--set-switch", "--sexp"],
        env=host_environment,
        text=True,
        capture_output=True,
        check=False,
    )
    if completed.returncode != 0:
        raise Phase0BridgeError(
            f"opam env --switch={opam_switch} failed ({completed.returncode}): "
            + completed.stderr.strip()
        )
    parsed: dict[str, str] = {}
    for key, escaped in re.findall(
        r'\("([^"]+)"\s+"((?:[^"\\]|\\.)*)"\)', completed.stdout
    ):
        try:
            parsed[key] = json.loads(f'"{escaped}"')
        except json.JSONDecodeError as exc:
            raise Phase0BridgeError(f"opam env returned invalid escaping for {key}") from exc
    missing = OPAM_SWITCH_ENVIRONMENT_KEYS - parsed.keys()
    if missing:
        raise Phase0BridgeError(
            "opam env omitted required switch variables: " + ", ".join(sorted(missing))
        )
    sealed = {key: parsed[key] for key in sorted(OPAM_SWITCH_ENVIRONMENT_KEYS)}
    prefix_raw = sealed["OPAM_SWITCH_PREFIX"]
    try:
        prefix = Path(prefix_raw).resolve(strict=True)
    except OSError as exc:
        raise Phase0BridgeError(f"opam switch prefix is unavailable: {prefix_raw}") from exc
    if not prefix.is_dir() or prefix.is_symlink() or not (prefix / "bin").is_dir():
        raise Phase0BridgeError(f"opam switch prefix is incomplete: {prefix}")
    if prefix_raw != str(prefix):
        raise Phase0BridgeError("opam switch prefix is not a normalized absolute path")
    if sealed["OPAMSWITCH"] != opam_switch:
        raise Phase0BridgeError("opam env selected a different switch")
    for key in sorted(OPAM_SWITCH_PATH_KEYS):
        values = sealed[key].split(os.pathsep)
        if not values or any(not value for value in values):
            raise Phase0BridgeError(f"opam switch variable {key} has an empty path")
        for value in values:
            try:
                resolved = Path(value).resolve(strict=True)
            except OSError as exc:
                raise Phase0BridgeError(
                    f"opam switch variable {key} names an unavailable path: {value}"
                ) from exc
            if not resolved.is_relative_to(prefix):
                raise Phase0BridgeError(
                    f"opam switch variable {key} escapes the switch prefix: {value}"
                )
    return prefix, sealed


def _initial_ledger(raw_manifest: dict[str, Any], goal_sha256: str) -> dict[str, Any]:
    ledger = {
        "schema": "trellis-phase0-adaptation-ledger/v1",
        "generation": 0,
        "unadapted_tree_sha256": raw_manifest["source_tree_sha256"],
        "adapted_tree_sha256": raw_manifest["source_tree_sha256"],
        "goal_sha256": goal_sha256,
        "entries": [],
        "ledger_entries_root": tagged_hash("adaptation-ledger-row", canonical_json([])),
        "ledger_sha256": ZERO,
    }
    ledger["ledger_sha256"] = _self_digest(ledger, "ledger_sha256")
    return ledger


def run_phase0(args: argparse.Namespace) -> dict[str, Any]:
    """Construct genesis and drive the production Phase-0 state machine."""

    raw = args.unadapted_tree.resolve(strict=True)
    goal = args.goal.resolve(strict=True)
    target_archive = args.target_archive.resolve(strict=True)
    supporting_archive = (
        args.supporting_archive.resolve(strict=True) if args.supporting_archive else None
    )
    partition_path = args.partition_manifest.resolve(strict=True)
    attempt = args.attempt_root.resolve(strict=True)
    authority = args.authority_root.resolve()
    if authority.exists():
        raise Phase0BridgeError(f"refusing to reuse Phase-0 authority: {authority}")
    authority.mkdir(parents=True)
    candidate = attempt / "candidate"
    _copy_tree(raw, candidate)
    raw_manifest = source_tree_manifest(raw)
    try:
        partition_manifest = _read_json(partition_path)
        validate_source_partition_manifest(partition_manifest, raw_manifest)
    except (OSError, ValueError) as exc:
        raise Phase0BridgeError(f"invalid source partition manifest: {exc}") from exc
    goal_bytes = goal.read_bytes()
    try:
        goal_bytes.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise Phase0BridgeError(f"GOAL is not UTF-8: {exc}") from exc
    parsed_goal = parse_goal(goal)
    target_ids = sorted(
        (f"goal:{row['name']}" for row in parsed_goal["targets"]),
        key=lambda value: value.encode("utf-8"),
    )
    if not target_ids:
        raise Phase0BridgeError("Phase 0 requires at least one GOAL target")

    kernel_cli = args.kernel_cli.resolve(strict=True)
    controller = args.controller.resolve(strict=True)
    bridge = Path(__file__).resolve(strict=True)
    checker_runner = (REPO_ROOT / "scripts/pv_phase0_checker_runner.py").resolve(strict=True)
    extractor = (REPO_ROOT / "scripts/extract_pv_model.py").resolve(strict=True)
    filespec_checker = args.filespec_checker.resolve(strict=True)
    prescribed_region_checker = args.prescribed_region_checker.resolve(strict=True)
    if any(
        not checker.is_file() or not os.access(checker, os.X_OK)
        for checker in (filespec_checker, prescribed_region_checker)
    ):
        raise Phase0BridgeError("Phase-0 kernel gate checkers must be executable files")
    cargo = _resolve_rust_tool("cargo")
    rustc = _resolve_rust_tool("rustc")
    toolchain_root = Path(os.path.commonpath([cargo, rustc])).parent
    while not (toolchain_root / "bin").is_dir() and toolchain_root != toolchain_root.parent:
        toolchain_root = toolchain_root.parent
    if not cargo.is_relative_to(toolchain_root) or not rustc.is_relative_to(toolchain_root):
        raise Phase0BridgeError("Cargo and rustc do not share a pinned toolchain root")
    opam_switch_prefix, opam_switch_environment = _resolve_opam_switch_environment(
        args.opam_root, args.opam_switch
    )

    components = {
        "kernel": _pin_file(kernel_cli),
        "controller": _pin_file(controller),
        "bridge": _pin_file(bridge),
        "checker_broker": _pin_file(bridge),
        "checker_runner": _pin_file(checker_runner),
        "extractor": _pin_file(extractor),
        "filespec_checker": _pin_file(filespec_checker),
        "prescribed_region_checker": _pin_file(prescribed_region_checker),
        "production_extraction_runner": _pin_file(
            REPO_ROOT / "scripts/pv_source_adaptation_sandbox.py"
        ),
        "worker_fragment": _pin_file(
            REPO_ROOT / "trellis/prompt_fragments/pv/source_adaptation/worker/05_role.md"
        ),
        "seam_audit_fragment": _pin_file(
            REPO_ROOT
            / "trellis/prompt_fragments/pv/source_adaptation/verifier/deviation/05_role.md"
        ),
        "correspondence_audit_fragment": _pin_file(
            REPO_ROOT
            / "trellis/prompt_fragments/pv/source_adaptation/verifier/correspondence/05_role.md"
        ),
    }
    closed_environment = {"LANG": "C.UTF-8", "LC_ALL": "C.UTF-8"}
    if os.environ.get("RUSTUP_TOOLCHAIN"):
        closed_environment["RUSTUP_TOOLCHAIN"] = os.environ["RUSTUP_TOOLCHAIN"]
    charon = _checkout_tool(args.charon_bin)
    charon_toolchain_root = _charon_toolchain_root(args.charon_bin)
    toolchain = {
        "cargo": _pin_file(cargo),
        "cargo_identity": _command_identity([str(cargo), "--version", "--verbose"]),
        "rustc": _pin_file(rustc),
        "rustc_identity": _command_identity([str(rustc), "-vV"]),
        "toolchain_root": str(toolchain_root),
        "charon_toolchain": _pin_tree(charon_toolchain_root),
        "charon": charon,
        "aeneas": _checkout_tool(args.aeneas_bin),
        "dependency_cache": None,
        "opam_switch_prefix": _pin_tree(opam_switch_prefix),
        "opam_switch_environment": opam_switch_environment,
        "opam_switch": args.opam_switch,
        "target_triple": args.target_triple,
        "extraction_profile": args.extraction_profile,
        "closed_environment": closed_environment,
    }
    bindings, allow_same_model_lanes = _load_phase0_bindings(
        args.lane_config.resolve(strict=True)
    )
    goal_binding_reporter = (
        REPO_ROOT / "scripts/pv_goal_binding.py"
    ).resolve(strict=True)
    components["repository_root"] = str(REPO_ROOT.resolve(strict=True))
    components["goal_binding_reporter"] = _pin_file(goal_binding_reporter)
    # The checker runner imports this to derive the model's bodiless
    # declarations, a fact the gate reads, so it is pinned like the runner.
    components["model_opacity_scanner"] = _pin_file(
        (REPO_ROOT / "scripts/pv_model_opacity.py").resolve(strict=True)
    )
    genesis = {
        "schema": "trellis-phase0-genesis/v1",
        "target_upload_sha256": _sha(target_archive.read_bytes()),
        "supporting_upload_sha256": (
            _sha(supporting_archive.read_bytes()) if supporting_archive else None
        ),
        "unadapted_source_manifest": raw_manifest,
        "source_partition_manifest": partition_manifest,
        "goal_base64": base64.b64encode(goal_bytes).decode("ascii"),
        "goal_sha256": _sha(goal_bytes),
        "policy": {
            "max_phase0_handbacks": 32,
            "allow_same_model_lanes": allow_same_model_lanes,
        },
        "bindings": bindings,
        "prompt_contract_version": PHASE0_PROMPT_CONTRACT_VERSION,
        "components": components,
        "toolchain": toolchain,
        "genesis_sha256": ZERO,
    }
    genesis["genesis_sha256"] = _self_digest(genesis, "genesis_sha256")
    ledger = _initial_ledger(raw_manifest, genesis["goal_sha256"])
    receipts = authority / "receipts"
    receipts.mkdir()
    context = {
        "schema": "trellis-phase0-validation-context/v1",
        "unadapted_tree_path": str(raw),
        "candidate_tree_path": str(candidate.resolve()),
        "goal_targets": target_ids,
        "ledger_path": str((authority / "adaptation-ledger.json").resolve()),
        "receipt_store_path": str(receipts.resolve()),
        "goal_binding_report_path": str((authority / "goal-binding-report.json").resolve()),
    }
    template = {
        "schema": "trellis-pv-phase0-checker-request/v1",
        "phase0_genesis_sha256": genesis["genesis_sha256"],
        "check_id": "genesis-placeholder",
        "check_kind": "candidate",
        "ledger_generation": 0,
        "ledger_root_sha256": ledger["ledger_entries_root"],
        "candidate_root": str(candidate.resolve()),
        "candidate_manifest": raw_manifest,
        "candidate_tree_sha256": raw_manifest["source_tree_sha256"],
        "repository_root": components["repository_root"],
        "goal_path": str(goal),
        "goal_sha256": genesis["goal_sha256"],
        "reverted_entry_id": None,
        "runner_path": str(checker_runner),
        "runner_sha256": components["checker_runner"]["sha256"],
        "extractor_path": str(extractor),
        "extractor_sha256": components["extractor"]["sha256"],
        "filespec_checker_path": str(filespec_checker),
        "filespec_checker_sha256": components["filespec_checker"]["sha256"],
        "prescribed_region_checker_path": str(prescribed_region_checker),
        "prescribed_region_checker_sha256": components["prescribed_region_checker"]["sha256"],
        "cargo_path": str(cargo),
        "cargo_sha256": toolchain["cargo"]["sha256"],
        "rustc_path": str(rustc),
        "rustc_sha256": toolchain["rustc"]["sha256"],
        "toolchain_root": str(toolchain_root),
        "charon_toolchain_root": toolchain["charon_toolchain"]["path"],
        "charon_toolchain_tree_sha256": toolchain["charon_toolchain"]["source_tree_sha256"],
        "charon_path": toolchain["charon"]["executable"]["path"],
        "charon_sha256": toolchain["charon"]["executable"]["sha256"],
        "charon_source_root": toolchain["charon"]["source"]["path"],
        "charon_source_tree_sha256": toolchain["charon"]["source"]["source_tree_sha256"],
        "aeneas_path": toolchain["aeneas"]["executable"]["path"],
        "aeneas_sha256": toolchain["aeneas"]["executable"]["sha256"],
        "aeneas_source_root": toolchain["aeneas"]["source"]["path"],
        "aeneas_source_tree_sha256": toolchain["aeneas"]["source"]["source_tree_sha256"],
        "dependency_cache_path": "",
        "dependency_cache_tree_sha256": _sha(b""),
        "opam_switch_prefix": str(opam_switch_prefix),
        "opam_switch_prefix_tree_sha256": toolchain["opam_switch_prefix"]["source_tree_sha256"],
        "opam_switch_environment": opam_switch_environment,
        "output_root": str((authority / "checker-runs").resolve()),
        "output_root_binding_sha256": _sha(str((authority / "checker-runs").resolve()).encode()),
        "target_triple": args.target_triple,
        "extraction_profile": args.extraction_profile,
        "opam_switch": args.opam_switch,
        "closed_environment": closed_environment,
        "run_id": "genesis-placeholder",
        "limits": {
            "timeout_seconds": 3600,
            "cpu_seconds": 3600,
            "address_space_bytes": 64 * 1024 * 1024 * 1024,
            "file_size_bytes": 4 * 1024 * 1024 * 1024,
            "process_count": 4096,
            "stdout_bytes": 1024 * 1024,
            "stderr_bytes": 1024 * 1024,
        },
    }
    genesis_path = authority / "genesis.json"
    identity_path = authority / "identity-ledger.json"
    context_path = authority / "context.json"
    template_path = authority / "checker-request-template.json"
    state_path = authority / "state.json"
    _write_new(genesis_path, genesis)
    _write_new(authority / "source-tree-manifest.json", raw_manifest)
    _write_new(authority / "source-partition-manifest.json", partition_manifest)
    _write_new(identity_path, ledger)
    _write_new(context_path, context)
    _write_new(template_path, template)
    initialized = subprocess.run(
        [
            str(kernel_cli), "phase0-init", str(genesis_path), str(identity_path),
            str(context_path), str(state_path),
        ],
        cwd=authority,
        capture_output=True,
        check=False,
    )
    if initialized.returncode != 0:
        raise Phase0BridgeError(
            "kernel rejected Phase-0 genesis: "
            + initialized.stderr.decode("utf-8", errors="replace").strip()
        )
    kernel = KernelPhase0(kernel_cli, state_path, context_path, authority)
    broker = Phase0CheckerBroker(
        kernel_cli=kernel_cli,
        checker_runner=checker_runner,
        working_directory=authority,
        request_template=template_path,
        authority_root=authority,
        state_path=state_path,
        candidate_path=candidate,
        interactive_candidate_root=attempt
        / "agent-output"
        / "logs"
        / "interactive-candidates",
    )
    dispatcher = ExistingLaneDispatcher(attempt, genesis)
    status = Phase0Driver(
        kernel=kernel,
        broker=broker,
        attempt_root=attempt,
        worker=dispatcher.worker,
        reviewer=dispatcher.reviewer,
    ).run()
    if status["stage"] != "sealed":
        raise Phase0BridgeError(
            f"Phase 0 ended without a clean sealed generation: {status['stage']}"
        )
    sealed_path = authority / "sealed-generation.json"
    if not sealed_path.is_file():
        raise Phase0BridgeError("Phase 0 reported sealed without a sealed-generation record")
    return {
        "schema": "trellis-phase0-launch-result/v1",
        "status": "sealed",
        "genesis_path": str(genesis_path),
        "sealed_generation_path": str(sealed_path),
        "ledger_path": context["ledger_path"],
        "goal_targets_path": str(authority / "goal-targets.json"),
        "source_tree_manifest_path": str(authority / "source-tree-manifest.json"),
        "source_partition_manifest_path": str(
            authority / "source-partition-manifest.json"
        ),
        "candidate_tree_path": str(candidate.resolve()),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--attempt-root", required=True, type=Path)
    parser.add_argument("--authority-root", required=True, type=Path)
    parser.add_argument("--target-archive", type=Path)
    parser.add_argument("--supporting-archive", type=Path)
    parser.add_argument("--partition-manifest", type=Path)
    parser.add_argument("--unadapted-tree", type=Path)
    parser.add_argument("--goal", type=Path)
    parser.add_argument("--kernel-cli", type=Path)
    parser.add_argument("--controller", type=Path)
    parser.add_argument("--filespec-checker", type=Path)
    parser.add_argument("--prescribed-region-checker", type=Path)
    parser.add_argument("--lane-config", type=Path, default=REPO_ROOT / "examples/trellis.config.json")
    parser.add_argument("--charon-bin", type=Path, default=Path("/path/to/trellis/pv_spike/tools/charon/bin/charon"))
    parser.add_argument("--aeneas-bin", type=Path, default=Path("/path/to/trellis/pv_spike/tools/aeneas/bin/aeneas"))
    parser.add_argument(
        "--opam-root",
        type=Path,
        default=Path(os.environ.get("OPAMROOT", Path.home() / ".opam")),
        help="host opam root used once to resolve the sealed switch environment",
    )
    parser.add_argument("--opam-switch", default="5.3.0")
    parser.add_argument("--target-triple", default="x86_64-unknown-linux-gnu")
    parser.add_argument("--extraction-profile", default="dev")
    parser.add_argument("--result-out", type=Path)
    args = parser.parse_args(argv)
    if args.dry_run == args.run:
        parser.error("select exactly one of --dry-run or --run")
    if args.dry_run:
        output = dry_run_snapshot(args.attempt_root, args.authority_root)
    else:
        required = {
            "target_archive": args.target_archive,
            "partition_manifest": args.partition_manifest,
            "unadapted_tree": args.unadapted_tree,
            "goal": args.goal,
            "kernel_cli": args.kernel_cli,
            "controller": args.controller,
            "filespec_checker": args.filespec_checker,
            "prescribed_region_checker": args.prescribed_region_checker,
        }
        missing = [name.replace("_", "-") for name, value in required.items() if value is None]
        if missing:
            parser.error("--run requires " + ", ".join(f"--{name}" for name in missing))
        output = run_phase0(args)
        targets = {
            "schema": "trellis-phase0-goal-target-set/v1",
            "targets": _read_json(Path(output["sealed_generation_path"]))["semantic_bundle"]
            ["goal_binding_report"]["targets"],
        }
        targets["targets"] = [row["target_id"] for row in targets["targets"]]
        _write_new(Path(output["goal_targets_path"]), targets)
    if args.result_out is not None:
        _write_new(args.result_out.resolve(), output)
    print(canonical_json(output).decode("utf-8"), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

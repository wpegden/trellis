#!/usr/bin/env python3
"""Differentially replay a checkpointed Trellis event history.

The input project is never checked out or modified.  The caller supplies a
*copy* of its Git directory plus the read-only event-log directory.  For each
event after the first available checkpoint this harness:

1. runs the baseline kernel and checks its commands against the transcript;
2. runs the candidate from the identical recorded/baseline state, isolating
   direct behavioral changes;
3. carries candidate-only state changes across later checkpoints and checks
   for downstream command changes; and
4. re-anchors the baseline at every recorded checkpoint, incorporating the
   runtime-owned effects which a pure kernel invocation cannot reproduce.

It invokes only the filesystem-free ``history_replay_driver`` example. It
does not run Lean, a checker, bridge, checkpoint hook, or a project Git
command against the source repo.
"""

from __future__ import annotations

import argparse
import base64
import collections
import copy
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Iterator


REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT))

from trellis.history_artifacts import decode_shared_state  # noqa: E402
from trellis.dispatch.history import checkpoint_requests, resolve, DIRECTORY  # noqa: E402


CHECKPOINT_RE = re.compile(r"\bcheckpoint\s+(\d+)\b")
HISTORY_STATE_PATH = ".trellis-history/supervisor_state.json"
MAX_SAMPLES = 12


class ReplayFailure(RuntimeError):
    """The inputs or a kernel result cannot support a sound comparison."""


def run_checked(command: list[str], *, input_text: str | None = None) -> str:
    completed = subprocess.run(
        command,
        input=input_text,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise ReplayFailure(
            f"command exited {completed.returncode}: {command!r}: {detail}"
        )
    return completed.stdout


def checkpoint_commits(git_dir: Path) -> dict[int, str]:
    raw = run_checked(
        [
            "git",
            f"--git-dir={git_dir}",
            "log",
            "--first-parent",
            "--format=%H%x09%s",
            "HEAD",
        ]
    )
    result: dict[int, str] = {}
    for line in raw.splitlines():
        sha, separator, subject = line.partition("\t")
        if not separator:
            continue
        match = CHECKPOINT_RE.search(subject)
        if match is None:
            continue
        index = int(match.group(1))
        if index in result:
            raise ReplayFailure(f"duplicate checkpoint index {index}")
        result[index] = sha
    if not result:
        raise ReplayFailure(f"no checkpoint commits found in {git_dir}")
    return result


def request_object(git_dir: Path, revision: str, path: str) -> bytes:
    completed = subprocess.run(['git', f'--git-dir={git_dir}', 'cat-file', 'blob', f'{revision}:{path}'], capture_output=True)
    if completed.returncode:
        raise ReplayFailure(f'cannot read committed request {revision}:{path}: {completed.stderr[:500]!r}')
    return completed.stdout


def checkpoint_document(git_dir: Path, sha: str) -> dict[str, Any]:
    raw = run_checked(
        ["git", f"--git-dir={git_dir}", "show", f"{sha}:{HISTORY_STATE_PATH}"]
    )
    try:
        document = checkpoint_requests(decode_shared_state(json.loads(raw)), git_dir,
            read_object=lambda path: request_object(git_dir, sha, path))
    except (KeyError, TypeError, ValueError) as exc:
        raise ReplayFailure(f"cannot decode state at {sha}: {exc}") from exc
    if not isinstance(document, dict):
        raise ReplayFailure(f"decoded checkpoint document at {sha} is not an object")
    return document


def checkpoint_state(git_dir: Path, sha: str) -> dict[str, Any]:
    document = checkpoint_document(git_dir, sha)
    try:
        state = document["state"]
    except KeyError as exc:
        raise ReplayFailure(f"checkpoint document at {sha} has no state") from exc
    if not isinstance(state, dict):
        raise ReplayFailure(f"decoded state at {sha} is not an object")
    return state


def event_rows(event_dir: Path) -> Iterator[tuple[Path, int, dict[str, Any]]]:
    paths = sorted(event_dir.glob("cycle-*.jsonl"))
    if not paths:
        raise ReplayFailure(f"no cycle-*.jsonl files found in {event_dir}")
    for path in paths:
        with path.open(encoding="utf-8") as stream:
            for line_number, line in enumerate(stream, 1):
                try:
                    row = json.loads(line)
                except json.JSONDecodeError as exc:
                    raise ReplayFailure(f"{path}:{line_number}: {exc}") from exc
                if not isinstance(row, dict):
                    raise ReplayFailure(f"{path}:{line_number}: row is not an object")
                yield path, line_number, row


def hydrate_dispatch_event(git_dir: Path, event: dict) -> dict[str, str]:
    archives = {}
    if event.get('event') == 'rebind_exclusive_checkpoint':
        raise ReplayFailure("exclusive checkpoint recovery requires native replay with Git evidence")
    if event.get('event') == 'dispatch':
        payload = event['payload']
        assignments = list(payload.get('assignments', []))
        assignments += [item['assignment'] for item in payload.get('items', []) if item.get('preparation') == 'ready']
        if payload.get('assignment') is not None:
            assignments.append(payload['assignment'])
        for assignment in assignments:
            request = assignment['request']
            if '$stored_dispatch_request' in request:
                assignment['request'] = resolve(git_dir, request['$stored_dispatch_request'],
                    read_object=lambda path: request_object(git_dir, 'HEAD', path))
        if payload.get('kind') == 'requests_archived':
            for reference in payload['requests'].values():
                name = f"{DIRECTORY}/{reference['sha256']}.zip"
                raw = request_object(git_dir, 'HEAD', name)
                # The filesystem-free driver independently verifies these
                # exact compressed bytes before constructing its carrier.
                archives[reference['sha256']] = base64.b64encode(raw).decode('ascii')
    return archives


class KernelDriver:
    def __init__(self, binary: Path) -> None:
        self.binary = binary
        self.process = subprocess.Popen(
            [str(binary)],
            text=True,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            # Historical states can emit one tolerated-legacy note per
            # re-anchor.  Do not let an unread diagnostic pipe back-pressure
            # the driver during a long replay.
            stderr=subprocess.DEVNULL,
            bufsize=1,
        )

    def request(self, payload: dict[str, Any]) -> dict[str, Any]:
        if self.process.stdin is None or self.process.stdout is None:
            raise ReplayFailure(f"driver pipes unavailable for {self.binary}")
        self.process.stdin.write(
            json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n"
        )
        self.process.stdin.flush()
        raw = self.process.stdout.readline()
        if not raw:
            raise ReplayFailure(
                f"driver {self.binary} exited unexpectedly: {self.process.poll()}"
            )
        try:
            response = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise ReplayFailure(f"driver {self.binary} emitted non-JSON: {raw[:500]!r}") from exc
        return response

    def set_state(self, state: dict[str, Any]) -> None:
        response = self.request({"action": "set_state", "state": state})
        if response.get("status") != "ready":
            raise ReplayFailure(f"driver {self.binary} refused state: {response!r}")

    def apply_event(self, event: Any, archives=None) -> dict[str, Any]:
        response = self.request({"action": "apply_event", "event": event,
                                 "dispatch_request_archives": archives or {}})
        if response.get("status") != "success":
            raise ReplayFailure(f"driver {self.binary} rejected event: {response!r}")
        if not isinstance(response.get("commands"), list) or not isinstance(
            response.get("state_sha256"), str
        ):
            raise ReplayFailure(f"driver {self.binary} returned an invalid success envelope")
        return response

    def get_state(self) -> dict[str, Any]:
        response = self.request({"action": "get_state"})
        if response.get("status") != "state" or not isinstance(
            response.get("state"), dict
        ):
            raise ReplayFailure(f"driver {self.binary} refused get_state: {response!r}")
        return response["state"]

    def close(self) -> None:
        if self.process.stdin is not None:
            self.process.stdin.close()
        try:
            return_code = self.process.wait(timeout=10)
        except subprocess.TimeoutExpired as exc:
            self.process.terminate()
            raise ReplayFailure(f"driver {self.binary} did not stop at EOF") from exc
        if return_code != 0:
            raise ReplayFailure(f"driver {self.binary} exited {return_code}")


def top_level_differences(left: dict[str, Any], right: dict[str, Any]) -> list[str]:
    return sorted(
        key
        for key in left.keys() | right.keys()
        if left.get(key, object()) != right.get(key, object())
    )


def difference_paths(left: Any, right: Any, prefix: str = "") -> list[str]:
    """Return exact differing JSON paths without dumping request payloads."""
    if type(left) is not type(right):
        return [prefix or "$"]
    if isinstance(left, dict):
        paths: list[str] = []
        for key in sorted(left.keys() | right.keys()):
            path = f"{prefix}.{key}" if prefix else key
            if key not in left or key not in right:
                paths.append(path)
            else:
                paths.extend(difference_paths(left[key], right[key], path))
        return paths
    if isinstance(left, list):
        paths = []
        for offset in range(max(len(left), len(right))):
            path = f"{prefix}[{offset}]"
            if offset >= len(left) or offset >= len(right):
                paths.append(path)
            else:
                paths.extend(difference_paths(left[offset], right[offset], path))
        return paths
    return [] if left == right else [prefix or "$"]


def without_acceptance_logic_identity(value: Any) -> Any:
    """Project away the intentionally source-versioned request identity."""
    if isinstance(value, dict):
        return {
            key: without_acceptance_logic_identity(item)
            for key, item in value.items()
            if key != "acceptance_logic_identity"
        }
    if isinstance(value, list):
        return [without_acceptance_logic_identity(item) for item in value]
    return value


def value_summary(value: Any) -> Any:
    if value is None or isinstance(value, (bool, int, float)):
        return value
    if isinstance(value, str):
        return value if len(value) <= 160 else value[:157] + "..."
    encoded = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    summary: dict[str, Any] = {
        "type": type(value).__name__,
        "sha256": hashlib.sha256(encoded.encode()).hexdigest(),
    }
    if isinstance(value, (dict, list)):
        summary["length"] = len(value)
    return summary


def append_sample(items: list[dict[str, Any]], sample: dict[str, Any]) -> None:
    if len(items) < MAX_SAMPLES:
        items.append(sample)


def command_names(commands: list[Any]) -> list[str]:
    return [
        str(command.get("command", "<missing>"))
        if isinstance(command, dict)
        else "<non-object>"
        for command in commands
    ]


def kernel_command_names(commands: list[Any]) -> list[str]:
    """Remove commands appended by SupervisorRuntime after apply_event."""
    return [
        name
        for name in command_names(commands)
        if name != "install_scheduled_local_closure_records"
    ]


def stage_projection(state: dict[str, Any]) -> dict[str, Any]:
    return {key: state.get(key) for key in ("cycle", "phase", "stage")}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-dir", type=Path, required=True)
    parser.add_argument(
        "--history-git-dir",
        type=Path,
        required=True,
        help="a writable/scratch copy of the recorded project's .git directory",
    )
    parser.add_argument("--baseline-bin", type=Path, required=True)
    parser.add_argument("--candidate-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    for binary in (args.baseline_bin, args.candidate_bin):
        if not binary.is_file():
            raise ReplayFailure(f"kernel binary does not exist: {binary}")

    commits = checkpoint_commits(args.history_git_dir)
    start_index = min(commits)
    baseline_state = checkpoint_state(args.history_git_dir, commits[start_index])
    propagated_candidate_state = copy.deepcopy(baseline_state)

    event_counts: collections.Counter[str] = collections.Counter()
    direct_difference_groups: dict[tuple[str, tuple[str, ...]], dict[str, Any]] = {}
    propagated_difference_groups: dict[
        tuple[str, tuple[str, ...]], dict[str, Any]
    ] = {}
    baseline_command_mismatches: list[dict[str, Any]] = []
    direct_command_mismatches: list[dict[str, Any]] = []
    propagated_command_mismatches: list[dict[str, Any]] = []
    stage_mismatches: list[dict[str, Any]] = []
    baseline_command_mismatch_count = 0
    direct_command_mismatch_count = 0
    propagated_command_mismatch_count = 0
    stage_mismatch_count = 0
    baseline_command_kind_mismatch_count = 0
    direct_command_kind_mismatch_count = 0
    propagated_command_kind_mismatch_count = 0
    direct_baseline_command_mismatch_count = 0
    direct_baseline_semantic_command_mismatch_count = 0
    propagated_baseline_command_mismatch_count = 0
    propagated_baseline_semantic_command_mismatch_count = 0
    direct_baseline_command_mismatch_samples: list[dict[str, Any]] = []
    propagated_baseline_command_mismatch_samples: list[dict[str, Any]] = []
    direct_semantic_difference_groups: dict[
        tuple[str, tuple[str, ...]], dict[str, Any]
    ] = {}
    propagated_semantic_difference_groups: dict[
        tuple[str, tuple[str, ...]], dict[str, Any]
    ] = {}
    direct_identity_only_state_difference_count = 0
    propagated_identity_only_state_difference_count = 0
    acceptance_logic_identities: dict[str, set[str]] = {
        "baseline": set(),
        "candidate": set(),
    }
    checkpoint_runtime_difference_groups: collections.Counter[
        tuple[str, ...]
    ] = collections.Counter()

    expected_index: int | None = None
    total_events = 0
    replayed_events = 0
    checkpoint_resyncs = 0
    pre_event_checkpoint_resyncs = 0
    pre_event_checkpoint_indices: list[int] = []
    propagation_seams: list[dict[str, Any]] = []

    baseline_driver = KernelDriver(args.baseline_bin)
    direct_driver = KernelDriver(args.candidate_bin)
    propagated_driver = KernelDriver(args.candidate_bin)
    baseline_driver.set_state(baseline_state)
    direct_driver.set_state(baseline_state)
    propagated_driver.set_state(propagated_candidate_state)

    for path, line_number, row in event_rows(args.event_dir):
        total_events += 1
        index = row.get("index")
        if not isinstance(index, int):
            raise ReplayFailure(f"{path}:{line_number}: non-integer event index")
        if expected_index is None:
            expected_index = index
        if index != expected_index:
            raise ReplayFailure(
                f"{path}:{line_number}: event index {index}, expected {expected_index}"
            )
        expected_index += 1
        event = row.get("event")
        if not isinstance(event, dict) or not isinstance(event.get("event"), str):
            raise ReplayFailure(f"{path}:{line_number}: malformed event envelope")
        event_tag = event["event"]
        event_counts[event_tag] += 1
        if event_tag == 'rebind_exclusive_checkpoint':
            raise ReplayFailure("exclusive checkpoint recovery requires native replay with Git evidence")
        if index <= start_index:
            continue

        expected_commands = row.get("commands")
        if not isinstance(expected_commands, list):
            raise ReplayFailure(f"{path}:{line_number}: commands is not a list")
        checkpoint_issued = any(
            isinstance(command, dict)
            and command.get("command") == "commit_checkpoint"
            for command in expected_commands
        )

        # A checkpoint commit can exist at the next dense event index even
        # though that event is not the transition which created it.  This is
        # the historical commit-before-event-persist crash window: Git carries
        # the authenticated post-transition state/commands, while JSONL lacks
        # the transition and later reuses its still-unadvanced index.  Install
        # that Git-ahead state BEFORE the recorded event and report the seam.
        if index in commits and not checkpoint_issued:
            document = checkpoint_document(args.history_git_dir, commits[index])
            recorded_commands = document.get("commands")
            if not (
                isinstance(recorded_commands, list)
                and any(
                    isinstance(command, dict)
                    and command.get("command") == "commit_checkpoint"
                    for command in recorded_commands
                )
            ):
                raise ReplayFailure(
                    f"checkpoint {index} does not correspond to the recorded event and "
                    "does not authenticate a commit_checkpoint command"
                )
            recorded_state = document.get("state")
            if not isinstance(recorded_state, dict):
                raise ReplayFailure(f"checkpoint {index} has no decoded state")
            prior_baseline = baseline_driver.get_state()
            prior_propagated = propagated_driver.get_state()
            candidate_fields = top_level_differences(
                prior_baseline, prior_propagated
            )
            propagation_seams.append(
                {
                    "index": index,
                    "candidate_fields_discarded_at_unrecorded_transition": candidate_fields,
                }
            )
            baseline_state = recorded_state
            # The transition which produced this checkpoint is absent from
            # JSONL, so there is no sound way to apply it to a candidate-only
            # predecessor. Reset the causal lane here and expose the lost
            # propagation in the result instead of inventing an input.
            propagated_candidate_state = copy.deepcopy(recorded_state)
            baseline_driver.set_state(baseline_state)
            direct_driver.set_state(baseline_state)
            propagated_driver.set_state(propagated_candidate_state)
            pre_event_checkpoint_resyncs += 1
            pre_event_checkpoint_indices.append(index)

        replayed_events += 1
        if replayed_events % 25 == 0:
            print(
                f"replay progress: {replayed_events} events; index={index}; event={event_tag}",
                file=sys.stderr,
                flush=True,
            )

        archives = hydrate_dispatch_event(args.history_git_dir, event)
        try:
            baseline = baseline_driver.apply_event(event, archives)
            direct = direct_driver.apply_event(event, archives)
            propagated = propagated_driver.apply_event(event, archives)
        except ReplayFailure as exc:
            raise ReplayFailure(f"event {index} ({event_tag}): {exc}") from exc

        if baseline["commands"] != expected_commands:
            baseline_command_mismatch_count += 1
            append_sample(
                baseline_command_mismatches,
                {
                    "index": index,
                    "event": event_tag,
                    "expected": command_names(expected_commands),
                    "actual": command_names(baseline["commands"]),
                },
            )
        if direct["commands"] != expected_commands:
            direct_command_mismatch_count += 1
            append_sample(
                direct_command_mismatches,
                {
                    "index": index,
                    "event": event_tag,
                    "expected": command_names(expected_commands),
                    "actual": command_names(direct["commands"]),
                },
            )
        if propagated["commands"] != expected_commands:
            propagated_command_mismatch_count += 1
            append_sample(
                propagated_command_mismatches,
                {
                    "index": index,
                    "event": event_tag,
                    "expected": command_names(expected_commands),
                    "actual": command_names(propagated["commands"]),
                },
            )

        expected_kinds = kernel_command_names(expected_commands)
        if kernel_command_names(baseline["commands"]) != expected_kinds:
            baseline_command_kind_mismatch_count += 1
        if kernel_command_names(direct["commands"]) != expected_kinds:
            direct_command_kind_mismatch_count += 1
        if kernel_command_names(propagated["commands"]) != expected_kinds:
            propagated_command_kind_mismatch_count += 1

        if direct["commands"] != baseline["commands"]:
            direct_baseline_command_mismatch_count += 1
            paths = difference_paths(baseline["commands"], direct["commands"])
            append_sample(
                direct_baseline_command_mismatch_samples,
                {"index": index, "event": event_tag, "paths": paths},
            )
        if without_acceptance_logic_identity(
            direct["commands"]
        ) != without_acceptance_logic_identity(baseline["commands"]):
            direct_baseline_semantic_command_mismatch_count += 1

        if propagated["commands"] != baseline["commands"]:
            propagated_baseline_command_mismatch_count += 1
            paths = difference_paths(baseline["commands"], propagated["commands"])
            append_sample(
                propagated_baseline_command_mismatch_samples,
                {"index": index, "event": event_tag, "paths": paths},
            )
        if without_acceptance_logic_identity(
            propagated["commands"]
        ) != without_acceptance_logic_identity(baseline["commands"]):
            propagated_baseline_semantic_command_mismatch_count += 1

        expected_stage = {
            "cycle": row.get("cycle"),
            "phase": row.get("phase"),
            "stage": row.get("stage"),
        }
        for kernel_name, response in (
            ("baseline", baseline),
            ("candidate_direct", direct),
            ("candidate_propagated", propagated),
        ):
            if stage_projection(response) != expected_stage:
                stage_mismatch_count += 1
                append_sample(
                    stage_mismatches,
                    {
                        "index": index,
                        "event": event_tag,
                        "kernel": kernel_name,
                        "expected": expected_stage,
                        "actual": stage_projection(response),
                    },
                )

        direct_fields: list[str] = []
        baseline_full: dict[str, Any] | None = None
        direct_full: dict[str, Any] | None = None
        if baseline["state_sha256"] != direct["state_sha256"]:
            baseline_full = baseline_driver.get_state()
            direct_full = direct_driver.get_state()
            direct_fields = top_level_differences(baseline_full, direct_full)
            if not direct_fields:
                raise ReplayFailure(
                    f"event {index}: direct state digests differ but decoded states do not"
                )
        if direct_fields:
            direct_paths = difference_paths(baseline_full, direct_full)
            key = (event_tag, tuple(direct_fields))
            group = direct_difference_groups.setdefault(
                key,
                {
                    "event": event_tag,
                    "fields": direct_fields,
                    "count": 0,
                    "event_indices": [],
                    "paths": direct_paths,
                    "sample_values": {},
                },
            )
            group["count"] += 1
            if len(group["event_indices"]) < MAX_SAMPLES:
                group["event_indices"].append(index)
            if not group["sample_values"]:
                group["sample_values"] = {
                    field: {
                        "baseline": value_summary(baseline_full.get(field)),
                        "candidate": value_summary(direct_full.get(field)),
                    }
                    for field in direct_fields
                }
            baseline_identity = (
                baseline_full.get("in_flight_request") or {}
            ).get("acceptance_logic_identity")
            candidate_identity = (
                direct_full.get("in_flight_request") or {}
            ).get("acceptance_logic_identity")
            if isinstance(baseline_identity, str):
                acceptance_logic_identities["baseline"].add(baseline_identity)
            if isinstance(candidate_identity, str):
                acceptance_logic_identities["candidate"].add(candidate_identity)
            semantic_baseline = without_acceptance_logic_identity(baseline_full)
            semantic_direct = without_acceptance_logic_identity(direct_full)
            semantic_paths = difference_paths(semantic_baseline, semantic_direct)
            if not semantic_paths:
                direct_identity_only_state_difference_count += 1
            else:
                semantic_fields = top_level_differences(
                    semantic_baseline, semantic_direct
                )
                semantic_key = (event_tag, tuple(semantic_paths))
                semantic_group = direct_semantic_difference_groups.setdefault(
                    semantic_key,
                    {
                        "event": event_tag,
                        "fields": semantic_fields,
                        "paths": semantic_paths,
                        "count": 0,
                        "event_indices": [],
                    },
                )
                semantic_group["count"] += 1
                if len(semantic_group["event_indices"]) < MAX_SAMPLES:
                    semantic_group["event_indices"].append(index)
            # The direct lane isolates each event.  Do not let this event's
            # candidate-only state influence the next event.
            direct_driver.set_state(baseline_full)

        propagated_fields: list[str] = []
        propagated_full: dict[str, Any] | None = None
        if baseline["state_sha256"] != propagated["state_sha256"]:
            if baseline_full is None:
                baseline_full = baseline_driver.get_state()
            propagated_full = propagated_driver.get_state()
            propagated_fields = top_level_differences(
                baseline_full, propagated_full
            )
            if not propagated_fields:
                raise ReplayFailure(
                    f"event {index}: propagated state digests differ but decoded states do not"
                )
        if propagated_fields:
            if propagated_full is None or baseline_full is None:
                raise ReplayFailure(f"event {index}: missing propagated state")
            propagated_paths = difference_paths(baseline_full, propagated_full)
            key = (event_tag, tuple(propagated_fields))
            group = propagated_difference_groups.setdefault(
                key,
                {
                    "event": event_tag,
                    "fields": propagated_fields,
                    "count": 0,
                    "event_indices": [],
                    "paths": propagated_paths,
                },
            )
            group["count"] += 1
            if len(group["event_indices"]) < MAX_SAMPLES:
                group["event_indices"].append(index)
            semantic_baseline = without_acceptance_logic_identity(baseline_full)
            semantic_propagated = without_acceptance_logic_identity(propagated_full)
            semantic_paths = difference_paths(
                semantic_baseline, semantic_propagated
            )
            if not semantic_paths:
                propagated_identity_only_state_difference_count += 1
            else:
                semantic_fields = top_level_differences(
                    semantic_baseline, semantic_propagated
                )
                semantic_key = (event_tag, tuple(semantic_paths))
                semantic_group = propagated_semantic_difference_groups.setdefault(
                    semantic_key,
                    {
                        "event": event_tag,
                        "fields": semantic_fields,
                        "paths": semantic_paths,
                        "count": 0,
                        "event_indices": [],
                    },
                )
                semantic_group["count"] += 1
                if len(semantic_group["event_indices"]) < MAX_SAMPLES:
                    semantic_group["event_indices"].append(index)

        if checkpoint_issued:
            sha = commits.get(index)
            if sha is None:
                raise ReplayFailure(
                    f"event {index} issued commit_checkpoint but no checkpoint commit exists"
                )
            if baseline_full is None:
                baseline_full = baseline_driver.get_state()
            if propagated_full is None:
                propagated_full = propagated_driver.get_state()
            recorded_state = checkpoint_state(args.history_git_dir, sha)
            runtime_fields = top_level_differences(baseline_full, recorded_state)
            checkpoint_runtime_difference_groups[tuple(runtime_fields)] += 1

            # A checkpoint hook may add runtime-owned data (notably the Git
            # clean pointer).  Start from that exact recorded post-hook state,
            # then overlay every candidate-only semantic field so downstream
            # behavior reflects the candidate change instead of erasing it.
            candidate_fields = top_level_differences(
                baseline_full, propagated_full
            )
            next_candidate_state = copy.deepcopy(recorded_state)
            for field in candidate_fields:
                if field in propagated_full:
                    next_candidate_state[field] = copy.deepcopy(
                        propagated_full[field]
                    )
                else:
                    next_candidate_state.pop(field, None)
            baseline_state = recorded_state
            propagated_candidate_state = next_candidate_state
            baseline_driver.set_state(baseline_state)
            direct_driver.set_state(baseline_state)
            propagated_driver.set_state(propagated_candidate_state)
            checkpoint_resyncs += 1

    baseline_driver.close()
    direct_driver.close()
    propagated_driver.close()

    result = {
        "status": "ok",
        "inputs": {
            "event_dir": str(args.event_dir),
            "history_git_dir": str(args.history_git_dir),
            "baseline_bin": str(args.baseline_bin),
            "candidate_bin": str(args.candidate_bin),
            "start_checkpoint_index": start_index,
        },
        "coverage": {
            "total_events": total_events,
            "replayed_events": replayed_events,
            "skipped_genesis_events": total_events - replayed_events,
            "checkpoint_resyncs": checkpoint_resyncs,
            "git_ahead_pre_event_resyncs": pre_event_checkpoint_resyncs,
            "git_ahead_pre_event_indices": pre_event_checkpoint_indices,
            "propagation_seams": propagation_seams,
            "event_counts": dict(sorted(event_counts.items())),
        },
        "transcript_checks": {
            "scope_note": (
                "Full transcript payloads include runtime dispatch hints and runtime-appended "
                "closure commands; command-kind checks remove only the latter."
            ),
            "baseline_command_mismatch_count": baseline_command_mismatch_count,
            "baseline_command_mismatch_samples": baseline_command_mismatches,
            "candidate_direct_command_mismatch_count": direct_command_mismatch_count,
            "candidate_direct_command_mismatch_samples": direct_command_mismatches,
            "candidate_propagated_command_mismatch_count": propagated_command_mismatch_count,
            "candidate_propagated_command_mismatch_samples": propagated_command_mismatches,
            "baseline_kernel_command_kind_mismatch_count": baseline_command_kind_mismatch_count,
            "candidate_direct_kernel_command_kind_mismatch_count": (
                direct_command_kind_mismatch_count
            ),
            "candidate_propagated_kernel_command_kind_mismatch_count": (
                propagated_command_kind_mismatch_count
            ),
            "stage_mismatch_count": stage_mismatch_count,
            "stage_mismatch_samples": stage_mismatches,
        },
        "direct_kernel_state_differences": sorted(
            direct_difference_groups.values(),
            key=lambda group: (group["event"], group["fields"]),
        ),
        "direct_kernel_semantic_state_differences": sorted(
            direct_semantic_difference_groups.values(),
            key=lambda group: (group["event"], group["paths"]),
        ),
        "direct_acceptance_identity_only_state_difference_count": (
            direct_identity_only_state_difference_count
        ),
        "propagated_kernel_state_differences": sorted(
            propagated_difference_groups.values(),
            key=lambda group: (group["event"], group["fields"]),
        ),
        "propagated_kernel_semantic_state_differences": sorted(
            propagated_semantic_difference_groups.values(),
            key=lambda group: (group["event"], group["paths"]),
        ),
        "propagated_acceptance_identity_only_state_difference_count": (
            propagated_identity_only_state_difference_count
        ),
        "kernel_differential_commands": {
            "baseline_candidate_direct_raw_mismatch_count": direct_baseline_command_mismatch_count,
            "baseline_candidate_direct_without_acceptance_identity_mismatch_count": (
                direct_baseline_semantic_command_mismatch_count
            ),
            "baseline_candidate_direct_mismatch_samples": direct_baseline_command_mismatch_samples,
            "baseline_candidate_propagated_raw_mismatch_count": (
                propagated_baseline_command_mismatch_count
            ),
            "baseline_candidate_propagated_without_acceptance_identity_mismatch_count": (
                propagated_baseline_semantic_command_mismatch_count
            ),
            "baseline_candidate_propagated_mismatch_samples": (
                propagated_baseline_command_mismatch_samples
            ),
            "acceptance_logic_identities": {
                key: sorted(values)
                for key, values in acceptance_logic_identities.items()
            },
        },
        "checkpoint_runtime_difference_groups": [
            {"fields": list(fields), "count": count}
            for fields, count in sorted(
                checkpoint_runtime_difference_groups.items(),
                key=lambda item: (item[0], item[1]),
            )
        ],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")

    print(
        json.dumps(
            {
                "status": "ok",
                "total_events": total_events,
                "replayed_events": replayed_events,
                "checkpoint_resyncs": checkpoint_resyncs,
                "git_ahead_pre_event_resyncs": pre_event_checkpoint_resyncs,
                "baseline_command_mismatches": baseline_command_mismatch_count,
                "candidate_direct_command_mismatches": direct_command_mismatch_count,
                "candidate_propagated_command_mismatches": propagated_command_mismatch_count,
                "stage_mismatches": stage_mismatch_count,
                "baseline_kernel_command_kind_mismatches": baseline_command_kind_mismatch_count,
                "candidate_direct_semantic_command_mismatches": (
                    direct_baseline_semantic_command_mismatch_count
                ),
                "candidate_direct_semantic_state_difference_groups": len(
                    direct_semantic_difference_groups
                ),
                "direct_state_difference_groups": len(direct_difference_groups),
                "propagated_state_difference_groups": len(
                    propagated_difference_groups
                ),
                "output": str(args.output),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ReplayFailure as exc:
        print(f"replay failed: {exc}", file=sys.stderr)
        raise SystemExit(1)

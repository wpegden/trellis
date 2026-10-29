#!/usr/bin/env python3
"""Read durable event logs; emit per-cycle certification denominators.

No runtime calls, state writes, replay, or prover execution. Consume a fixed
copy for comparisons. Source transactions mean accepted source-apply boundaries,
including partial progress/no-op work, not new proofs or final reviewer approval.
"""
import argparse
import json
from pathlib import Path


RESTORES = {
    "restore_worktree_to_active_worker_base", "restore_worktree_to_head",
    "restore_worktree_to_last_clean", "restore_theorem_stating_node_and_prune_orphans",
}


def summarize(records):
    cycles = {}
    last_index = None
    pending = {}
    for record in records:
        index = record["index"]
        # Do not silently double-count concatenated/replayed input or gaps.
        if last_index is not None and index != last_index + 1:
            raise ValueError(f"noncontiguous event indices: {last_index}, {index}")
        last_index = index
        cycle = record["cycle"]
        row = cycles.setdefault(cycle, dict(
            schema=1, cycle=cycle, cycle_starts=0, accepted_source_transactions=0,
            worker_source_acceptances=0, sidecar_acceptances=0,
            rejected_worker_responses=0, unclassified_worker_responses=0,
            source_restores=0, accepted_event_indices=[],
            first_ts_ms=record.get("ts_ms"), last_ts_ms=None))
        row["last_ts_ms"] = record.get("ts_ms")
        event = record["event"]
        commands = record["commands"]
        names = {c["command"] for c in commands}
        restored = bool(names & RESTORES)
        row["source_restores"] += int(restored)
        if event["event"] == "start_cycle":
            row["cycle_starts"] += 1
        sidecar = event["event"] == "sidecar_closure"
        if sidecar:
            # The kernel emits this only after acceptance, with checkpoint.
            if "commit_checkpoint" not in names or restored:
                raise ValueError(f"unexpected sidecar acceptance shape at {index}")
            row["sidecar_acceptances"] += 1
        response = event.get("response", {})
        worker = False
        if response.get("kind") == "worker":
            request = pending.pop(response.get("request_id"), None)
            if restored:
                row["rejected_worker_responses"] += 1
            elif (request is not None and request.get("work_kind", "Standard") == "Standard"
                  and response.get("status") == "Ok" and response.get("outcome") == "Valid"):
                worker = True
                row["worker_source_acceptances"] += 1
            else:
                # A fragment may begin after dispatch. Unknown is not accepted.
                row["unclassified_worker_responses"] += 1
        if worker or sidecar:
            row["accepted_source_transactions"] += 1
            row["accepted_event_indices"].append(index)
        for command in commands:
            if command["command"] == "issue_request":
                request = command["request"]
                if request.get("kind") == "Worker":
                    pending[request["id"]] = request
    return list(cycles.values())


def read_records(paths):
    for path in paths:
        with path.open() as handle:
            for line in handle:
                if line.strip():
                    yield json.loads(line)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("events", type=Path, help="event-log directory or one JSONL file")
    args = parser.parse_args()
    paths = sorted(args.events.glob("cycle-*.jsonl")) if args.events.is_dir() else [args.events]
    for row in summarize(read_records(paths)):
        print(json.dumps(row, separators=(",", ":")))


if __name__ == "__main__":
    main()

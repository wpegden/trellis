#!/usr/bin/env python3
"""One-shot migration: seed `process-memory/` from the v1 cumulative
audit-report memory (PROCESS_MEMORY_SPEC.md §9).

Parses the latest audit plan's `## Established constraints and refuted
routes` section out of a runtime's `protocol_state.json` (`audit_plan`
first, else `previous_audit_plan_snapshot`) and writes one entry file per
bullet under `<repo>/process-memory/global/` plus `INDEX.md`.

Also bumps `process_memory_seq` in the runtime's protocol_state.json to
the number of seeded entries so the kernel's next `add` cannot mint a
colliding id. Run while the supervisor is DOWN (the in-memory supervisor
would overwrite the state file), then commit the new directory.

Idempotent: refuses to run when `process-memory/` already exists.

Usage:
    python3 scripts/seed_process_memory.py --runtime <runtime_root> --repo <repo_root> [--dry-run]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

SECTION_HEADING = "## Established constraints and refuted routes"
REFUTED_MARKERS = ("refut", "dead end", "does not work", "fails", "cannot work", "impossible")


def kebab_title(text: str) -> str:
    out: list[str] = []
    pending = False
    for ch in text.strip():
        low = ch.lower()
        if low.isascii() and low.isalnum():
            if pending and out:
                out.append("-")
            pending = False
            out.append(low)
        else:
            pending = True
        if len("".join(out)) >= 48:
            break
    slug = "".join(out)
    return slug or "entry"


def extract_section(report: str) -> list[str]:
    """Return the section's bullet items (multi-line bullets folded)."""
    lines = report.splitlines()
    try:
        start = next(i for i, line in enumerate(lines) if line.strip() == SECTION_HEADING)
    except StopIteration:
        return []
    items: list[str] = []
    for line in lines[start + 1 :]:
        if line.startswith("## "):
            break
        stripped = line.strip()
        if not stripped:
            continue
        if re.match(r"^[-*]\s+|^\d+[.)]\s+", stripped):
            items.append(re.sub(r"^[-*]\s+|^\d+[.)]\s+", "", stripped, count=1))
        elif items:
            # Continuation line of the previous bullet.
            items[-1] += " " + stripped
    return items


def classify(item: str) -> str:
    lowered = item.lower()
    if any(marker in lowered for marker in REFUTED_MARKERS):
        return "refuted-route"
    return "constraint"


def entry_text(entry_id: str, entry_type: str, body: str, cycle: int, request_id: int) -> str:
    return (
        "---\n"
        f"id: {entry_id}\n"
        f"type: {entry_type}\n"
        "status: active\n"
        "coarse_node: global\n"
        f"created: {{cycle: {cycle}, request_id: {request_id}}}\n"
        "---\n\n"
        f"{body.strip()}\n"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", required=True, type=Path, help="runtime root holding protocol_state.json")
    parser.add_argument("--repo", required=True, type=Path, help="tablet repo root receiving process-memory/")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    state_path = args.runtime / "protocol_state.json"
    if not state_path.is_file():
        # Some runtimes name it supervisor_state.json; probe both.
        alt = args.runtime / "supervisor_state.json"
        if alt.is_file():
            state_path = alt
        else:
            print(f"ERROR: no protocol_state.json under {args.runtime}", file=sys.stderr)
            return 2

    memory_root = args.repo / "process-memory"
    if memory_root.exists():
        print(f"ERROR: {memory_root} already exists; refusing to re-seed", file=sys.stderr)
        return 2

    state = json.loads(state_path.read_text(encoding="utf-8"))
    plan = state.get("audit_plan") or state.get("previous_audit_plan_snapshot")
    if not isinstance(plan, dict) or not str(plan.get("report") or "").strip():
        print("ERROR: no audit plan report found in state", file=sys.stderr)
        return 2
    cycle = int(plan.get("written_at_cycle") or 0)
    request_id = int(plan.get("written_by_request") or 0)

    items = extract_section(str(plan["report"]))
    if not items:
        print(f"ERROR: report has no `{SECTION_HEADING}` bullets to seed from", file=sys.stderr)
        return 2

    entries = []
    for i, item in enumerate(items, start=1):
        entry_id = f"pm-{i:04}-{kebab_title(item)}"
        entries.append((entry_id, classify(item), item))

    if args.dry_run:
        for entry_id, entry_type, item in entries:
            print(f"{entry_id}  [{entry_type}]  {item[:100]}")
        print(f"(dry run) would seed {len(entries)} entries and set process_memory_seq={len(entries)}")
        return 0

    global_dir = memory_root / "global"
    global_dir.mkdir(parents=True)
    index_lines = [
        "# Process memory index",
        "",
        "Kernel-generated; one line per ACTIVE entry. Full entries live in the",
        "per-cone files. Do not hand-edit.",
        "",
    ]
    for entry_id, entry_type, item in entries:
        (global_dir / f"{entry_id}.md").write_text(
            entry_text(entry_id, entry_type, item, cycle, request_id), encoding="utf-8"
        )
        hook = item.strip().splitlines()[0][:160]
        index_lines.append(f"- [{entry_id}] {entry_type}/global — {hook}")
    (memory_root / "INDEX.md").write_text("\n".join(index_lines) + "\n", encoding="utf-8")

    seeded_seq = len(entries)
    current_seq = int(state.get("process_memory_seq") or 0)
    if seeded_seq > current_seq:
        state["process_memory_seq"] = seeded_seq
        tmp = state_path.with_suffix(".json.pm-seed-tmp")
        tmp.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8")
        tmp.replace(state_path)
        print(f"process_memory_seq: {current_seq} -> {seeded_seq} in {state_path}")

    print(f"seeded {seeded_seq} entries under {global_dir} + INDEX.md; commit them in the repo")
    return 0


if __name__ == "__main__":
    sys.exit(main())

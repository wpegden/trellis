#!/usr/bin/env python3
"""Canonical GOAL-to-extracted-item binding shared by Phase 0 and setup."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import sys
from pathlib import Path
from typing import Any

try:
    from scripts.pv_model_opacity import (
        declared_names,
        read_model_bodiless_declarations,
        referenced_names,
    )
    from scripts.pv_prose_goal import parse_goal
    from scripts.pv_source_tree import (
        build_source_partition_manifest,
        canonical_json,
        source_tree_digest,
        source_tree_manifest,
        tagged_hash,
        validate_source_partition_manifest,
    )
except ModuleNotFoundError:  # direct script execution
    from pv_model_opacity import (  # type: ignore[no-redef]
        declared_names,
        read_model_bodiless_declarations,
        referenced_names,
    )
    from pv_prose_goal import parse_goal  # type: ignore[no-redef]
    from pv_source_tree import (  # type: ignore[no-redef]
        build_source_partition_manifest,
        canonical_json,
        source_tree_digest,
        source_tree_manifest,
        tagged_hash,
        validate_source_partition_manifest,
    )


SCHEMA = "trellis-phase0-goal-binding-report/v1"
# One hop, not a closure: a target is reported against the bodiless
# declarations its own model text writes down. What those declarations reach in
# turn is not followed, and the report says so in this field.
BODILESS_SCOPE = "direct_calls_only"
BODILESS_SCAN_STATUS = ("read", "unavailable", "not_extracted")
_DIGEST = re.compile(r"[0-9a-f]{64}")
_PROVENANCE = re.compile(r"\[([^\]\n]+)\](?::|\s|$)")
_SOURCE = re.compile(
    r"Source:\s*'([^'\n]+)'(?:,\s*lines\s+(\d+):\d+-(\d+):\d+)?"
)


class GoalBindingError(ValueError):
    pass


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise GoalBindingError(f"duplicate JSON member: {key}")
        result[key] = value
    return result


def _load_object(path: Path, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_bytes(), object_pairs_hook=_strict_object)
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise GoalBindingError(f"cannot read {label} {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise GoalBindingError(f"{label} must be a JSON object")
    return value


def _matches(pv_tablet: dict[str, Any]) -> list[dict[str, Any]]:
    models = pv_tablet.get("extraction_models")
    if not isinstance(models, list):
        raise GoalBindingError("pv_tablet extraction_models must be an array")
    matches: list[dict[str, Any]] = []
    for model in models:
        if not isinstance(model, dict):
            raise GoalBindingError("pv_tablet contains a malformed extraction model")
        lean = model.get("lean")
        node = model.get("node")
        provenance = model.get("provenance")
        if not isinstance(lean, str) or not isinstance(node, str) or not node:
            raise GoalBindingError("extraction model lacks lean bytes or model node")
        if not isinstance(provenance, dict):
            raise GoalBindingError("extraction model lacks provenance")
        source_sha256 = provenance.get("source_sha256")
        if not isinstance(source_sha256, str) or _DIGEST.fullmatch(source_sha256) is None:
            raise GoalBindingError("extraction model has an invalid source digest")
        paths: set[str] = set()
        for line in lean.splitlines():
            if not line.lstrip().startswith("/--"):
                continue
            paths.update(match.group(1) for match in _PROVENANCE.finditer(line))
        for item_path in paths:
            source = _SOURCE.search(lean)
            source_file = source.group(1) if source else provenance.get("source_file")
            if not isinstance(source_file, str) or not source_file:
                raise GoalBindingError("extraction model lacks source-file provenance")
            matches.append(
                {
                    "adapted_item_path": item_path,
                    "model_node": node,
                    "source_sha256": source_sha256,
                    "_lean": lean,
                    "_source_file": source_file,
                    "_source_line_start": int(source.group(2)) if source and source.group(2) else None,
                    "_source_line_end": int(source.group(3)) if source and source.group(3) else None,
                }
            )
    matches.sort(
        key=lambda row: (
            row["adapted_item_path"].encode("utf-8"),
            row["model_node"].encode("utf-8"),
            row["source_sha256"],
        )
    )
    if len({(row["adapted_item_path"], row["model_node"], row["source_sha256"]) for row in matches}) != len(matches):
        raise GoalBindingError("extraction models repeat an exact provenance binding")
    return matches


def _adaptation_masks(
    unadapted_tree: Path,
    adapted_tree: Path,
    ledger: dict[str, Any],
    partition_manifest: dict[str, Any],
) -> dict[str, list[str]]:
    unadapted_manifest = source_tree_manifest(unadapted_tree)
    try:
        validate_source_partition_manifest(partition_manifest, unadapted_manifest)
    except ValueError as exc:
        raise GoalBindingError(f"source partition manifest is invalid: {exc}") from exc
    origins = {
        row["relative_path"]: (
            "unadapted_target" if row["origin"] == "target" else "supporting"
        )
        for row in partition_manifest["files"]
    }
    states: dict[str, tuple[bytes, list[str]]] = {}
    # Per file, the uploaded-coordinate spans already replayed and the bytes
    # that replaced them, so a later entry's span can be rebased.
    applied: dict[str, list[tuple[int, int, bytes]]] = {}
    for path in sorted(unadapted_tree.rglob("*"), key=lambda item: os.fsencode(item)):
        if path.is_file():
            relative = path.relative_to(unadapted_tree).as_posix()
            raw = path.read_bytes()
            states[relative] = (raw, [origins[relative]] * len(raw))
    entries = ledger.get("entries")
    if not isinstance(entries, list):
        raise GoalBindingError("adaptation ledger entries must be an array")
    for entry in entries:
        try:
            relative = entry["file"]
            operation = entry["operation"]
            start = entry["before_span"]["start"]
            end = entry["before_span"]["end"]
            removed = base64.b64decode(entry["patch"]["removed_base64"], validate=True)
            inserted = base64.b64decode(entry["patch"]["inserted_base64"], validate=True)
        except (KeyError, TypeError, ValueError) as exc:
            raise GoalBindingError("adaptation ledger contains a malformed patch") from exc
        if not isinstance(relative, str) or not isinstance(start, int) or not isinstance(end, int):
            raise GoalBindingError("adaptation ledger patch coordinates are invalid")
        if operation == "add":
            if relative in states or start != 0 or end != 0 or removed:
                raise GoalBindingError("adaptation add entry is inconsistent")
            states[relative] = (inserted, ["inserted"] * len(inserted))
            continue
        if relative not in states:
            raise GoalBindingError(f"adaptation entry names absent file: {relative}")
        raw, mask = states[relative]
        # A span is measured in the uploaded file even when earlier entries have
        # already edited this file, so replaying in place needs the same rebase
        # the kernel applies: shift by the length change of every earlier span
        # that ends at or before this one.
        shift = sum(
            len(done_inserted) - (done_end - done_start)
            for done_start, done_end, done_inserted in applied.get(relative, ())
            if done_end <= start
        )
        for done_start, done_end, _done_inserted in applied.get(relative, ()):
            if done_start < end and start < done_end:
                raise GoalBindingError(
                    f"adaptation entries overlap in the uploaded file: {relative}"
                )
        here, there = start + shift, end + shift
        if not (0 <= here <= there <= len(raw)) or raw[here:there] != removed:
            raise GoalBindingError(f"adaptation entry span does not replay: {relative}")
        if operation == "delete":
            if start != 0 or there != len(raw) or inserted:
                raise GoalBindingError("adaptation delete entry is inconsistent")
            del states[relative]
            applied.pop(relative, None)
        elif operation == "modify":
            states[relative] = (
                raw[:here] + inserted + raw[there:],
                mask[:here] + ["inserted"] * len(inserted) + mask[there:],
            )
            applied.setdefault(relative, []).append((start, end, inserted))
        else:
            raise GoalBindingError(f"adaptation entry has invalid operation: {operation}")
    masks: dict[str, list[str]] = {}
    for relative, (raw, mask) in states.items():
        final = adapted_tree / relative
        if not final.is_file() or final.read_bytes() != raw:
            raise GoalBindingError(f"adaptation ledger does not replay final file: {relative}")
        masks[relative] = mask
    final_files = {
        path.relative_to(adapted_tree).as_posix()
        for path in adapted_tree.rglob("*")
        if path.is_file()
    }
    if final_files != set(states):
        raise GoalBindingError("adaptation ledger does not replay the final tree")
    return masks


def _match_origin(
    match: dict[str, Any], adapted_tree: Path, masks: dict[str, list[str]]
) -> str:
    relative = match["_source_file"]
    mask = masks.get(relative)
    if mask is None:
        raise GoalBindingError(f"binding source file is absent: {relative}")
    start_line = match["_source_line_start"]
    end_line = match["_source_line_end"]
    if start_line is None or end_line is None:
        selected = set(mask)
    else:
        lines = (adapted_tree / relative).read_bytes().splitlines(keepends=True)
        if not (1 <= start_line <= end_line <= len(lines)):
            raise GoalBindingError(f"binding source span is invalid: {relative}")
        start = sum(len(line) for line in lines[: start_line - 1])
        end = sum(len(line) for line in lines[:end_line])
        selected = set(mask[start:end])
    if "inserted" in selected:
        return "adaptation"
    if "supporting" in selected:
        return "supporting"
    return "target"


def _bodiless_scan(scan: dict[str, Any] | None) -> dict[str, Any]:
    """Normalize the checker's bodiless-declaration scan for the report.

    A caller that has no scan gets the honest `not_extracted` status and an
    empty list, never a silent "nothing was suppressed".
    """
    if scan is None:
        return {"status": "not_extracted", "declarations": []}
    if not isinstance(scan, dict):
        raise GoalBindingError("bodiless-declaration scan must be a JSON object")
    status = scan.get("status")
    if status not in BODILESS_SCAN_STATUS:
        raise GoalBindingError("bodiless-declaration scan has an unknown status")
    rows = scan.get("declarations")
    if not isinstance(rows, list):
        raise GoalBindingError("bodiless-declaration scan lacks its declarations")
    declarations: list[dict[str, str]] = []
    for row in rows:
        if (
            not isinstance(row, dict)
            or set(row) != {"name", "kind", "item_path"}
            or not isinstance(row["name"], str)
            or not row["name"]
            or row["kind"] not in ("axiom", "opaque")
            or not isinstance(row["item_path"], str)
        ):
            raise GoalBindingError("bodiless declaration row is malformed")
        declarations.append({key: row[key] for key in ("name", "kind", "item_path")})
    declarations.sort(key=lambda row: (row["name"].encode("utf-8"), row["kind"]))
    if status != "read" and declarations:
        raise GoalBindingError("an unread model cannot carry bodiless declarations")
    return {"status": status, "declarations": declarations}


def _target_bodiless(
    match: dict[str, Any],
    declarations: list[dict[str, str]],
    names: set[str],
) -> tuple[bool, list[str]]:
    """Whether this target's own model declaration has no body, and which
    bodiless declarations its model text writes down.

    Direct only, one hop: what those declarations reach in turn is not
    followed. The report states the scope in `model_bodiless_scope`.
    """
    if not names:
        return False, []
    lean = match.get("_lean")
    if not isinstance(lean, str):
        raise GoalBindingError("extraction model match lacks its Lean text")
    own = set(declared_names(lean, names))
    own.update(
        row["name"]
        for row in declarations
        if row["item_path"] and row["item_path"] == match["adapted_item_path"]
    )
    calls = [name for name in referenced_names(lean, names) if name not in own]
    return bool(own), calls


def build_goal_binding_report(
    goal_path: Path,
    pv_tablet_path: Path,
    adapted_tree: Path,
    unadapted_tree: Path,
    ledger: dict[str, Any],
    partition_manifest: dict[str, Any] | None = None,
    model_bodiless_declarations: dict[str, Any] | None = None,
    *,
    reject_unquoted_matches: bool = True,
) -> dict[str, Any]:
    goal_path = Path(goal_path)
    goal_bytes = goal_path.read_bytes()
    parsed_goal = parse_goal(goal_path)
    pv_tablet = _load_object(Path(pv_tablet_path), "pv_tablet")
    available = _matches(pv_tablet)
    unadapted_manifest = source_tree_manifest(Path(unadapted_tree))
    if partition_manifest is None:
        partition_manifest = build_source_partition_manifest(
            unadapted_manifest,
            {entry["relative_path"] for entry in unadapted_manifest["files"]},
        )
    masks = _adaptation_masks(
        Path(unadapted_tree), Path(adapted_tree), ledger, partition_manifest
    )

    if reject_unquoted_matches:
        unquoted = [
            heading
            for heading in parsed_goal["unquoted_headings"]
            if any(
                row["adapted_item_path"].endswith(f"::{heading['name']}")
                for row in available
            )
        ]
        if unquoted:
            rendered = [f"{item['name']} (line {item['line']})" for item in unquoted]
            raise GoalBindingError(
                "unquoted level-three heading(s) naming extracted functions: "
                f"{rendered!r}; mark each target with backticks"
            )

    scan = _bodiless_scan(model_bodiless_declarations)
    bodiless = scan["declarations"]
    bodiless_names = {row["name"] for row in bodiless}

    targets: list[dict[str, Any]] = []
    for target in parsed_goal["targets"]:
        name = target["name"]
        candidates = [
            row
            for row in available
            if row["adapted_item_path"].endswith(f"::{name}")
        ]
        status = "bound" if len(candidates) == 1 else "missing" if not candidates else "ambiguous"
        public_candidates = [
            {key: row[key] for key in ("adapted_item_path", "model_node", "source_sha256")}
            for row in candidates
        ]
        origin = (
            _match_origin(candidates[0], Path(adapted_tree), masks)
            if len(candidates) == 1 else "target"
        )
        own, calls = (
            _target_bodiless(candidates[0], bodiless, bodiless_names)
            if len(candidates) == 1 else (False, [])
        )
        targets.append(
            {
                "target_id": f"goal:{name}",
                "name": name,
                "resolution": target["resolution"],
                "heading_line": target["line"],
                "status": status,
                "matches": public_candidates,
                "origin": origin,
                "goal_binding_outside_target": len(candidates) == 1 and origin != "target",
                "goal_binding_in_adaptation": len(candidates) == 1 and origin == "adaptation",
                "model_declaration_bodiless": own,
                "model_bodiless_direct_calls": calls,
                "model_computation_suppressed": own or bool(calls),
            }
        )
    targets.sort(key=lambda row: row["target_id"].encode("utf-8"))
    report: dict[str, Any] = {
        "schema": SCHEMA,
        "goal_base64": base64.b64encode(goal_bytes).decode("ascii"),
        "goal_sha256": hashlib.sha256(goal_bytes).hexdigest(),
        "adapted_tree_sha256": source_tree_digest(Path(adapted_tree)),
        "model_bodiless_status": scan["status"],
        "model_bodiless_declarations": bodiless,
        "model_bodiless_scope": BODILESS_SCOPE,
        "targets": targets,
    }
    report["report_sha256"] = tagged_hash("raw-artifact", canonical_json(report))
    return report


def validate_goal_binding_report(report: dict[str, Any]) -> None:
    if set(report) != {
        "schema",
        "goal_base64",
        "goal_sha256",
        "adapted_tree_sha256",
        "model_bodiless_status",
        "model_bodiless_declarations",
        "model_bodiless_scope",
        "targets",
        "report_sha256",
    } or report.get("schema") != SCHEMA:
        raise GoalBindingError("GOAL-binding report has missing/unknown fields or wrong schema")
    embedded = report.get("report_sha256")
    unsigned = dict(report)
    unsigned.pop("report_sha256", None)
    if embedded != tagged_hash("raw-artifact", canonical_json(unsigned)):
        raise GoalBindingError("GOAL-binding report self digest is invalid")
    try:
        goal = base64.b64decode(report["goal_base64"], validate=True)
    except (KeyError, TypeError, ValueError) as exc:
        raise GoalBindingError("GOAL-binding report has invalid base64") from exc
    if base64.b64encode(goal).decode("ascii") != report["goal_base64"]:
        raise GoalBindingError("GOAL-binding report base64 is not canonical")
    if hashlib.sha256(goal).hexdigest() != report.get("goal_sha256"):
        raise GoalBindingError("GOAL-binding report GOAL digest is invalid")
    if _DIGEST.fullmatch(str(report.get("adapted_tree_sha256", ""))) is None:
        raise GoalBindingError("GOAL-binding report adapted root is invalid")
    if report.get("model_bodiless_scope") != BODILESS_SCOPE:
        raise GoalBindingError("GOAL-binding report bodiless scope is not direct-only")
    scan = _bodiless_scan(
        {
            "status": report.get("model_bodiless_status"),
            "declarations": report.get("model_bodiless_declarations"),
        }
    )
    if scan["declarations"] != report["model_bodiless_declarations"]:
        raise GoalBindingError("GOAL-binding report bodiless declarations are not canonical")
    bodiless_names = {row["name"] for row in scan["declarations"]}
    targets = report.get("targets")
    if not isinstance(targets, list) or not targets:
        raise GoalBindingError("GOAL-binding report has no targets")
    ids = [target.get("target_id") for target in targets if isinstance(target, dict)]
    if ids != sorted(ids, key=lambda value: str(value).encode("utf-8")) or len(set(ids)) != len(ids):
        raise GoalBindingError("GOAL-binding report targets are not canonical")
    for target in targets:
        if not isinstance(target, dict) or set(target) != {
            "target_id", "name", "resolution", "heading_line", "status", "matches",
            "origin", "goal_binding_outside_target", "goal_binding_in_adaptation",
            "model_declaration_bodiless", "model_bodiless_direct_calls",
            "model_computation_suppressed",
        } or target["origin"] not in {"target", "supporting", "adaptation"} \
            or not isinstance(target["goal_binding_outside_target"], bool) \
            or not isinstance(target["goal_binding_in_adaptation"], bool) \
            or target["goal_binding_outside_target"] != (target["status"] == "bound" and target["origin"] != "target") \
            or target["goal_binding_in_adaptation"] != (target["status"] == "bound" and target["origin"] == "adaptation"):
            raise GoalBindingError("GOAL-binding report target is malformed")
        calls = target["model_bodiless_direct_calls"]
        if (
            not isinstance(target["model_declaration_bodiless"], bool)
            or not isinstance(target["model_computation_suppressed"], bool)
            or not isinstance(calls, list)
            or any(name not in bodiless_names for name in calls)
            or calls != sorted(set(calls), key=lambda value: str(value).encode("utf-8"))
            or target["model_computation_suppressed"]
            != (target["model_declaration_bodiless"] or bool(calls))
            or (
                target["status"] != "bound"
                and (target["model_declaration_bodiless"] or calls)
            )
        ):
            raise GoalBindingError("GOAL-binding report target opacity facts are inconsistent")
        if not isinstance(target["matches"], list) or any(
            not isinstance(match, dict)
            or set(match) != {"adapted_item_path", "model_node", "source_sha256"}
            for match in target["matches"]
        ):
            raise GoalBindingError("GOAL-binding report match is malformed")


def report_is_complete(report: dict[str, Any]) -> bool:
    return bool(report.get("targets")) and all(
        isinstance(target, dict)
        and target.get("status") == "bound"
        and isinstance(target.get("matches"), list)
        and len(target["matches"]) == 1
        for target in report["targets"]
    )


def sealed_goal_binding_bytes(sealed_path: Path) -> bytes:
    """Return the exact canonical binding projection of a sealed generation."""
    sealed_path = Path(sealed_path)
    sealed = _load_object(sealed_path, "sealed Phase-0 generation")
    try:
        raw = sealed_path.read_bytes()
    except OSError as exc:
        raise GoalBindingError(
            f"cannot read sealed Phase-0 generation {sealed_path}: {exc}"
        ) from exc
    if canonical_json(sealed) != raw:
        raise GoalBindingError("sealed Phase-0 generation is not canonical JSON")
    if sealed.get("schema") != "trellis-phase0-sealed-generation/v1":
        raise GoalBindingError("sealed Phase-0 generation has the wrong schema")
    bundle = sealed.get("semantic_bundle")
    if not isinstance(bundle, dict):
        raise GoalBindingError("sealed Phase-0 generation lacks its semantic bundle")
    report = bundle.get("goal_binding_report")
    if not isinstance(report, dict):
        raise GoalBindingError("sealed Phase-0 generation lacks its binding report")
    return canonical_json(report)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--goal", required=True, type=Path)
    parser.add_argument("--pv-tablet", required=True, type=Path)
    parser.add_argument("--adapted-tree", required=True, type=Path)
    parser.add_argument("--unadapted-tree", required=True, type=Path)
    parser.add_argument("--ledger", type=Path)
    parser.add_argument("--partition-manifest", type=Path)
    parser.add_argument(
        "--model-root",
        type=Path,
        help="extraction output directory to scan for declarations with no definition",
    )
    parser.add_argument(
        "--model-bodiless",
        type=Path,
        help="a checker's already-computed bodiless-declaration scan",
    )
    parser.add_argument("--out", type=Path)
    parser.add_argument("--require-complete", action="store_true")
    parser.add_argument("--expected-sealed-generation", type=Path)
    args = parser.parse_args(argv)
    try:
        ledger = (
            _load_object(args.ledger, "adaptation ledger")
            if args.ledger is not None else {"entries": []}
        )
        if args.model_root is not None and args.model_bodiless is not None:
            raise GoalBindingError(
                "give the model to scan or an existing scan, not both"
            )
        scan = None
        if args.model_root is not None:
            scan = read_model_bodiless_declarations(args.model_root)
        elif args.model_bodiless is not None:
            scan = _load_object(args.model_bodiless, "bodiless-declaration scan")
        report = build_goal_binding_report(
            args.goal, args.pv_tablet, args.adapted_tree,
            args.unadapted_tree, ledger,
            _load_object(args.partition_manifest, "source partition manifest")
            if args.partition_manifest is not None else None,
            scan,
        )
        validate_goal_binding_report(report)
        if args.require_complete and not report_is_complete(report):
            unresolved = [
                f"{target['target_id']}:{target['status']}"
                for target in report["targets"]
                if target["status"] != "bound"
            ]
            raise GoalBindingError(f"GOAL bindings are incomplete: {unresolved!r}")
        output = canonical_json(report)
        if (
            args.expected_sealed_generation is not None
            and output != sealed_goal_binding_bytes(args.expected_sealed_generation)
        ):
            raise GoalBindingError(
                "production GOAL-binding report differs byte-for-byte from the sealed report"
            )
        if args.out is None:
            sys.stdout.buffer.write(output)
        else:
            args.out.parent.mkdir(parents=True, exist_ok=True)
            temporary = args.out.with_name(f".{args.out.name}.tmp")
            temporary.write_bytes(output)
            temporary.replace(args.out)
        return 0
    except (GoalBindingError, OSError, SystemExit) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Build the campaign target-resolution plan from the prose target registry."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path

try:
    from scripts.pv_source_tree import tagged_hash
except ModuleNotFoundError:  # direct script execution
    from pv_source_tree import tagged_hash  # type: ignore[no-redef]


PLAN_SCHEMA = "trellis-campaign-trust-plan/v4"
_RUST_IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


class DerivationBuildFailure(RuntimeError):
    """The prose target registry cannot produce a closed resolution plan."""


def canonical_bytes(value: object) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def _load_json_object(path: Path, label: str) -> dict[str, object]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise DerivationBuildFailure(f"cannot read {label} {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise DerivationBuildFailure(f"{label} {path} is not a JSON object")
    return value


def _prose_targets(targets_path: Path) -> list[tuple[str, str]]:
    raw = _load_json_object(targets_path, "prose target registry")
    entries = raw.get("verification_targets")
    if not isinstance(entries, list) or not entries:
        raise DerivationBuildFailure(
            f"{targets_path}: verification_targets must be a non-empty array"
        )

    targets: list[tuple[str, str]] = []
    for entry in entries:
        if isinstance(entry, str):
            name = entry
            resolution = "decide"
        elif isinstance(entry, dict):
            name = entry.get("name")
            resolution = entry.get("resolution", "decide")
        else:
            name = None
            resolution = None

        if not isinstance(name, str) or _RUST_IDENTIFIER.fullmatch(name) is None:
            raise DerivationBuildFailure(
                f"{targets_path}: target name {name!r} is not a Rust identifier"
            )
        if resolution not in ("decide", "prove"):
            raise DerivationBuildFailure(
                f"{targets_path}: target {name!r} has invalid resolution {resolution!r}"
            )
        targets.append((name, resolution))

    if len({name for name, _ in targets}) != len(targets):
        raise DerivationBuildFailure(f"{targets_path}: prose target names repeat")
    return sorted(targets)


def _prose_functions(targets_path: Path) -> list[str]:
    """Name-only projection retained for setup/release callers."""
    return [name for name, _ in _prose_targets(targets_path)]


def _self_digest(value: dict[str, object], field: str) -> str:
    unsigned = dict(value)
    unsigned.pop(field, None)
    return tagged_hash("raw-artifact", canonical_bytes(unsigned))


def _sealed_projection(
    sealed_path: Path, binding_path: Path
) -> tuple[list[dict[str, object]], list[dict[str, str]]]:
    sealed = _load_json_object(sealed_path, "sealed Phase-0 generation")
    binding = _load_json_object(binding_path, "GOAL-binding report")
    if canonical_bytes(sealed) != sealed_path.read_bytes():
        raise DerivationBuildFailure("sealed Phase-0 generation is not canonical JSON")
    if canonical_bytes(binding) != binding_path.read_bytes():
        raise DerivationBuildFailure("GOAL-binding report is not canonical JSON")
    if sealed.get("schema") != "trellis-phase0-sealed-generation/v1":
        raise DerivationBuildFailure("sealed Phase-0 generation has the wrong schema")
    if sealed.get("sealed_generation_sha256") != _self_digest(
        sealed, "sealed_generation_sha256"
    ):
        raise DerivationBuildFailure("sealed Phase-0 generation self digest is invalid")
    bundle = sealed.get("semantic_bundle")
    if not isinstance(bundle, dict) or bundle.get("schema") != "trellis-phase0-semantic-bundle/v1":
        raise DerivationBuildFailure("sealed Phase-0 generation lacks its semantic bundle")
    if bundle.get("semantic_bundle_sha256") != _self_digest(
        bundle, "semantic_bundle_sha256"
    ):
        raise DerivationBuildFailure("Phase-0 semantic-bundle self digest is invalid")
    if bundle.get("goal_binding_report") != binding:
        raise DerivationBuildFailure(
            "GOAL-binding report differs from the sealed Phase-0 projection"
        )
    if binding.get("report_sha256") != _self_digest(binding, "report_sha256"):
        raise DerivationBuildFailure("GOAL-binding report self digest is invalid")
    bound_targets = binding.get("targets")
    if not isinstance(bound_targets, list) or not bound_targets:
        raise DerivationBuildFailure("sealed GOAL-binding report has no targets")
    target_projection: list[dict[str, str]] = []
    for target in bound_targets:
        if (
            not isinstance(target, dict)
            or target.get("status") != "bound"
            or not isinstance(target.get("matches"), list)
            or len(target["matches"]) != 1
        ):
            raise DerivationBuildFailure("sealed GOAL-binding report is not complete")
        target_id = target.get("target_id")
        resolution = target.get("resolution")
        if not isinstance(target_id, str) or resolution not in {"decide", "prove"}:
            raise DerivationBuildFailure("sealed GOAL-binding target is malformed")
        target_projection.append({"target_id": target_id, "resolution": resolution})
    if target_projection != sorted(target_projection, key=lambda row: row["target_id"].encode()):
        raise DerivationBuildFailure("sealed GOAL-binding targets are not canonical")

    ledger = bundle.get("ledger")
    if not isinstance(ledger, dict) or ledger.get("schema") != "trellis-phase0-adaptation-ledger/v1":
        raise DerivationBuildFailure("sealed semantic bundle lacks its Phase-0 ledger")
    if ledger.get("ledger_sha256") != _self_digest(ledger, "ledger_sha256"):
        raise DerivationBuildFailure("sealed Phase-0 ledger self digest is invalid")
    entries = ledger.get("entries")
    if not isinstance(entries, list):
        raise DerivationBuildFailure("sealed Phase-0 ledger entries are malformed")
    rows: list[dict[str, object]] = []
    empty_sha256 = hashlib.sha256(b"").hexdigest()
    for entry in entries:
        if not isinstance(entry, dict):
            raise DerivationBuildFailure("sealed Phase-0 ledger entry is malformed")
        after = entry.get("after_sha256")
        rows.append(
            {
                "id": entry.get("id"),
                "seam_class": entry.get("seam_class"),
                "paths": [
                    {
                        "path": entry.get("file"),
                        **(
                            {"before_sha256": entry["before_sha256"]}
                            if entry.get("before_sha256") is not None
                            else {}
                        ),
                        # The later compact ledger predates deletion. Its
                        # deterministic deleted-file sentinel is sha256(empty);
                        # the authoritative operation/bytes remain in Phase 0.
                        "after_sha256": after if after is not None else empty_sha256,
                    }
                ],
                "citation": entry.get("citation"),
                "affected_targets": entry.get("affected_targets"),
                "meaning_change": entry.get("meaning_change"),
                "status": "seed",
            }
        )
    return rows, target_projection


def build_prose_plan(
    targets_path: Path, sealed_path: Path, binding_path: Path
) -> dict[str, object]:
    ledger, sealed_targets = _sealed_projection(sealed_path, binding_path)
    configured_targets = [
        {"target_id": f"goal:{name}", "resolution": resolution}
        for name, resolution in _prose_targets(targets_path)
    ]
    if configured_targets != sealed_targets:
        raise DerivationBuildFailure(
            "production targets differ from the sealed GOAL-binding projection"
        )
    return {
        "schema": PLAN_SCHEMA,
        "adaptation_ledger": ledger,
        "targets": configured_targets,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--prose-targets", required=True, type=Path)
    parser.add_argument("--sealed-generation", required=True, type=Path)
    parser.add_argument("--goal-binding-report", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if args.out.exists():
        raise SystemExit(f"refusing to replace existing plan: {args.out}")
    try:
        plan = build_prose_plan(
            args.prose_targets, args.sealed_generation, args.goal_binding_report
        )
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_bytes(canonical_bytes(plan))
    except DerivationBuildFailure as exc:
        raise SystemExit(f"build_failed: {exc}") from exc


if __name__ == "__main__":
    main()

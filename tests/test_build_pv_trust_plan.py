"""Target-resolution plan guards for prose campaigns."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

from scripts.build_pv_trust_plan import (
    PLAN_SCHEMA,
    DerivationBuildFailure,
    build_prose_plan,
    canonical_bytes,
)
from scripts.pv_source_tree import tagged_hash


ROOT = Path(__file__).resolve().parents[1]


def _registry(tmp_path: Path, entries: list[object]) -> Path:
    path = tmp_path / "pv_tablet.json"
    path.write_text(json.dumps({"verification_targets": entries}), encoding="utf-8")
    return path


def _phase0_projection(
    tmp_path: Path,
    targets: list[tuple[str, str]],
    entries: list[dict[str, object]] | None = None,
) -> tuple[Path, Path]:
    binding: dict[str, object] = {
        "schema": "trellis-phase0-goal-binding-report/v1",
        "goal_sha256": "1" * 64,
        "adapted_tree_sha256": "2" * 64,
        "targets": [
            {
                "target_id": f"goal:{name}",
                "resolution": resolution,
                "status": "bound",
                "matches": [{"adapted_item_path": f"fixture::{name}"}],
            }
            for name, resolution in sorted(targets)
        ],
        "report_sha256": "0" * 64,
    }
    binding["report_sha256"] = tagged_hash(
        "raw-artifact",
        canonical_bytes({key: value for key, value in binding.items() if key != "report_sha256"}),
    )
    ledger: dict[str, object] = {
        "schema": "trellis-phase0-adaptation-ledger/v1",
        "generation": 1,
        "unadapted_tree_sha256": "3" * 64,
        "adapted_tree_sha256": "2" * 64,
        "goal_sha256": "1" * 64,
        "entries": entries or [],
        "ledger_entries_root": "4" * 64,
        "ledger_sha256": "0" * 64,
    }
    ledger["ledger_sha256"] = tagged_hash(
        "raw-artifact",
        canonical_bytes({key: value for key, value in ledger.items() if key != "ledger_sha256"}),
    )
    bundle: dict[str, object] = {
        "schema": "trellis-phase0-semantic-bundle/v1",
        "ledger": ledger,
        "goal_binding_report": binding,
        "semantic_bundle_sha256": "0" * 64,
    }
    bundle["semantic_bundle_sha256"] = tagged_hash(
        "raw-artifact",
        canonical_bytes({key: value for key, value in bundle.items() if key != "semantic_bundle_sha256"}),
    )
    sealed: dict[str, object] = {
        "schema": "trellis-phase0-sealed-generation/v1",
        "semantic_bundle": bundle,
        "sealed_generation_sha256": "0" * 64,
    }
    sealed["sealed_generation_sha256"] = tagged_hash(
        "raw-artifact",
        canonical_bytes({key: value for key, value in sealed.items() if key != "sealed_generation_sha256"}),
    )
    sealed_path = tmp_path / "sealed.json"
    binding_path = tmp_path / "binding.json"
    sealed_path.write_bytes(canonical_bytes(sealed))
    binding_path.write_bytes(canonical_bytes(binding))
    return sealed_path, binding_path


def test_builder_emits_only_sealed_ledger_and_goal_owned_targets(tmp_path: Path) -> None:
    path = _registry(
        tmp_path,
        [
            {"name": "zeta", "resolution": "prove"},
            {"name": "alpha", "resolution": "decide"},
        ],
    )
    sealed, binding = _phase0_projection(
        tmp_path, [("zeta", "prove"), ("alpha", "decide")]
    )
    assert build_prose_plan(path, sealed, binding) == {
        "schema": PLAN_SCHEMA,
        "adaptation_ledger": [],
        "targets": [
            {"target_id": "goal:alpha", "resolution": "decide"},
            {"target_id": "goal:zeta", "resolution": "prove"},
        ],
    }


def test_builder_rejects_production_targets_outside_the_sealed_projection(
    tmp_path: Path,
) -> None:
    sealed, binding = _phase0_projection(tmp_path, [("alpha", "decide")])
    with pytest.raises(
        DerivationBuildFailure,
        match="production targets differ from the sealed GOAL-binding projection",
    ):
        build_prose_plan(
            _registry(tmp_path, [{"name": "beta", "resolution": "decide"}]),
            sealed,
            binding,
        )


def test_name_only_and_missing_resolution_default_to_decide(tmp_path: Path) -> None:
    path = _registry(tmp_path, ["first", {"name": "second"}])
    sealed, binding = _phase0_projection(
        tmp_path, [("first", "decide"), ("second", "decide")]
    )
    assert build_prose_plan(path, sealed, binding)["targets"] == [
        {"target_id": "goal:first", "resolution": "decide"},
        {"target_id": "goal:second", "resolution": "decide"},
    ]


def test_lock_shape_and_effect_data_cannot_change_decide(tmp_path: Path) -> None:
    path = _registry(
        tmp_path,
        [
            {
                "name": "target",
                "resolution": "decide",
                "lockfile": None,
                "callable_shape": "unsupported",
                "effect_summary": {"determinism": "not_established"},
            }
        ],
    )
    sealed, binding = _phase0_projection(tmp_path, [("target", "decide")])
    assert build_prose_plan(path, sealed, binding)["targets"] == [
        {"target_id": "goal:target", "resolution": "decide"}
    ]


@pytest.mark.parametrize(
    "entries",
    [
        [],
        [{"name": "bad-name"}],
        [{"name": "ok", "resolution": "conditional"}],
        [{"name": "same"}, {"name": "same"}],
        [42],
    ],
)
def test_invalid_registry_is_rejected(tmp_path: Path, entries: list[object]) -> None:
    sealed, binding = _phase0_projection(tmp_path, [("fixture", "decide")])
    with pytest.raises(DerivationBuildFailure):
        build_prose_plan(_registry(tmp_path, entries), sealed, binding)


def test_cli_accepts_only_prose_targets_and_out(tmp_path: Path) -> None:
    targets = _registry(tmp_path, [{"name": "alpha", "resolution": "decide"}])
    sealed, binding = _phase0_projection(tmp_path, [("alpha", "decide")])
    out = tmp_path / "plan.json"
    subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts" / "build_pv_trust_plan.py"),
            "--prose-targets",
            str(targets),
            "--sealed-generation",
            str(sealed),
            "--goal-binding-report",
            str(binding),
            "--out",
            str(out),
        ],
        check=True,
    )
    assert out.read_bytes() == canonical_bytes(build_prose_plan(targets, sealed, binding))

    failed = subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts" / "build_pv_trust_plan.py"),
            "--prose-targets",
            str(targets),
            "--sealed-generation",
            str(sealed),
            "--goal-binding-report",
            str(binding),
            "--callable-manifest",
            str(tmp_path / "removed.json"),
            "--out",
            str(tmp_path / "other.json"),
        ],
        text=True,
        capture_output=True,
    )
    assert failed.returncode != 0
    assert "unrecognized arguments: --callable-manifest" in failed.stderr

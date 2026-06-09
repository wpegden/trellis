#!/usr/bin/env python3
"""Estimate the runtime impact of StuckMathAudit cone-cleaning a node."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any


HISTORY_PATH = ".trellis-history/supervisor_state.json"
BASELINE_SCAN_LIMIT = 5000
PREAMBLE_NAME = "Preamble"
AXIOMS_NAME = "Axioms"
HEADER_NAME = "header"


def load_json(path: Path) -> dict[str, Any]:
    with path.open() as fh:
        value = json.load(fh)
    if not isinstance(value, dict):
        raise ValueError(f"{path} did not contain a JSON object")
    return value


def as_set(value: Any) -> set[str]:
    if value is None:
        return set()
    if isinstance(value, dict):
        return {str(k) for k in value}
    if isinstance(value, list):
        return {str(item) for item in value}
    raise ValueError(f"expected list/dict/set-like value, got {type(value).__name__}")


def map_of_sets(value: Any) -> dict[str, set[str]]:
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise ValueError(f"expected object map, got {type(value).__name__}")
    return {str(k): as_set(v) for k, v in value.items()}


def git(repo: Path, args: list[str]) -> str:
    proc = subprocess.run(
        ["git", "--no-optional-locks", "-C", str(repo), *args],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"git {' '.join(args)} failed with exit {proc.returncode}: {proc.stderr.strip()}"
        )
    return proc.stdout


def phase_is(state: dict[str, Any], *names: str) -> bool:
    phase = str(state.get("phase", ""))
    normalized = phase.replace("-", "_").lower()
    return normalized in {name.lower() for name in names} or phase in names


def recover_theorem_stating_baseline(repo: Path) -> tuple[str, dict[str, Any]]:
    log = git(
        repo,
        [
            "log",
            "--format=%H",
            f"--max-count={BASELINE_SCAN_LIMIT}",
            "--",
            HISTORY_PATH,
        ],
    )
    candidate: tuple[str, dict[str, Any]] | None = None
    for sha in [line.strip() for line in log.splitlines() if line.strip()]:
        try:
            raw = git(repo, ["show", f"{sha}:{HISTORY_PATH}"])
            parsed = json.loads(raw)
        except Exception:
            continue
        state = parsed.get("state") if isinstance(parsed, dict) else None
        if not isinstance(state, dict):
            continue
        if phase_is(state, "ProofFormalization", "proof_formalization") and as_set(
            state.get("coarse_dag_nodes")
        ):
            candidate = (sha, state)
            continue
        if candidate is not None and phase_is(state, "TheoremStating", "theorem_stating"):
            break
    if candidate is None:
        raise RuntimeError(
            f"could not recover theorem-stating baseline from {HISTORY_PATH} history"
        )
    return candidate


def extract_tablet_imports(text: str) -> set[str]:
    imports: set[str] = set()
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("import Tablet."):
            suffix = stripped.removeprefix("import Tablet.").strip()
            if suffix:
                imports.add(suffix)
    return imports


def current_present_nodes(repo: Path, context: dict[str, Any]) -> set[str]:
    from_context = as_set(context.get("current_present_nodes"))
    if from_context:
        return from_context
    tablet = repo / "Tablet"
    if not tablet.is_dir():
        return set()
    names: set[str] = set()
    for path in tablet.iterdir():
        if path.suffix == ".lean" and path.stem != AXIOMS_NAME:
            names.add(path.stem)
        elif path.suffix == ".tex" and path.stem != HEADER_NAME:
            names.add(path.stem)
    return names


def deps_after_restore(repo: Path, present: set[str], baseline_commit: str, node: str) -> dict[str, set[str]]:
    deps: dict[str, set[str]] = {}
    for name in present:
        if name == node:
            try:
                text = git(repo, ["show", f"{baseline_commit}:Tablet/{node}.lean"])
            except RuntimeError:
                text = ""
        else:
            path = repo / "Tablet" / f"{name}.lean"
            text = path.read_text() if path.exists() else ""
        deps[name] = {dep for dep in extract_tablet_imports(text) if dep != name}
    return deps


def retain_target_claims(
    target_claims: dict[str, set[str]], present: set[str], configured_targets: set[str]
) -> dict[str, set[str]]:
    retained: dict[str, set[str]] = {}
    for node, targets in target_claims.items():
        if node not in present:
            continue
        filtered = targets & configured_targets
        if filtered:
            retained[node] = filtered
    return retained


def coverage_from_claims(
    configured_targets: set[str], target_claims: dict[str, set[str]], present: set[str]
) -> dict[str, set[str]]:
    coverage = {target: set() for target in configured_targets}
    for node, targets in target_claims.items():
        if node not in present:
            continue
        for target in targets:
            if target in coverage:
                coverage[target].add(node)
    return coverage


def dep_closure(roots: set[str], present: set[str], deps: dict[str, set[str]]) -> set[str]:
    closure = {node for node in roots if node in present}
    frontier = list(closure)
    while frontier:
        node = frontier.pop()
        for dep in deps.get(node, set()):
            if dep in present and dep not in closure:
                closure.add(dep)
                frontier.append(dep)
    return closure


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--context-json", required=True, type=Path)
    parser.add_argument("--node", required=True)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    args = parser.parse_args()

    context = load_json(args.context_json)
    node = args.node
    repo = args.repo
    present = current_present_nodes(repo, context)
    allowed = as_set(context.get("resettable_theorem_stating_nodes"))
    if allowed and node not in allowed:
        print(
            json.dumps(
                {
                    "ok": False,
                    "node": node,
                    "error": "node is not in resettable_theorem_stating_nodes",
                    "resettable_theorem_stating_nodes": sorted(allowed),
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 2

    baseline_commit, baseline_state = recover_theorem_stating_baseline(repo)
    baseline_present = as_set(baseline_state.get("live", {}).get("present_nodes"))
    if node not in baseline_present:
        print(
            json.dumps(
                {
                    "ok": False,
                    "node": node,
                    "baseline_commit": baseline_commit,
                    "error": "node is absent from theorem-stating baseline",
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 2

    current_claims = map_of_sets(context.get("current_target_claims"))
    baseline_claims = map_of_sets(baseline_state.get("target_claims"))
    target_claims = dict(current_claims)
    if node in baseline_claims:
        target_claims[node] = set(baseline_claims[node])
    else:
        target_claims.pop(node, None)
    configured_targets = as_set(context.get("configured_targets"))
    target_claims = retain_target_claims(target_claims, present, configured_targets)
    coverage = coverage_from_claims(configured_targets, target_claims, present)
    deps = deps_after_restore(repo, present, baseline_commit, node)
    roots = set().union(*coverage.values()) if coverage else set()
    supported = dep_closure(roots, present, deps)
    pruned = sorted(name for name in present if name != PREAMBLE_NAME and name not in supported)

    current_node_path = repo / "Tablet" / f"{node}.lean"
    current_imports = (
        extract_tablet_imports(current_node_path.read_text()) if current_node_path.exists() else set()
    )
    baseline_imports = extract_tablet_imports(
        git(repo, ["show", f"{baseline_commit}:Tablet/{node}.lean"])
    )
    print(
        json.dumps(
            {
                "ok": True,
                "node": node,
                "baseline_commit": baseline_commit,
                "current_imports": sorted(current_imports),
                "baseline_imports": sorted(baseline_imports),
                "target_claims_after_restore": {
                    key: sorted(value) for key, value in sorted(target_claims.items())
                },
                "roots_after_restore": sorted(roots),
                "supported_nodes_after_restore": sorted(supported),
                "pruned_nodes": pruned,
            },
            indent=2,
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(json.dumps({"ok": False, "error": str(exc)}, indent=2, sort_keys=True))
        raise SystemExit(1)

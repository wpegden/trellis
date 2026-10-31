"""Run revision-task checks in the existing sealed source-checker sandbox."""
from __future__ import annotations

import os
import hashlib
from pathlib import Path
import shutil
import time

from trellis.config import SandboxConfig
from trellis.json_io import save_json
from trellis.modeling.environment import run_source_command, source_environment, tool_readonly_paths
from trellis.pv_execution import execution_timeout
from trellis.sandbox import wrap_command


class ProductionChecker:
    def __init__(self, root: Path, tools: dict):
        self.root, self.tools = Path(root), tools

    def __call__(self, source: Path, attempt: dict) -> dict:
        from trellis.pv_revision.store import source_manifest, select_manifest, harness_paths, copy_manifest_files, _read

        checks = attempt["authorization"].get("checks", [])
        authorizations = [attempt['authorization']]
        state_path = self.root / 'state.json'
        if state_path.exists():
            state = _read(state_path)
            authorizations.extend(task['authorization'] for task in state['tasks'].values())
        evidence_paths = harness_paths(authorizations)
        original_paths = {row['relative_path'] for row in source_manifest(self.root / 'sources/original')['files']}
        if evidence_paths & original_paths:
            raise ValueError('Evidence/harness paths include original production files; use separate copies')
        identity = hashlib.sha256(str(attempt['id']).encode()).hexdigest()[:12]
        output = self.root / "checks" / f"check-{identity}-{time.time_ns()}"
        output.mkdir(parents=True)
        result = {"ok": bool(checks), "checks": [], "artifacts": {}, "source": source_manifest(source),
                  "reason": "" if checks else "The task needs executable checks before source approval."}
        for index, check in enumerate(checks):
            argv = check.get("argv")
            if not isinstance(argv, list) or not argv or any(not isinstance(s, str) or not s for s in argv):
                raise ValueError("Task checks require an executable argument array")
            baseline = (self.root / attempt["base_source"]).resolve()
            if not baseline.is_relative_to(self.root.resolve() / "sources"):
                raise ValueError("Regression baseline leaves the revision source snapshots")
            for side, original, expected in (
                ("candidate", source, check.get("expected_exit", 0)),
                ("baseline", baseline, check.get("baseline_expected_exit")),
            ):
                if expected is None:
                    continue
                if not isinstance(expected, int) or isinstance(expected, bool):
                    raise ValueError("Expected check exit codes must be integers")
                work = output / f"{index}-{side}"
                home = work / "home"
                home.mkdir(parents=True)
                checked_manifest = source_manifest(original)
                production = select_manifest(checked_manifest, {
                    row['relative_path'] for row in checked_manifest['files']} - evidence_paths)
                copy_manifest_files(original, home / "source", production)
                harness = home / "harness"
                harness.mkdir()
                for relative in check.get("harness_paths", []):
                    if not isinstance(relative, str) or Path(relative).is_absolute() or ".." in Path(relative).parts:
                        raise ValueError("Check harness paths must name confined candidate files")
                    path = (source / relative).resolve()
                    if not path.is_relative_to(source.resolve()) or not path.is_file():
                        raise ValueError("Check harness paths must name candidate source files")
                    target = harness / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(path, target)
                env = source_environment(self.tools, home, base_env={})
                # No shell interpolation: placeholders are substituted within
                # individual arguments and all execution is inside bubblewrap.
                command = [s.replace("{source}", str(home / "source"))
                           .replace("{scratch}", str(home)).replace("{harness}", str(harness)) for s in argv]
                previous = os.environ.get("TRELLIS_SANDBOX_EXTRA_RO_PATHS")
                try:
                    os.environ["TRELLIS_SANDBOX_EXTRA_RO_PATHS"] = os.pathsep.join(tool_readonly_paths(self.tools))
                    inner = ["/usr/bin/env", *[f"{k}={v}" for k, v in env.items()
                        if k in {"PATH", "LEAN_PATH", "RUSTC", "RUSTDOC", "RUSTUP_TOOLCHAIN", "CARGO_HOME", "CARGO_TARGET_DIR", "TMPDIR", "CHARON_TOOLCHAIN_IS_IN_PATH"}], *command]
                    wrapped = wrap_command(inner, sandbox=SandboxConfig(enabled=True, backend="bwrap"),
                                           work_dir=work, burst_home=home, role="source_adaptation_checker")
                finally:
                    if previous is None:
                        os.environ.pop("TRELLIS_SANDBOX_EXTRA_RO_PATHS", None)
                    else:
                        os.environ["TRELLIS_SANDBOX_EXTRA_RO_PATHS"] = previous
                record = run_source_command(wrapped, cwd=work, env={}, stdout=work / "stdout",
                                            stderr=work / "stderr", timeout=execution_timeout(self.tools, "revision_check"))
                record.update(id=check["id"], side=side, expected_exit=expected,
                              production_manifest=production,
                              harness_manifest=source_manifest(harness),
                              matched=record["exit_code"] == expected and not record["timeout"])
                for stream in ("stdout", "stderr"):
                    path = Path(record[stream])
                    result["artifacts"][str(path)] = hashlib.sha256(path.read_bytes()).hexdigest()
                result["checks"].append(record)
                result["ok"] &= record["matched"]
        save_json(output / "result.json", result)
        return {**result, "evidence": str(output / "result.json"),
                "evidence_sha256": hashlib.sha256((output / "result.json").read_bytes()).hexdigest()}

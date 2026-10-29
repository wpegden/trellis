"""Run an agent-supplied observation command on separate source snapshots."""
from __future__ import annotations

import filecmp
from pathlib import Path
import shutil

from trellis.modeling.observations import fingerprint, write_json
from scripts.extract_pv_model import copy_source_inputs
from trellis.modeling.environment import run_source_command, source_environment
from trellis.pv_execution import execution_timeout


def compare(baseline: Path, candidate: Path, argv: list[str], output: Path, *,
            timeout: float | None = None, tools: dict | None = None) -> dict:
    if not argv:
        raise ValueError("Supply an observation command with a {source} argument")
    if not any("{source}" in argument for argument in argv):
        raise ValueError("The comparison command must identify its input using {source}")
    timeout = execution_timeout(tools or {}, "compare", timeout, default=60)
    output.mkdir(parents=True)
    outcomes = {}
    for name, source in (("baseline", baseline), ("candidate", candidate)):
        folder = output / name
        snapshot = folder / "source"
        copy_source_inputs(source, snapshot)
        arguments = [argument.replace("{source}", str(snapshot)).replace("{scratch}", str(folder)) for argument in argv]
        result = run_source_command(arguments, cwd=folder, env=source_environment(tools or {}, folder),
                                    stdout=folder / "stdout", stderr=folder / "stderr", timeout=timeout)
        outcomes[name] = {"source": str(source), "fingerprint": fingerprint(source), **result}
    a, b = outcomes.values()
    outcomes["commands_succeeded"] = all(item["exit_code"] == 0 and not item["timeout"] for item in (a, b))
    outcomes["same_observation"] = (outcomes["commands_succeeded"]
                                    and filecmp.cmp(a["stdout"], b["stdout"], shallow=False))
    outcomes["comparison_file"] = str(output / "comparison.json")
    write_json(output / "comparison.json", outcomes)
    return outcomes

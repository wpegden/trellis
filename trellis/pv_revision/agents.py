"""Revision roles through the ordinary sandboxed agent request executor."""
from dataclasses import replace
import json
import os
from pathlib import Path

from trellis.agent_wrapper.executor import execute_agent_request, prepare_artifact_paths
from trellis.agent_wrapper.protocol import AgentLane, ArtifactSpec
from trellis.config import load_config, _parse_provider_config
from trellis.json_io import save_json
from trellis.runtime.bridge import _single_request_common, any_halt_marker_present

FRAGMENTS = Path(__file__).resolve().parents[1] / "prompt_fragments/pv/revision"


class Agents:
    def __call__(self, root, state, task):
        role = task["role"]
        runtime = root.with_name(root.name + "-runtime")
        if any_halt_marker_present(runtime):
            raise RuntimeError("Revision agent runtime is halted for operator review")
        config = load_config(root / "trellis.config.json")
        if not config.sandbox.enabled:
            raise ValueError("Revision agents require the filesystem sandbox")
        paths = ()
        if role == "worker":
            path = Path(task["writable_source"]).resolve()
            if not path.is_relative_to(root / "sources") or path == (root / state["source"]):
                raise ValueError("Worker source is not an isolated candidate")
            paths = (str(path.relative_to(root)),)
        config = replace(config, sandbox=replace(config.sandbox, extra_writable_paths=paths))
        inputs = json.loads((root / "trellis.config.json").read_text())
        binding = "worker" if role == "worker" else "audit" if role in {"audit", "scope", "aggregate_scope", "comment_scope"} else "reviewer"
        provider_raw = inputs.get("preparation", {}).get(binding)
        provider = (_parse_provider_config(provider_raw, f"preparation.{binding}") if provider_raw
                    else config.worker if role == "worker" else config.reviewer)
        name = f"revision-{state['sequence']:05d}-{role}"
        # The reviewer sandbox role grants scratch/staging and only the explicit
        # candidate overlay. The worker lane independently selects authoring base
        # instructions: reviewer base instructions prohibit all source edits.
        request = _single_request_common(
            config=config, runtime_root=runtime,
            request={"kind": "review", "id": state["sequence"], "cycle": state["sequence"],
                     "phase": "revision", "fresh_context": True},
            provider=provider, lane=AgentLane(kind="worker" if role == "worker" else "reviewer"), kind_label=role,
            burst_role="reviewer", prompt="",
            artifact=ArtifactSpec(canonical_name=name + ".json", kind=f"pv-revision-{role}-result", phase="revision"))
        artifacts = prepare_artifact_paths(request.state_dir, root, request.artifact.canonical_name)
        context = root / "requests" / f"{state['sequence']:05d}-{role}" / "context.json"
        prompt = ((FRAGMENTS / "common.md").read_text() + "\n" if role != "comment_scope" else "") + (FRAGMENTS / f"{role}.md").read_text()
        prompt += f"\nRead complete immutable request context: {context}.\n"
        prompt += f"Write response JSON to {artifacts.raw}, then create {artifacts.done} as your final filesystem change.\n"
        if role == "worker":
            prompt += f"Your only writable candidate source: {task['writable_source']}.\n"
        request = replace(request, prompt=prompt)
        save_json(context.parent / "request.json", request.to_dict())
        (context.parent / "prompt.txt").write_text(prompt)
        from trellis.modeling.environment import tool_readonly_paths
        references = state.get("handoff", {}).get("reference_paths", [])
        env = {"TRELLIS_SANDBOX_EXTRA_RO_PATHS": os.pathsep.join([
            *tool_readonly_paths(state.get("tools", {})), *references]),
            "TRELLIS_KERNEL_CACHE_ROOT": str(runtime)}
        previous = {key: os.environ.get(key) for key in env}
        os.environ.update(env)
        try:
            response = execute_agent_request(request)
        finally:
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
        record = response.to_dict()
        record.pop("captured_output", None)
        save_json(context.parent / "execution.json", record)
        if not response.ok:
            raise RuntimeError(f"{name}: {response.error}; logs: {request.log_dir}")
        if any_halt_marker_present(runtime) and not response.payload.get("system_feedback", "").strip():
            raise RuntimeError("Revision agent runtime halted; inspect logs before recovery")
        return response.payload

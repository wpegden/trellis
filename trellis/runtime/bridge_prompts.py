"""Prompt builders for the trellis bridge.

These are intentionally new prompt assemblers. They reuse a small amount of
verbatim instructional text from the existing prompt assets, but they do not
call older prompt-builder code paths.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping

from trellis.history_artifacts import (
    last_invalid_dir,
    last_invalid_metadata_path,
)
from trellis.project_paths import repo_tmp_subdir
from trellis.worker_scratch import (
    worker_scratch_dir,
    worker_scratch_example_path,
    worker_scratch_notes_path,
    worker_scratch_readme_path,
)

PROMPT_FRAGMENT_ROOT = Path(__file__).resolve().parent.parent / "prompt_fragments"
PROMPT_SCHEME_REFERENCE_PATH = (
    PROMPT_FRAGMENT_ROOT / "common" / "TRELLIS_FORMALIZATION_SCHEME.md"
).resolve()
CHECKER_MISMATCH_REJECTION_PREFIX = "authoritative checker mismatch:"


def _filespec_path(repo_path: Path) -> Path:
    repo_path = repo_path.resolve()
    repo_local = repo_path / "FILESPEC.md"
    if repo_local.exists():
        return repo_local
    runtime_copy = repo_path / ".trellis" / "runtime" / "src" / "FILESPEC.md"
    if runtime_copy.exists():
        return runtime_copy
    return repo_local


def _scheme_reference_path(repo_path: Path) -> Path:
    """Prefer the in-repo runtime-snapshot copy over the supervisor-side
    source path. Inside the bwrap sandbox the supervisor's home isn't
    bind-mounted; only `<repo>/.trellis/runtime/src/...` is reachable.
    Embedding the supervisor path made every reviewer file `system_feedback`
    saying the path didn't exist."""
    repo_path = repo_path.resolve()
    runtime_copy = (
        repo_path
        / ".trellis" / "runtime" / "src" / "trellis"
        / "prompt_fragments" / "common"
        / "TRELLIS_FORMALIZATION_SCHEME.md"
    )
    if runtime_copy.exists():
        return runtime_copy
    return PROMPT_SCHEME_REFERENCE_PATH


def _loogle_helper_path(repo_path: Path) -> Path:
    return repo_path.resolve() / ".trellis" / "runtime" / "src" / "scripts" / "loogle_json.sh"


def _loogle_enabled(repo_path: Path) -> bool:
    """Whether a local Loogle server is configured for this project.

    Reads ``loogle.enabled`` from the project's ``trellis.config.json`` (same
    direct-read pattern as ``_paper_tex_path``). Defaults to ``True`` when the
    key is absent so runs that predate the knob keep their Loogle guidance;
    ``setup_repo.sh`` writes an explicit value into freshly-generated configs.
    When false, the worker prompt omits the Loogle helper fragment so the
    worker isn't told to use a server that isn't running.
    """
    config_file = repo_path.resolve() / "trellis.config.json"
    try:
        raw = json.loads(config_file.read_text(encoding="utf-8"))
    except Exception:
        return True
    loogle = raw.get("loogle")
    if isinstance(loogle, dict):
        return bool(loogle.get("enabled", True))
    return True


def _paper_tex_path(repo_path: Path) -> str:
    """Resolve the configured paper .tex path relative to repo root.

    Reads `workflow.paper_tex_path` from `trellis.config.json` (the
    canonical key per `trellis/config.py`); falls back to a top-level
    `paper_tex_path` if some caller stored it there; otherwise returns
    a sentinel so prompt readers see the indirection failed loudly.
    Returned value is a string relative path (e.g. "paper/reference.tex")
    so it inlines cleanly in prompt text.
    """
    config_file = repo_path.resolve() / "trellis.config.json"
    try:
        raw = json.loads(config_file.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return "<paper_tex_path not resolvable: trellis.config.json unreadable>"
    workflow = raw.get("workflow") if isinstance(raw, dict) else None
    if isinstance(workflow, dict):
        value = workflow.get("paper_tex_path")
        if isinstance(value, str) and value.strip():
            return value.strip()
    if isinstance(raw, dict):
        value = raw.get("paper_tex_path")
        if isinstance(value, str) and value.strip():
            return value.strip()
    return "<paper_tex_path not configured>"


# Reviewer-selected paper-fragment inline budget. The full cited text is
# always written to a sidecar on disk; these caps bound only the inline
# excerpt rendered into the worker prompt so a runaway chapter-sized
# citation cannot drown out the rest of the worker context.
PAPER_FOCUS_INLINE_MAX_CHARS = 24000
PAPER_FOCUS_INLINE_MAX_LINES = 300
STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES_DEFAULT = 200
STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES_ENV = (
    "TRELLIS_STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES"
)

# Process Issue 4 (2026-05-22): the kernel owns blocker-count gating and the
# decision whether to overflow into a sidecar. As of the 2026-06-04 bridge-to-
# kernel migration (batch 2) the inline-limit / fallback-K constants live in
# the kernel (`worker_blocker_inline_limit` in `kernel/src/request_contracts.rs`)
# and the bridge only writes the kernel-rendered sidecar payload to disk.


def _paper_focus_ranges_from_worker_contract(
    worker_contract: Mapping[str, Any],
) -> List[Dict[str, Any]]:
    request_summary = contract_request_summary(worker_contract)
    worker_context = request_summary.get("worker_context")
    if not isinstance(worker_context, Mapping):
        return []
    ranges = worker_context.get("paper_focus_ranges")
    if not isinstance(ranges, list):
        return []
    return [dict(r) for r in ranges if isinstance(r, Mapping)]


def _resolved_paper_tex_file(repo_path: Path) -> Path:
    rel = _paper_tex_path(repo_path)
    if rel.startswith("<"):
        raise ValueError(f"cannot extract paper_focus_ranges: {rel}")
    root = repo_path.resolve()
    path = (root / rel).resolve()
    if path != root and root not in path.parents:
        raise ValueError(f"configured paper_tex_path escapes repo root: {rel}")
    if not path.is_file():
        raise ValueError(f"configured paper_tex_path does not exist: {rel}")
    return path


def _format_paper_focus_full_text(
    repo_path: Path, ranges: List[Dict[str, Any]]
) -> str:
    paper_path = _resolved_paper_tex_file(repo_path)
    lines = paper_path.read_text(encoding="utf-8").splitlines()
    rel_source = paper_path.relative_to(repo_path.resolve())
    parts: List[str] = []
    for idx, raw in enumerate(ranges, start=1):
        try:
            start = int(raw.get("start_line", 0) or 0)
            end = int(raw.get("end_line", 0) or 0)
        except (TypeError, ValueError) as exc:
            raise ValueError(
                f"invalid paper_focus_ranges[{idx - 1}]: start_line/end_line must be integers"
            ) from exc
        reason = str(raw.get("reason", "") or "").strip()
        if start < 1 or end < start:
            raise ValueError(
                f"invalid paper_focus_ranges[{idx - 1}]: "
                f"start_line={start}, end_line={end} (require start_line >= 1 and end_line >= start_line)"
            )
        if end > len(lines):
            raise ValueError(
                f"paper_focus_ranges[{idx - 1}] ends at line {end}, "
                f"but {rel_source} has {len(lines)} lines"
            )
        parts.append(f"### Fragment {idx}: {rel_source}:{start}-{end}")
        if reason:
            parts.append(f"Reason: {reason}")
        parts.append("")
        for lineno in range(start, end + 1):
            parts.append(f"{lineno:>5}: {lines[lineno - 1]}")
        parts.append("")
    return "\n".join(parts).rstrip() + "\n"


def _paper_focus_sidecar_path(raw_output_path: Path) -> Path:
    return raw_output_path.with_name(
        f"{raw_output_path.stem}.paper_focus_fragments.md"
    )


def _truncate_paper_focus_inline(full_text: str) -> tuple[str, bool]:
    lines = full_text.splitlines()
    line_truncated = len(lines) > PAPER_FOCUS_INLINE_MAX_LINES
    capped = "\n".join(lines[:PAPER_FOCUS_INLINE_MAX_LINES])
    char_truncated = False
    if len(capped) > PAPER_FOCUS_INLINE_MAX_CHARS:
        capped = capped[:PAPER_FOCUS_INLINE_MAX_CHARS].rstrip()
        char_truncated = True
    truncated = line_truncated or char_truncated
    rendered = capped.rstrip()
    if rendered:
        rendered += "\n"
    return rendered, truncated


def _stuck_math_audit_report_prompt_max_lines() -> int:
    raw = os.environ.get(STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES_ENV, "").strip()
    if raw:
        try:
            return max(1, int(raw))
        except ValueError:
            pass
    return STUCK_MATH_AUDIT_REPORT_PROMPT_MAX_LINES_DEFAULT


def _safe_path_label(raw: Any) -> str:
    text = str(raw if raw is not None else "unknown").strip() or "unknown"
    safe = "".join(ch if ch.isalnum() or ch in {"-", "_"} else "_" for ch in text)
    return safe or "unknown"


def _stuck_math_audit_report_path(
    *,
    repo_path: Path,
    request: Mapping[str, Any],
    audit_plan: Mapping[str, Any],
) -> Path:
    cycle = audit_plan.get("written_at_cycle") or request.get("cycle") or "unknown"
    request_id = audit_plan.get("written_by_request") or request.get("id") or "unknown"
    return (
        repo_path
        / ".trellis"
        / "stuck-math-audit"
        / f"cycle-{_safe_path_label(cycle)}-request-{_safe_path_label(request_id)}"
        / "audit_report.md"
    )


def _existing_reviewer_notes(report_path: Path) -> str:
    try:
        existing = report_path.read_text(encoding="utf-8")
    except OSError:
        return ""
    marker = "# Reviewer Notes"
    idx = existing.rfind(marker)
    if idx < 0:
        return ""
    notes = existing[idx + len(marker) :].lstrip("\r\n")
    return notes.rstrip() + "\n" if notes.strip() else ""


def _audit_plan_tasks_markdown(audit_plan: Mapping[str, Any]) -> str:
    tasks = audit_plan.get("tasks")
    if not isinstance(tasks, list) or not tasks:
        return "(no audit tasks)\n"
    return f"```json\n{json.dumps(tasks, indent=2, sort_keys=True)}\n```\n"


def _write_stuck_math_audit_report_file(
    *,
    report_path: Path,
    audit_plan: Mapping[str, Any],
) -> None:
    report = str(audit_plan.get("report", "") or "").rstrip()
    probe_paths = audit_plan.get("probe_paths")
    if not isinstance(probe_paths, list):
        probe_paths = []
    reviewer_notes = _existing_reviewer_notes(report_path)
    lines = [
        "# StuckMathAudit Report",
        "",
        report or "(no audit report text)",
        "",
        "# Audit Tasks",
        "",
        _audit_plan_tasks_markdown(audit_plan).rstrip(),
        "",
        "# Probe Paths",
        "",
    ]
    if probe_paths:
        lines.extend(f"- {path}" for path in probe_paths)
    else:
        lines.append("(none)")
    lines.extend(["", "# Reviewer Notes", ""])
    if reviewer_notes:
        lines.append(reviewer_notes.rstrip())
        lines.append("")
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


def _truncate_report_for_prompt(
    *,
    report: str,
    report_path: Path,
    max_lines: int,
) -> tuple[str, bool, int, int]:
    lines = report.splitlines()
    line_count = len(lines)
    if line_count <= max_lines:
        return report, False, line_count, 0
    omitted = line_count - max_lines
    preview = "\n".join(lines[:max_lines]).rstrip()
    preview = (
        f"{preview}\n\n"
        f"[TRUNCATED FOR PROMPT: {omitted} more line(s) omitted. "
        f"Read the full report at {report_path}.]"
    )
    return preview, True, line_count, omitted


def _stuck_math_audit_plan_prompt_view(
    *,
    repo_path: Path,
    request: Mapping[str, Any],
    audit_plan: Any,
) -> tuple[Any, str, str]:
    if not isinstance(audit_plan, Mapping):
        return audit_plan, "(no StuckMathAudit report file)", "0"
    report_path = _stuck_math_audit_report_path(
        repo_path=repo_path,
        request=request,
        audit_plan=audit_plan,
    )
    _write_stuck_math_audit_report_file(
        report_path=report_path,
        audit_plan=audit_plan,
    )
    max_lines = _stuck_math_audit_report_prompt_max_lines()
    report = str(audit_plan.get("report", "") or "")
    report_preview, truncated, line_count, omitted = _truncate_report_for_prompt(
        report=report,
        report_path=report_path,
        max_lines=max_lines,
    )
    prompt_view = dict(audit_plan)
    prompt_view["report"] = report_preview
    prompt_view["report_path"] = str(report_path)
    prompt_view["report_prompt_line_limit"] = max_lines
    prompt_view["report_line_count"] = line_count
    prompt_view["report_truncated_for_prompt"] = truncated
    prompt_view["report_omitted_lines"] = omitted
    prompt_view["reviewer_notes_section"] = "# Reviewer Notes"
    return prompt_view, str(report_path), str(max_lines)


PAPER_FOCUS_FRAGMENT_ID = "worker/common/19_paper_focus_fragments.md"


def _paper_focus_fragments_block(
    *,
    repo_path: Path,
    raw_output_path: Path,
    worker_contract: Mapping[str, Any],
) -> str:
    """Build the markdown block rendered into the worker prompt.

    Returns the empty string when the kernel did not request the
    paper-focus fragment (the contract's `prompt_fragments` list is
    authoritative about whether paper grounding is active). When the
    fragment is requested but extraction fails (bad config, range
    out of bounds, etc.), raises `ValueError` — failing closed is
    preferable to handing the worker `paper_focus_ranges` without
    the promised source text.
    """
    fragments = contract_prompt_fragments(worker_contract)
    if PAPER_FOCUS_FRAGMENT_ID not in fragments:
        return ""
    ranges = _paper_focus_ranges_from_worker_contract(worker_contract)
    if not ranges:
        return ""

    full_text = _format_paper_focus_full_text(repo_path, ranges)
    sidecar_path = _paper_focus_sidecar_path(raw_output_path)
    sidecar_path.parent.mkdir(parents=True, exist_ok=True)
    sidecar_path.write_text(full_text, encoding="utf-8")

    inline_text, truncated = _truncate_paper_focus_inline(full_text)
    if truncated:
        tail = (
            f"\n[TRUNCATED: the full cited paper text is on disk at "
            f"{sidecar_path}. Continue there if the omitted part matters.]"
        )
    else:
        tail = "\n[Complete cited text is inline above.]"

    return (
        "## Reviewer-selected paper fragments\n\n"
        "The reviewer cited these source-paper ranges as relevant to this "
        "task. The inline text below is part of your task context and should "
        "guide the edit alongside (not in place of) the rest of the paper.\n\n"
        f"{inline_text.rstrip()}\n"
        f"{tail}\n\n"
        f"Full cited text sidecar: `{sidecar_path}`"
    )


def _latest_worker_json_path(runtime_root: Path | None) -> str:
    if runtime_root is None:
        return "(runtime root unavailable to this prompt builder)"
    return str(runtime_root.resolve() / "bridge" / "latest_worker.json")


def _read_last_invalid_request_id(metadata_path: Path) -> int | None:
    try:
        data = json.loads(metadata_path.read_text(encoding="utf-8"))
    except Exception:
        return None
    if not isinstance(data, Mapping):
        return None
    try:
        request_id = int(data.get("request_id", 0) or 0)
    except Exception:
        return None
    return request_id if request_id > 0 else None


def _worker_checker_trace_path(repo_path: Path, request_id: int) -> Path:
    return repo_path.resolve() / ".trellis" / "checker" / f"worker_request_{request_id}.json"


def _has_checker_mismatch_rejection_reason(reasons: Any) -> bool:
    if isinstance(reasons, Iterable) and not isinstance(reasons, (str, bytes, bytearray, Mapping)):
        items = reasons
    else:
        items = [reasons]
    return any(
        str(item).strip().startswith(CHECKER_MISMATCH_REJECTION_PREFIX)
        for item in items
        if item
    )


def _deterministic_rejection_artifacts_text(
    *,
    repo_path: Path,
    runtime_root: Path | None,
    deterministic_rejection_reasons: Any,
    last_invalid_metadata_file_path: Path | None = None,
) -> str:
    if not _has_checker_mismatch_rejection_reason(deterministic_rejection_reasons):
        return ""
    metadata_path = (
        last_invalid_metadata_file_path
        if last_invalid_metadata_file_path is not None
        else last_invalid_metadata_path(repo_path)
    )
    request_id = _read_last_invalid_request_id(metadata_path)
    if request_id is not None:
        checker_trace = str(_worker_checker_trace_path(repo_path, request_id))
    else:
        checker_trace = (
            f"{repo_path.resolve()}/.trellis/checker/worker_request_<request_id>.json "
            f"(read request_id from {metadata_path})"
        )
    artifact_lines = "\n".join(
        [
            f"- Supervisor-side normalized worker result: {_latest_worker_json_path(runtime_root)}",
            f"- Last invalid worker metadata: {metadata_path}",
            f"- Worker-side in-burst checker trace: {checker_trace}",
        ]
    )
    return (
        "If one of these reasons is an authoritative checker mismatch and the bounded "
        "summary is not enough to diagnose it, inspect the raw checker artifacts:\n\n"
        f"{artifact_lines}"
    )


def request_contract_block(request: Mapping[str, Any], field: str) -> Dict[str, Any]:
    raw = request.get(field)
    if not isinstance(raw, Mapping):
        raise ValueError(f"request is missing {field}")
    return dict(raw)


def request_project_invariants(request: Mapping[str, Any]) -> Dict[str, Any]:
    raw = request.get("project_invariants")
    if not isinstance(raw, Mapping):
        raise ValueError("request is missing project_invariants")
    return dict(raw)

def _mapping_block(raw: Any, *, owner: str) -> Dict[str, Any]:
    if not isinstance(raw, Mapping):
        raise ValueError(f"{owner} must be an object")
    return dict(raw)


def request_worker_acceptance(request: Mapping[str, Any]) -> Dict[str, Any]:
    return _mapping_block(request.get("worker_acceptance"), owner="worker request.worker_acceptance")


def worker_gate_acceptance(worker_gate: Mapping[str, Any]) -> Dict[str, Any]:
    return _mapping_block(worker_gate.get("worker_acceptance"), owner="worker gate.worker_acceptance")


def contract_request_summary(contract: Mapping[str, Any]) -> Dict[str, Any]:
    raw = contract.get("request_summary")
    if not isinstance(raw, Mapping):
        raise ValueError("kernel-authored contract is missing request_summary")
    return dict(raw)


def contract_prompt_fragments(contract: Mapping[str, Any]) -> List[str]:
    raw = contract.get("prompt_fragments")
    if not isinstance(raw, list) or not raw:
        raise ValueError("kernel-authored contract is missing prompt_fragments")
    fragments: List[str] = []
    for idx, value in enumerate(raw):
        if not isinstance(value, str) or not value.strip():
            raise ValueError(f"kernel-authored contract prompt_fragments[{idx}] must be a non-empty string")
        fragments.append(value)
    return fragments


def contract_artifact_prompt_view(contract: Mapping[str, Any]) -> Dict[str, Any]:
    raw = contract.get("artifact_prompt_view")
    if not isinstance(raw, Mapping):
        raise ValueError("kernel-authored contract is missing artifact_prompt_view")
    return dict(raw)


def contract_previous_own_findings_by_lane(
    contract: Mapping[str, Any],
) -> Dict[str, Any]:
    raw = contract.get("previous_own_findings_by_lane")
    if raw is None:
        return {}
    if not isinstance(raw, Mapping):
        raise ValueError(
            "kernel-authored contract previous_own_findings_by_lane must be an object"
        )
    return dict(raw)


def contract_previous_own_findings_per_node(
    contract: Mapping[str, Any],
) -> Dict[str, Any]:
    """Read the sound-contract per-node previous-findings map.

    Audit Finding 3 reshaped sound reviewer evidence to be keyed by NodeId
    first and then by LaneId, so the sound contract emits the rename
    `previous_own_findings` (instead of `previous_own_findings_by_lane`).
    """
    raw = contract.get("previous_own_findings")
    if raw is None:
        return {}
    if not isinstance(raw, Mapping):
        raise ValueError(
            "kernel-authored contract previous_own_findings must be an object"
        )
    return dict(raw)


def flatten_sound_previous_findings_for_lane(
    previous_own_findings_per_node: Mapping[str, Any],
    *,
    lane_id: str,
) -> List[Dict[str, Any]]:
    """Flatten per-node-per-lane sound evidence to a per-lane list.

    Returns `[{"node": <NodeId>, "evidence": <SoundReviewerLaneEvidence>},
    ...]` containing every node whose evidence map has the requested
    `lane_id`. Preserves node-key order (BTreeMap-sorted from kernel).
    """
    flattened: List[Dict[str, Any]] = []
    for node_id, by_lane in previous_own_findings_per_node.items():
        if not isinstance(by_lane, Mapping):
            continue
        evidence = by_lane.get(lane_id)
        if evidence is None:
            continue
        flattened.append({"node": node_id, "evidence": evidence})
    return flattened


def contract_required_mapping(contract: Mapping[str, Any], field: str) -> Dict[str, Any]:
    raw = contract.get(field)
    if not isinstance(raw, Mapping):
        raise ValueError(f"kernel-authored contract is missing {field}")
    return dict(raw)


def paper_target_covering_nodes(
    paper_contract: Mapping[str, Any],
) -> Dict[str, List[str]]:
    """Typed accessor for the kernel-authored `target_covering_nodes` map.

    The kernel emits this field in all paper-contract scenarios
    (target_package / substantiveness / deviation_authorization); the bridge
    used to derive it from request fallbacks when the field was absent, but
    that legacy compat shim was removed once the kernel guarantee landed
    (2026-06-04). Missing field is a bug, not a compatibility scenario.
    """
    raw = paper_contract.get("target_covering_nodes")
    if not isinstance(raw, Mapping):
        raise ValueError(
            "kernel-authored paper contract missing required "
            "'target_covering_nodes' field; rebuild trellis_runtime_cli"
        )
    covering_nodes: Dict[str, List[str]] = {}
    for target, nodes in raw.items():
        if not isinstance(target, str):
            raise ValueError("kernel-authored target_covering_nodes keys must be strings")
        if not isinstance(nodes, list):
            raise ValueError(
                f"kernel-authored target_covering_nodes[{target!r}] must be a list"
            )
        covering_nodes[target] = sorted(
            node for node in nodes if isinstance(node, str) and node.strip()
        )
    return covering_nodes


_PROMPT_LIST_TRUNCATION_THRESHOLD = 15


def _truncate_long_arrays(value: Any, threshold: int = _PROMPT_LIST_TRUNCATION_THRESHOLD) -> Any:
    """Walk a JSON-shaped value; replace lists longer than `threshold`
    with their first `threshold` items plus a string marker pointing
    the agent at the on-disk structured request file for the rest.
    Dicts with more than `threshold` keys are similarly truncated. The
    output remains valid JSON. Used by `_json_fence` so every JSON
    block surfaced in agent prompts gets the same treatment."""
    if isinstance(value, list):
        truncated = [_truncate_long_arrays(item, threshold) for item in value]
        if len(truncated) > threshold:
            head = truncated[:threshold]
            head.append(
                f"... [truncated; {len(truncated) - threshold} more items in full request file]"
            )
            return head
        return truncated
    if isinstance(value, dict):
        items = list(value.items())
        if len(items) > threshold:
            kept: Dict[str, Any] = {
                str(k): _truncate_long_arrays(v, threshold) for k, v in items[:threshold]
            }
            kept["__truncated__"] = (
                f"... [truncated; {len(items) - threshold} more keys in full request file]"
            )
            return kept
        return {k: _truncate_long_arrays(v, threshold) for k, v in value.items()}
    return value


def _json_fence(payload: Any) -> str:
    truncated = _truncate_long_arrays(payload)
    return f"```json\n{json.dumps(truncated, indent=2, sort_keys=True)}\n```"


def _structured_request_path_for(raw_output_path: Path) -> Path:
    """Return the path to the kernel-emitted full-structure request
    file that sits beside the agent's `.raw.json` output. The bridge
    writes this file before dispatch (one of `request_path` /
    `context_json_path` in bridge.py); naming follows the canonical
    `<base>.raw.json` -> `<base>.request.json` rewrite."""
    name = raw_output_path.name
    if name.endswith(".raw.json"):
        name = name[: -len(".raw.json")] + ".request.json"
    elif name.endswith(".json"):
        name = name[: -len(".json")] + ".request.json"
    else:
        name = name + ".request.json"
    return raw_output_path.parent / name


def _check_script_path(repo_path: Path) -> Path:
    return repo_path / ".trellis" / "scripts" / "check.py"


def _shell_command(*parts: Any) -> str:
    return " ".join(shlex.quote(str(part)) for part in parts)


def _artifact_command(
    *,
    contract: Mapping[str, Any],
    context: Mapping[str, str],
    field: str,
) -> str:
    prompt_view = contract_artifact_prompt_view(contract)
    raw_template = prompt_view.get(field)
    if not isinstance(raw_template, list) or not raw_template:
        raise ValueError(f"kernel-authored artifact_prompt_view is missing {field}")
    rendered_parts: List[str] = []
    for idx, part in enumerate(raw_template):
        if not isinstance(part, str) or not part:
            raise ValueError(f"kernel-authored {field}[{idx}] must be a non-empty string")
        def _replace(match: re.Match[str]) -> str:
            key = match.group(1)
            if key not in context:
                raise ValueError(f"{field}[{idx}] references missing context key {key}")
            return context[key]

        rendered_parts.append(_PROMPT_PLACEHOLDER_RE.sub(_replace, part))
    return _shell_command(*rendered_parts)


def _validated_artifact_instructions(
    *,
    repo_path: Path,
    raw_output_path: Path,
    done_path: Path,
    contract: Mapping[str, Any],
    command_context: Mapping[str, str],
) -> Dict[str, Any]:
    command_template_context = {
        "check_script_path": str(_check_script_path(repo_path)),
        "raw_output_path": str(raw_output_path),
        "done_path": str(done_path),
        **command_context,
    }
    json_check_command = _artifact_command(
        contract=contract,
        context=command_template_context,
        field="json_check_command_template",
    )
    prompt_view = contract_artifact_prompt_view(contract)
    acceptance_check_command = ""
    raw_acceptance = prompt_view.get("acceptance_check_command_template")
    if isinstance(raw_acceptance, list) and raw_acceptance:
        acceptance_check_command = _artifact_command(
            contract=contract,
            context=command_template_context,
            field="acceptance_check_command_template",
        )
    acceptance_check_guidance = (
        "An additional acceptance checker is provided below. You should run it too and use its output if you can make honest progress."
        if acceptance_check_command
        else "No additional acceptance checker is provided for this role. The raw JSON validator above is the agent-side check you must satisfy before writing the done marker."
    )
    acceptance_check_block = (
        "You should test ahead of time whether your work will satisfy deterministic validity checks by running the command\n\n"
        f"`{acceptance_check_command}`\n\n"
        "If it does not pass, you are supposed to fix it. "
        "Expect this to take many minutes; a long quiet wait is normal, not a stall."
        if acceptance_check_command
        else "No additional acceptance checker is provided for this role."
    )
    return {
        "prompt_view": prompt_view,
        "raw_output_path": str(raw_output_path),
        "done_path": str(done_path),
        "json_check_command": json_check_command,
        "acceptance_check_command": acceptance_check_command,
        "acceptance_check_guidance": acceptance_check_guidance,
        "acceptance_check_block": acceptance_check_block,
        "required_json_shape": _contract_prompt_schema_example(contract),
    }


def _prompt_facing_artifact_delivery(artifact_delivery: Mapping[str, Any]) -> Dict[str, Any]:
    payload: Dict[str, Any] = {
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "required_json_shape": artifact_delivery["required_json_shape"],
    }
    acceptance_check_command = str(artifact_delivery.get("acceptance_check_command", "") or "").strip()
    if acceptance_check_command:
        payload["acceptance_check_command"] = acceptance_check_command
    return payload


def _drop_null_keys(payload: Any) -> Any:
    """Recursively drop dict keys whose value is JSON null.

    Conservative trim helper: when the kernel emits a field as `null` (e.g.
    `held_target: None` for a TheoremStating worker, or
    `difficulty_update_contract: null` per audit A4), the rendered prompt
    JSON would otherwise show the literal `"key": null` line. This adds no
    information beyond "the field is not set" -- which the field's absence
    already conveys -- so we drop it. Empty mappings/lists are NOT dropped
    (they may carry semantic meaning that nullification does not).
    """
    if isinstance(payload, Mapping):
        result: Dict[str, Any] = {}
        for key, value in payload.items():
            if value is None:
                continue
            result[key] = _drop_null_keys(value)
        return result
    if isinstance(payload, list):
        return [_drop_null_keys(item) for item in payload]
    return payload


def _verifier_evidence_is_empty(ev: Mapping[str, Any]) -> bool:
    """All four lanes (paper / substantiveness / corr / sound) are empty.

    Used by `build_review_prompt` (Trim 7) to short-circuit rendering the
    full `verifier_evidence` JSON block in the post-Worker reviewer state
    where no verifier has yet observed the current worker output.
    """
    for sub in ("paper", "substantiveness", "corr", "sound"):
        value = ev.get(sub)
        if isinstance(value, Mapping) and value:
            return False
        if isinstance(value, list) and value:
            return False
    return True


def _prompt_facing_worker_contract(contract: Mapping[str, Any]) -> Dict[str, Any]:
    prompt_contract = dict(contract)
    for field in (
        "prompt_fragments",
        "request_summary",
        "artifact_prompt_view",
        "reviewer_comments",
        "reviewer_lean_product",
    ):
        prompt_contract.pop(field, None)
    # Trim 1: when worker_context.authorized_nodes and
    # scope_contract.authorized_existing_nodes match, the second copy is
    # redundant. Kernel keeps both for downstream consumers; bridge
    # dedups for prompt rendering only when the lists are equal. The
    # canonical copy lives in worker_context (tied to routing-hint and
    # validation-kind fields).
    request_summary = contract.get("request_summary")
    worker_context = (
        request_summary.get("worker_context")
        if isinstance(request_summary, Mapping)
        else None
    )
    scope_contract_value = prompt_contract.get("scope_contract")
    worker_context_authorized = (
        worker_context.get("authorized_nodes")
        if isinstance(worker_context, Mapping)
        else None
    )
    scope_authorized = (
        scope_contract_value.get("authorized_existing_nodes")
        if isinstance(scope_contract_value, Mapping)
        else None
    )
    if (
        isinstance(worker_context_authorized, list)
        and isinstance(scope_authorized, list)
        and worker_context_authorized == scope_authorized
        and isinstance(scope_contract_value, Mapping)
    ):
        new_scope = dict(scope_contract_value)
        new_scope.pop("authorized_existing_nodes", None)
        new_scope["authorized_existing_nodes_ref"] = (
            "request_summary.worker_context.authorized_nodes"
        )
        prompt_contract["scope_contract"] = new_scope
    # Trim 11: drop literal `null` values surfaced by Option<NodeId>
    # fields (active_node, held_target).
    return _drop_null_keys(prompt_contract)


_ACTIVE_NODE_ONLY_VALIDATION_KINDS = ("proof_easy", "proof_local")


def _prompt_facing_worker_acceptance(
    worker_acceptance: Mapping[str, Any],
) -> Dict[str, Any]:
    """Bridge-side scrub of `worker_acceptance` for prompt rendering.

    Trim 9: ProofEasy and ProofLocal validation kinds use
    `existing_node_scope_mode = active_node_only`; their
    `authorized_nodes` field is structurally always an empty list (see
    `current_worker_authorized_nodes` in kernel/src/model.rs). Drop the
    empty-list line for these kinds so the rendered prompt does not
    suggest "no nodes are authorized" — the field is conceptually absent
    in active-node-only mode rather than an empty whitelist.
    """
    payload = dict(worker_acceptance)
    validation_kind = str(payload.get("validation_kind") or "").strip().lower()
    if validation_kind in _ACTIVE_NODE_ONLY_VALIDATION_KINDS:
        nodes = payload.get("authorized_nodes")
        if isinstance(nodes, list) and not nodes:
            payload.pop("authorized_nodes", None)
            payload["authorized_nodes_note"] = (
                "active_node_only: existing-node scope is the active_node only; "
                "no whitelist applies"
            )
    # Trim 11: drop any null Option<> fields that may surface here
    # (defensive — current schema does not embed nullable nodes here).
    return _drop_null_keys(payload)


def _prompt_facing_review_contract(contract: Mapping[str, Any]) -> Dict[str, Any]:
    prompt_contract = dict(contract)
    for field in (
        "prompt_fragments",
        "request_summary",
        "artifact_prompt_view",
        "verifier_evidence",
        "audit_plan",
        # Phase 2 of bridge-to-kernel migration: kernel-rendered Markdown
        # for the blocker-choices block. The bridge splices the rendered
        # text in via the `blocker_choices_summary` placeholder; the raw
        # `{md, sidecar_payload}` struct is not shown to the reviewer.
        "blocker_choices_block",
    ):
        prompt_contract.pop(field, None)
    raw_blocker_partition = prompt_contract.get("blocker_partition")
    if isinstance(raw_blocker_partition, Mapping):
        blocker_partition = dict(raw_blocker_partition)
        blocker_partition.pop("choices", None)
        prompt_contract["blocker_partition"] = blocker_partition
    raw_blocker_actions = prompt_contract.get("blocker_actions")
    if isinstance(raw_blocker_actions, Mapping):
        blocker_actions = dict(raw_blocker_actions)
        blocker_actions.pop("choices", None)
        prompt_contract["blocker_actions"] = blocker_actions
    # Trim 5: kernel A4 already emits difficulty_update_contract as
    # `null` during TheoremStating; bridge null-drop suppresses the
    # `"difficulty_update_contract": null` line from the rendered JSON.
    # Trim 11: also handles any other null-valued fields.
    return _drop_null_keys(prompt_contract)


def _blocker_sidecar_path(raw_output_path: Path) -> Path:
    """Where the full structured blocker list is written when count overflows.

    Mirrors `_paper_focus_sidecar_path` — a sibling of `raw_output_path` so
    it lives in the same bridge staging directory the worker/reviewer is
    already permitted to read.
    """
    return raw_output_path.with_name(f"{raw_output_path.stem}.blockers.json")


_WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER = "{sidecar_path}"
_REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER = "{context_json_path}"


def _resolve_review_blocker_choices_block(
    *,
    review_contract: Mapping[str, Any],
    raw_output_path: Path,
    context_json_path: Path,
) -> str:
    """Apply the kernel-emitted `blocker_choices_block` to the reviewer prompt.

    Direct parallel to `_resolve_worker_blocker_status_block` (Phase 2 of the
    2026-06-04 bridge-to-kernel migration, fallback removed in batch 2 on
    2026-06-04). The kernel renders the Markdown body and the structured
    sidecar payload (when the live blocker count overflows the inline limit).
    The bridge's only remaining responsibilities are:

      * write the sidecar JSON to ``<raw_output_path>.blockers.json`` when
        ``sidecar_payload`` is present,
      * substitute ``{sidecar_path}`` and ``{context_json_path}`` placeholders
        in ``md`` with concrete on-disk paths.

    The kernel binary is always rebuilt at or before the bridge process that
    invokes it (see `feedback_kernel_cli_subprocess_reloads`), so the
    contract must always carry `blocker_choices_block`. Missing field is a
    bug, not a compatibility scenario.
    """
    block = review_contract.get("blocker_choices_block")
    if not isinstance(block, Mapping):
        raise KeyError(
            "review contract missing required kernel-emitted "
            "'blocker_choices_block' field; rebuild trellis_runtime_cli"
        )
    md = str(block.get("md") or "")
    sidecar_payload = block.get("sidecar_payload")
    if isinstance(sidecar_payload, Mapping):
        sidecar = _write_blocker_sidecar_payload(raw_output_path, sidecar_payload)
        md = md.replace(_WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER, str(sidecar))
    md = md.replace(
        _REVIEW_BLOCKER_CONTEXT_JSON_PATH_PLACEHOLDER, str(context_json_path)
    )
    return md


def _resolve_worker_blocker_status_block(
    *,
    worker_contract: Mapping[str, Any],
    raw_output_path: Path,
) -> str:
    """Apply the kernel-emitted `blocker_status` block to the worker prompt.

    The kernel renders the Markdown body and the structured sidecar payload
    (when the live blocker count overflows the inline limit). The bridge's
    only remaining responsibilities are:

      * write the sidecar JSON to ``<raw_output_path>.blockers.json`` when
        ``sidecar_payload`` is present,
      * substitute the ``{sidecar_path}`` placeholder in ``md`` with the
        concrete on-disk path.

    For backwards compatibility with older kernels that have not yet been
    rebuilt with the migration, the function falls back to "No live
    blockers." when `blocker_status` is absent from the worker contract.
    """
    block = worker_contract.get("blocker_status")
    if not isinstance(block, Mapping):
        return "No live blockers."
    md = str(block.get("md") or "")
    sidecar_payload = block.get("sidecar_payload")
    if isinstance(sidecar_payload, Mapping):
        sidecar = _write_blocker_sidecar_payload(raw_output_path, sidecar_payload)
        md = md.replace(_WORKER_BLOCKER_SIDECAR_PATH_PLACEHOLDER, str(sidecar))
    return md


def _write_blocker_sidecar_payload(
    raw_output_path: Path, payload: Mapping[str, Any]
) -> Path:
    """Write the kernel-rendered sidecar JSON next to the worker's raw output.

    Mirrors `_blocker_sidecar_path`'s naming convention (used by the
    reviewer-side overflow path too) so on-disk shapes stay consistent.
    """
    sidecar = _blocker_sidecar_path(raw_output_path)
    sidecar.parent.mkdir(parents=True, exist_ok=True)
    sidecar.write_text(
        json.dumps(dict(payload), indent=2, sort_keys=True),
        encoding="utf-8",
    )
    return sidecar


# A9/A10: scrub kernel housekeeping and prompt-render duplicates from verifier
# contract JSON. The audit calls out:
#   - drop `prompt_fragments` (kernel render-machinery, not actionable)
#   - drop `artifact_prompt_view` (kernel render-machinery)
#   - drop `issue_reporting_policy` and `fixed_item_reporting_policy`
#     (kernel-only flags; the rubric/fragment text already explains them)
#   - drop `previous_own_findings_by_lane` (duplicate of the dedicated
#     `previous_own_findings_json` block at the top of the prompt) and the
#     bridge-injected `previous_own_findings_for_lane` flatten
#   - drop `request_summary` (rendered separately as `request_summary_json`)
#   - drop the per-lane scope duplicates: `node_issue_scope` (corr),
#     `target_issue_scope` (paper), `target_nodes` (sound),
#     `target_covering_nodes` (paper) — each is a copy of fields in
#     `request_summary` or rendered separately.
_VERIFIER_HOUSEKEEPING_FIELDS = (
    "prompt_fragments",
    "request_summary",
    "artifact_prompt_view",
    "issue_reporting_policy",
    "fixed_item_reporting_policy",
    "previous_own_findings_by_lane",
    "previous_own_findings",
    "previous_own_findings_for_lane",
)


def _contract_prompt_facing_view(contract: Mapping[str, Any]) -> Dict[str, Any]:
    """Return the kernel-authored `prompt_facing_view` sub-field for inline
    prompt rendering. Kernel `paper_contract_payload`,
    `audit_contract_payload`, and `stuck_math_audit_contract_payload`
    populate this verbatim; the bridge no longer trims these three
    contracts. Falls back to the full contract for legacy/dormant
    payloads that have no `prompt_facing_view` (e.g. RequestKind mismatch
    short-circuits returning the `no_*_contract_payload` skeletons that
    have no view sub-field)."""
    view = contract.get("prompt_facing_view")
    if isinstance(view, Mapping):
        return dict(view)
    return dict(contract)


def _prompt_facing_corr_contract(contract: Mapping[str, Any]) -> Dict[str, Any]:
    prompt_contract = dict(contract)
    for field in _VERIFIER_HOUSEKEEPING_FIELDS:
        prompt_contract.pop(field, None)
    # node_issue_scope duplicates request_summary.nodes
    prompt_contract.pop("node_issue_scope", None)
    # Trim 11: drop any null-valued Option<> fields surfaced by the
    # kernel (active_node, held_target).
    return _drop_null_keys(prompt_contract)


def substantiveness_basis_inputs(paper_contract: Mapping[str, Any]) -> Dict[str, Any]:
    raw = paper_contract.get("node_paper_basis_inputs")
    if isinstance(raw, Mapping):
        return dict(raw)
    return {}


def _prompt_facing_sound_contract(contract: Mapping[str, Any]) -> Dict[str, Any]:
    prompt_contract = dict(contract)
    for field in _VERIFIER_HOUSEKEEPING_FIELDS:
        prompt_contract.pop(field, None)
    # target_nodes duplicates request_summary content
    prompt_contract.pop("target_nodes", None)
    # `reverification_context` is rendered separately by the dedicated
    # `verifier/common/15a_reverification_context.md` fragment via the
    # `reverification_context_json` placeholder; drop from the inline
    # contract JSON to avoid duplication.
    prompt_contract.pop("reverification_context", None)
    # Trim 11: drop any null-valued Option<> fields.
    return _drop_null_keys(prompt_contract)


def _contract_prompt_schema_example(contract: Mapping[str, Any]) -> Any:
    artifact_contract = contract.get("artifact_contract")
    if isinstance(artifact_contract, Mapping) and "prompt_schema_example" in artifact_contract:
        return artifact_contract["prompt_schema_example"]
    if "prompt_schema_example" in contract:
        return contract["prompt_schema_example"]
    raise ValueError("kernel-authored contract is missing prompt_schema_example")


_PROMPT_PLACEHOLDER_RE = re.compile(r"\{\{([a-zA-Z0-9_]+)\}\}")
_PROMPT_HTML_COMMENT_RE = re.compile(r"<!--.*?-->", re.DOTALL)


def _strip_prompt_fragment_comments(template: str) -> str:
    return _PROMPT_HTML_COMMENT_RE.sub("", template)


def _resolve_prompt_fragment(fragment_id: str) -> Path:
    path = (PROMPT_FRAGMENT_ROOT / fragment_id).resolve()
    root = PROMPT_FRAGMENT_ROOT.resolve()
    if root not in path.parents:
        raise ValueError(f"prompt fragment escapes fragment root: {fragment_id}")
    if path.suffix != ".md":
        raise ValueError(f"prompt fragment must be markdown: {fragment_id}")
    if not path.is_file():
        raise ValueError(f"prompt fragment not found: {fragment_id}")
    return path


def _render_prompt_fragment(fragment_id: str, context: Mapping[str, str]) -> str:
    template = _strip_prompt_fragment_comments(
        _resolve_prompt_fragment(fragment_id).read_text(encoding="utf-8")
    )

    def _replace(match: re.Match[str]) -> str:
        key = match.group(1)
        if key not in context:
            raise ValueError(f"prompt fragment {fragment_id} references missing context key {key}")
        return context[key]

    rendered = _PROMPT_PLACEHOLDER_RE.sub(_replace, template)
    return rendered.strip()


def render_prompt_sections(
    fragment_ids: Iterable[str], context: Mapping[str, str]
) -> List[Dict[str, str]]:
    sections: List[Dict[str, str]] = []
    for fragment_id in fragment_ids:
        rendered = _render_prompt_fragment(fragment_id, context)
        if not rendered:
            continue
        sections.append(
            {
                "fragment_id": fragment_id,
                "source_path": str(_resolve_prompt_fragment(fragment_id)),
                "text": rendered,
            }
        )
    return sections


def _render_prompt_from_fragments(fragment_ids: Iterable[str], context: Mapping[str, str]) -> str:
    sections = render_prompt_sections(fragment_ids, context)
    return "\n\n".join(section["text"] for section in sections if section.get("text"))


def _lane_scoped_contract_json(
    contract: Mapping[str, Any],
    *,
    lane_id: str,
    previous_own_findings: Mapping[str, Any],
) -> Dict[str, Any]:
    scoped = dict(contract)
    scoped["previous_own_findings_by_lane"] = (
        {lane_id: previous_own_findings[lane_id]}
        if lane_id in previous_own_findings
        else {}
    )
    return scoped


def _safe_scratch_component(value: Any, *, fallback: str) -> str:
    raw = str(value if value is not None else "").strip()
    safe = re.sub(r"[^A-Za-z0-9_.-]+", "-", raw).strip(".-")
    return safe or fallback


def _correspondence_scratch_path(
    repo_path: Path,
    *,
    request: Mapping[str, Any],
    lane_id: str,
) -> Path:
    cycle = _safe_scratch_component(request.get("cycle"), fallback="unknown-cycle")
    request_id = _safe_scratch_component(request.get("id"), fallback="unknown-request")
    lane = _safe_scratch_component(lane_id, fallback="unknown-lane")
    parent = repo_tmp_subdir(repo_path, "correspondence")
    try:
        parent.chmod(0o770)
    except OSError:
        pass
    path = parent / f"cycle-{cycle}-request-{request_id}-lane-{lane}"
    if path.exists():
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink()
    path.mkdir(parents=True, exist_ok=True)
    try:
        path.chmod(0o770)
    except OSError:
        pass
    return path


def _sound_lane_scoped_contract_json(
    contract: Mapping[str, Any],
    *,
    lane_id: str,
    previous_own_findings_per_node: Mapping[str, Any],
    flattened_lane_findings: List[Dict[str, Any]],
) -> Dict[str, Any]:
    """Lane-scope the sound contract while preserving node provenance.

    The kernel ships sound previous-findings as `Map<NodeId, Map<LaneId,
    Evidence>>`. This helper rewrites the field for the requested lane by
    keeping only entries whose inner map contains `lane_id`, and also
    surfaces a flattened `previous_own_findings_for_lane` list so the
    prompt verifier can read multi-node evidence directly.
    """
    scoped = dict(contract)
    per_node_for_lane: Dict[str, Any] = {}
    for node_id, by_lane in previous_own_findings_per_node.items():
        if not isinstance(by_lane, Mapping):
            continue
        if lane_id in by_lane:
            per_node_for_lane[node_id] = {lane_id: by_lane[lane_id]}
    scoped["previous_own_findings"] = per_node_for_lane
    scoped["previous_own_findings_for_lane"] = flattened_lane_findings
    return scoped


def build_correspondence_prompt(
    *,
    request: Dict[str, Any],
    repo_path: Path,
    lane_id: str,
    raw_output_path: Path,
    done_path: Path,
) -> str:
    corr_contract = request_contract_block(request, "corr_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(corr_contract)
    previous_own_findings = contract_previous_own_findings_by_lane(corr_contract)
    lane_scoped_contract = _lane_scoped_contract_json(
        corr_contract,
        lane_id=lane_id,
        previous_own_findings=previous_own_findings,
    )
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=corr_contract,
        command_context={},
    )
    rubric = contract_required_mapping(corr_contract, "rubric")
    correspondence_scratch_path = _correspondence_scratch_path(
        repo_path,
        request=request,
        lane_id=lane_id,
    )
    context = {
        "repo_path": str(repo_path),
        "lane_id": lane_id,
        "correspondence_scratch_path": str(correspondence_scratch_path),
        "trellis_scheme_reference_path": str(_scheme_reference_path(repo_path)),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "previous_own_findings_json": _json_fence(previous_own_findings.get(lane_id)),
        "contract_json": _json_fence(_prompt_facing_corr_contract(lane_scoped_contract)),
        "rubric_json": _json_fence(rubric),
        "artifact_delivery_json": _json_fence(_prompt_facing_artifact_delivery(artifact_delivery)),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
        "structured_request_path": str(_structured_request_path_for(raw_output_path)),
    }
    return _render_prompt_from_fragments(
        contract_prompt_fragments(corr_contract),
        context,
    )


def build_paper_faithfulness_prompt(
    *,
    request: Dict[str, Any],
    repo_path: Path,
    lane_id: str,
    raw_output_path: Path,
    done_path: Path,
) -> str:
    paper_contract = request_contract_block(request, "paper_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(paper_contract)
    target_covering_nodes = paper_target_covering_nodes(paper_contract)
    # Per-node scenario surface (kernel emits this when substantiveness_verify_nodes
    # is non-empty AND paper_verify_targets is empty). We render whichever
    # blocks are populated; fragments for the unused scenario quietly
    # render their corresponding template variables as empty strings.
    node_basis_inputs = substantiveness_basis_inputs(paper_contract)
    deviation = paper_contract.get("deviation") if isinstance(paper_contract, Mapping) else None
    if not isinstance(deviation, Mapping):
        deviation = {}
    # The kernel's request_summary.scenario field disambiguates target
    # vs per-node; bridge_prompts has historically been schema-tolerant,
    # so fall back on the presence of `node_paper_basis_inputs`.
    is_per_node_scenario = bool(node_basis_inputs) or (
        request_summary.get("scenario") == "substantiveness"
        if isinstance(request_summary, Mapping)
        else False
    )
    if is_per_node_scenario:
        previous_own_findings_raw = paper_contract.get("previous_own_findings")
        if isinstance(previous_own_findings_raw, Mapping):
            previous_own_findings = dict(previous_own_findings_raw)
        else:
            previous_own_findings = {}
    else:
        previous_own_findings = contract_previous_own_findings_by_lane(paper_contract)
    # Lane scoping for paper is now a no-op: the kernel-authored
    # `prompt_facing_view` (consumed below) already drops
    # `previous_own_findings_by_lane`, and `previous_own_findings_json`
    # is rendered separately via the dedicated placeholder using
    # `previous_own_findings.get(lane_id)`. corr/sound still use
    # `_lane_scoped_contract_json` because their prompt-facing scrubs
    # were not part of this migration.
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=paper_contract,
        command_context={},
    )
    rubric = contract_required_mapping(paper_contract, "rubric")
    if is_per_node_scenario:
        # In the per-node scenario, "previous own findings" is keyed by
        # node, not by lane. Render the whole map as a single JSON block —
        # the verifier picks out their lane's records during reading.
        previous_own_findings_block = _json_fence(previous_own_findings)
    else:
        previous_own_findings_block = _json_fence(previous_own_findings.get(lane_id))
    context = {
        "repo_path": str(repo_path),
        "lane_id": lane_id,
        "trellis_scheme_reference_path": str(_scheme_reference_path(repo_path)),
        "paper_tex_path": _paper_tex_path(repo_path),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "target_covering_nodes_json": _json_fence(target_covering_nodes),
        "node_paper_basis_inputs_json": _json_fence(node_basis_inputs),
        "deviation_json": _json_fence(deviation),
        "previous_own_findings_json": previous_own_findings_block,
        "contract_json": _json_fence(_contract_prompt_facing_view(paper_contract)),
        "rubric_json": _json_fence(rubric),
        "artifact_delivery_json": _json_fence(_prompt_facing_artifact_delivery(artifact_delivery)),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
        "structured_request_path": str(_structured_request_path_for(raw_output_path)),
    }
    return _render_prompt_from_fragments(
        contract_prompt_fragments(paper_contract),
        context,
    )


def build_worker_prompt(
    *,
    request: Dict[str, Any],
    worker_gate: Mapping[str, Any],
    repo_path: Path,
    raw_output_path: Path,
    done_path: Path,
    acceptance_context_path: Path,
    runtime_root: Path | None = None,
    verifier_evidence_path: Path | None = None,
    theorem_initial_dag_size_guidance: str = "15-50",
    scratch_workspace_path: Path | None = None,
    scratch_workspace_status_text: str = "available for worker notes and Lean experiments.",
    scratch_readme_path: Path | None = None,
    scratch_notes_path: Path | None = None,
    scratch_example_path: Path | None = None,
    last_invalid_root_path: Path | None = None,
    last_invalid_metadata_file_path: Path | None = None,
) -> str:
    worker_acceptance = worker_gate_acceptance(worker_gate)
    worker_contract = request_contract_block(request, "worker_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(worker_contract)
    reviewer_comments = str(worker_contract.get("reviewer_comments", "") or "").strip()
    # Option A audit-plan visibility unification (kernel commit 1387341):
    # when the kernel has no live `audit_plan` but does have a snapshot
    # (`previous_audit_plan_snapshot`), it inserts the advisory
    # `worker/common/34d_last_audit_plan.md` fragment which references
    # `{{previous_audit_plan_snapshot_json}}`. Source the value the same
    # way the StuckMathAudit build does so the placeholder always
    # resolves. Mirrors bridge_prompts.py:2100 (SMA).
    previous_audit_plan_snapshot = (
        worker_contract.get("previous_audit_plan_snapshot")
        if "previous_audit_plan_snapshot" in worker_contract
        else request.get("previous_audit_plan_snapshot")
    )
    paper_focus_block = _paper_focus_fragments_block(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        worker_contract=worker_contract,
    )
    # Process issue 4 (2026-05-22): pull blockers out of the inline
    # request_summary JSON and render them as a dedicated section AFTER the
    # reviewer comments. The previous order (blockers inside the early
    # request_summary JSON) buried the narrow actionable task in free-text
    # reviewer comments below; with ~50 live soundness blockers the worker
    # would read the big global list first and reason from it rather than
    # the immediate reviewer guidance + verifier evidence.
    #
    # Migration 2026-06-02: rendering moved to the kernel. The bridge now
    # reads the pre-rendered `blocker_status` payload from `worker_contract`,
    # writes the structured sidecar (when present), substitutes the
    # `{sidecar_path}` placeholder, and splices the resulting Markdown into
    # the worker prompt context.
    blockers_for_block: List[Any] = list(request_summary.get("blockers") or [])
    request_summary_inline = dict(request_summary)
    if blockers_for_block:
        # Replace the inline list with a count + pointer note so readers know
        # the data was moved, not lost. The blocker_status_block context var
        # carries the full surface (actionable subset, sidecar pointer when
        # overflow).
        request_summary_inline["blockers"] = (
            f"<{len(blockers_for_block)} live blocker(s); see the dedicated "
            "blocker section below for the actionable subset and (if "
            "applicable) the sidecar path>"
        )
    blocker_status_block = _resolve_worker_blocker_status_block(
        worker_contract=worker_contract,
        raw_output_path=raw_output_path,
    )
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=worker_contract,
        command_context={
            "repo_path": str(repo_path),
            "acceptance_context_path": str(acceptance_context_path),
        },
    )
    context = {
        "repo_path": str(repo_path),
        "trellis_scheme_reference_path": str(_scheme_reference_path(repo_path)),
        "loogle_helper_path": str(_loogle_helper_path(repo_path)),
        "paper_tex_path": _paper_tex_path(repo_path),
        "theorem_initial_dag_size_guidance": theorem_initial_dag_size_guidance,
        "effective_fresh_context_mode": "fresh" if bool(request.get("fresh_context", False)) else "resume",
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary_inline),
        "blocker_status_block": blocker_status_block,
        "reviewer_lean_product_json": _json_fence(
            worker_contract.get("reviewer_lean_product")
        ),
        "audit_plan_json": _json_fence(worker_contract.get("audit_plan")),
        "previous_audit_plan_snapshot_json": _json_fence(previous_audit_plan_snapshot),
        "deterministic_worker_rejection_reasons_json": _json_fence(
            request_summary.get("deterministic_worker_rejection_reasons", [])
        ),
        "deterministic_worker_rejection_artifacts_text": _deterministic_rejection_artifacts_text(
            repo_path=repo_path,
            runtime_root=runtime_root,
            deterministic_rejection_reasons=request_summary.get(
                "deterministic_worker_rejection_reasons", []
            ),
            last_invalid_metadata_file_path=last_invalid_metadata_file_path,
        ),
        "review_verifier_evidence_path": (
            str(verifier_evidence_path)
            if verifier_evidence_path is not None
            else "No request-local verifier evidence sidecar was provided."
        ),
        "scratch_workspace_path": str(
            scratch_workspace_path if scratch_workspace_path is not None else worker_scratch_dir(repo_path)
        ),
        "scratch_readme_path": str(
            scratch_readme_path if scratch_readme_path is not None else worker_scratch_readme_path(repo_path)
        ),
        "scratch_notes_path": str(
            scratch_notes_path if scratch_notes_path is not None else worker_scratch_notes_path(repo_path)
        ),
        "scratch_example_path": str(
            scratch_example_path if scratch_example_path is not None else worker_scratch_example_path(repo_path)
        ),
        "scratch_workspace_status_text": scratch_workspace_status_text,
        "last_invalid_path": str(
            last_invalid_root_path if last_invalid_root_path is not None else last_invalid_dir(repo_path)
        ),
        "last_invalid_metadata_path": str(
            last_invalid_metadata_file_path
            if last_invalid_metadata_file_path is not None
            else last_invalid_metadata_path(repo_path)
        ),
        "reviewer_comments_text": reviewer_comments or "No reviewer comments.",
        "paper_focus_fragments_block": paper_focus_block,
        "contract_json": _json_fence(_prompt_facing_worker_contract(worker_contract)),
        "acceptance_contract_json": _json_fence(
            _prompt_facing_worker_acceptance(worker_acceptance)
        ),
        "artifact_delivery_json": _json_fence(_prompt_facing_artifact_delivery(artifact_delivery)),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
        "structured_request_path": str(_structured_request_path_for(raw_output_path)),
    }
    _worker_fragments = list(contract_prompt_fragments(worker_contract))
    # Loogle is opt-in per project: when no local server is configured
    # (`loogle.enabled` false in trellis.config.json), drop the Loogle helper
    # fragment so the worker isn't told to query a server that isn't running.
    if not _loogle_enabled(repo_path):
        _worker_fragments = [
            f for f in _worker_fragments if not f.endswith("/15_loogle.md")
        ]
    # Process issue 4 (2026-05-22): inject the `35a_blocker_status.md`
    # fragment AFTER `35_reviewer_comments.md` (and, when present, after
    # `35b_reviewer_comments_with_history.md`). Bridge-level splice so the
    # kernel emit list doesn't have to grow. The fragment reads
    # `blocker_status_block` from the context dict.
    _blocker_status_frag = "worker/common/35a_blocker_status.md"
    if _blocker_status_frag not in _worker_fragments:
        _insert_at = len(_worker_fragments)
        for _i in range(len(_worker_fragments) - 1, -1, -1):
            name = _worker_fragments[_i]
            if (
                name.endswith("/35_reviewer_comments.md")
                or name.endswith("/35b_reviewer_comments_with_history.md")
            ):
                _insert_at = _i + 1
                break
        _worker_fragments.insert(_insert_at, _blocker_status_frag)
    return _render_prompt_from_fragments(
        _worker_fragments,
        context,
    )


def build_soundness_prompt(
    *,
    request: Dict[str, Any],
    repo_path: Path,
    lane_id: str,
    node_name: str,
    raw_output_path: Path,
    done_path: Path,
) -> str:
    sound_contract = request_contract_block(request, "sound_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(sound_contract)
    previous_own_findings_per_node = contract_previous_own_findings_per_node(
        sound_contract
    )
    flattened_lane_findings = flatten_sound_previous_findings_for_lane(
        previous_own_findings_per_node, lane_id=lane_id
    )
    lane_scoped_contract = _sound_lane_scoped_contract_json(
        sound_contract,
        lane_id=lane_id,
        previous_own_findings_per_node=previous_own_findings_per_node,
        flattened_lane_findings=flattened_lane_findings,
    )
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=sound_contract,
        command_context={"node_name": node_name},
    )
    rubric = contract_required_mapping(sound_contract, "rubric")
    # Re-verification context (Sound only): the kernel sets
    # `reverification_context` to a non-null object when the target was
    # previously approved and is being re-verified because of a
    # fingerprint change. When non-null, the kernel ALSO includes the
    # `verifier/common/15a_reverification_context.md` fragment in the
    # contract's `prompt_fragments` list — which references the
    # `reverification_context_json` placeholder we add here.
    reverification_context = sound_contract.get("reverification_context")
    context = {
        "repo_path": str(repo_path),
        "lane_id": lane_id,
        "trellis_scheme_reference_path": str(_scheme_reference_path(repo_path)),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "previous_own_findings_json": _json_fence(
            flattened_lane_findings if flattened_lane_findings else None
        ),
        "reverification_context_json": _json_fence(
            reverification_context if reverification_context else None
        ),
        "contract_json": _json_fence(_prompt_facing_sound_contract(lane_scoped_contract)),
        "rubric_json": _json_fence(rubric),
        "artifact_delivery_json": _json_fence(_prompt_facing_artifact_delivery(artifact_delivery)),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
        "structured_request_path": str(_structured_request_path_for(raw_output_path)),
    }
    return _render_prompt_from_fragments(
        contract_prompt_fragments(sound_contract),
        context,
    )


def build_audit_prompt(
    *,
    request: Dict[str, Any],
    repo_path: Path,
    runtime_root: Path,
    raw_output_path: Path,
    done_path: Path,
    context_json_path: Path,
) -> str:
    """Cleanup-v2 (audit Finding 1): build the cleanup-phase audit-burst
    prompt. Parallel to `build_review_prompt`; reads from
    `request["audit_contract"]` (populated by
    `request_contracts::audit_contract_payload`).
    """
    audit_contract = request_contract_block(request, "audit_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(audit_contract)
    dag_view = audit_contract.get("dag_view") or {}
    if not isinstance(dag_view, Mapping):
        raise ValueError("kernel-authored audit contract dag_view must be an object")
    protected_set = audit_contract.get("protected_statement_node_set") or []
    cleanup_audit_tasks = audit_contract.get("cleanup_audit_tasks") or []
    cleanup_audit_scratchpad = audit_contract.get("cleanup_audit_scratchpad") or ""
    latest_rejection = audit_contract.get("latest_audit_rejection_reason") or ""
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=audit_contract,
        command_context={"context_json_path": str(context_json_path)},
    )
    rejection_block = (
        f"The previous audit burst was rejected. Reason: {latest_rejection}\n"
        "Repair the issue described above in your next emission."
        if latest_rejection
        else "(no previous-burst rejection — this is a clean burst)"
    )
    scratchpad_block = (
        cleanup_audit_scratchpad.strip()
        if isinstance(cleanup_audit_scratchpad, str) and cleanup_audit_scratchpad.strip()
        else "(scratchpad is empty)"
    )
    context = {
        "repo_path": str(repo_path),
        "trellis_scheme_reference_path": str(_scheme_reference_path(repo_path)),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "dag_view_json": _json_fence(dict(dag_view)),
        "protected_statement_node_set_json": _json_fence(protected_set),
        "cleanup_audit_tasks_json": _json_fence(cleanup_audit_tasks),
        "cleanup_audit_scratchpad": scratchpad_block,
        "latest_audit_rejection_block": rejection_block,
        "contract_json": _json_fence(_contract_prompt_facing_view(audit_contract)),
        "artifact_delivery_json": _json_fence(_prompt_facing_artifact_delivery(artifact_delivery)),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
        "structured_request_path": str(_structured_request_path_for(raw_output_path)),
    }
    return _render_prompt_from_fragments(
        contract_prompt_fragments(audit_contract),
        context,
    )


def build_stuck_math_audit_prompt(
    *,
    request: Dict[str, Any],
    repo_path: Path,
    runtime_root: Path,
    raw_output_path: Path,
    done_path: Path,
    context_json_path: Path,
) -> str:
    """Build the StuckMathAudit/NeedInputAuditor prompt.

    This is distinct from `build_audit_prompt`, which is the cleanup-v2
    audit-task role. The kernel-authored `stuck_math_audit_contract`
    owns the fragment list and schema details; the bridge renders paths,
    artifact delivery commands, and repo-local history entry points.
    """
    audit_contract = request_contract_block(request, "stuck_math_audit_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(audit_contract)
    audit_latch = (
        audit_contract.get("audit_latch")
        or request.get("audit_latch")
        or request.get("stuck_math_audit")
        or request_summary.get("audit_latch")
        or request_summary.get("stuck_math_audit")
    )
    previous_plan = (
        audit_contract.get("previous_audit_plan_snapshot")
        if "previous_audit_plan_snapshot" in audit_contract
        else request.get("previous_audit_plan_snapshot")
    )
    latest_rejection = str(
        audit_contract.get("latest_stuck_math_audit_rejection_reason")
        or request.get("latest_stuck_math_audit_rejection_reason")
        or ""
    ).strip()
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=audit_contract,
        command_context={"context_json_path": str(context_json_path)},
    )
    scratch_path = (
        repo_path
        / ".trellis"
        / "stuck-math-audit"
        / f"cycle-{request.get('cycle', 'unknown')}-request-{request.get('id', 'unknown')}"
    )
    scratch_path.mkdir(parents=True, exist_ok=True)
    rejection_block = (
        f"The previous StuckMathAudit burst was rejected. Reason: {latest_rejection}\n"
        "Repair the issue described above in your next emission."
        if latest_rejection
        else "(no previous StuckMathAudit rejection for this request)"
    )
    state_dir = repo_path / ".trellis"
    context = {
        "repo_path": str(repo_path),
        "trellis_scheme_reference_path": str(_scheme_reference_path(repo_path)),
        "paper_tex_path": _paper_tex_path(repo_path),
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(request_summary),
        "audit_latch_json": _json_fence(audit_latch),
        "previous_audit_plan_snapshot_json": _json_fence(previous_plan),
        "latest_stuck_math_audit_rejection_block": rejection_block,
        "stuck_math_audit_scratch_path": str(scratch_path),
        "context_json_path": str(context_json_path),
        "burst_history_path": str(state_dir / "logs" / "burst-history.jsonl"),
        "audit_chats_glob": str(state_dir / "chats" / "cycle-*" / "trellis_stuck_math_audit_*"),
        "reviewer_chats_glob": str(state_dir / "chats" / "cycle-*" / "trellis_review_*_decision"),
        "worker_chats_glob": str(state_dir / "chats" / "cycle-*" / "trellis_worker_*_result"),
        "audit_scratch_glob": str(state_dir / "stuck-math-audit" / "cycle-*"),
        "contract_json": _json_fence(_contract_prompt_facing_view(audit_contract)),
        "artifact_delivery_json": _json_fence(_prompt_facing_artifact_delivery(artifact_delivery)),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
        "structured_request_path": str(_structured_request_path_for(raw_output_path)),
    }
    return _render_prompt_from_fragments(
        contract_prompt_fragments(audit_contract),
        context,
    )


def build_review_prompt(
    *,
    request: Dict[str, Any],
    repo_path: Path,
    runtime_root: Path,
    raw_output_path: Path,
    done_path: Path,
    context_json_path: Path,
    theorem_initial_dag_size_guidance: str = "15-50",
) -> str:
    review_contract = request_contract_block(request, "review_contract")
    project_invariants = request_project_invariants(request)
    request_summary = contract_request_summary(review_contract)
    blocker_partition = contract_required_mapping(review_contract, "blocker_partition")
    verifier_evidence = review_contract.get("verifier_evidence")
    if not isinstance(verifier_evidence, Mapping):
        raise ValueError("kernel-authored review contract verifier_evidence must be an object")
    blocker_choices = blocker_partition.get("choices")
    if not isinstance(blocker_choices, list):
        raise ValueError("kernel-authored review contract blocker_partition.choices must be a list")
    # Trim 7: when no verifier has run yet for this cycle (post-Worker
    # reviewer state), all four lanes are empty. The empty
    # `{paper:{}, substantiveness:{}, corr:{}, sound:{}}` JSON block is
    # information-free in that state. Replace with a one-line sentinel.
    if _verifier_evidence_is_empty(verifier_evidence):
        verifier_evidence_block = (
            "(no verifier evidence yet for this cycle -- the verifiers have "
            "not run on the current worker output)"
        )
    else:
        verifier_evidence_block = _json_fence(verifier_evidence)
    # Bridge-to-kernel migration batch 2 (2026-06-04): the kernel-rendered
    # `blocker_choices_block` is always present on review contracts; the bridge
    # only writes the sidecar payload (when overflow) and substitutes the on-
    # disk-path placeholders. Selection / formatting / inline-limit policy all
    # live kernel-side now (`review_blocker_choices_block` in
    # `kernel/src/request_contracts.rs`).
    blocker_choices_summary_text = _resolve_review_blocker_choices_block(
        review_contract=review_contract,
        raw_output_path=raw_output_path,
        context_json_path=context_json_path,
    )
    # Trim 8: when blocker_choices is empty, the rendered "Available
    # blocker ids" fragment otherwise shows "0 blocker choices total.
    # ... (none)" plus the index-table header. Replace with a one-line
    # sentinel.
    if not blocker_choices:
        blocker_choices_summary_text = (
            "(no blockers yet -- Pass-status blockers appear here once the "
            "verifier lanes report verdicts)"
        )
    artifact_delivery = _validated_artifact_instructions(
        repo_path=repo_path,
        raw_output_path=raw_output_path,
        done_path=done_path,
        contract=review_contract,
        command_context={"context_json_path": str(context_json_path)},
    )
    # Lives under <repo>/.trellis/ so it's covered by the bwrap writable
    # allowlist (see trellis/sandbox.py:_repo_writable_paths). The previous
    # location at <runtime>/stuck-math-audit/ is sandbox-readonly for the
    # reviewer/worker roles — writes succeeded inside the bwrap tmpfs overlay
    # but never persisted to the host, so reviewer_lean_product.scratch_file
    # paths in the response pointed at non-existent files.
    stuck_math_audit_scratch_path = (
        repo_path
        / ".trellis"
        / "stuck-math-audit"
        / f"cycle-{request.get('cycle', 'unknown')}-request-{request.get('id', 'unknown')}"
    )
    stuck_math_audit_scratch_path.mkdir(parents=True, exist_ok=True)
    audit_plan_prompt_view, audit_report_path, audit_report_prompt_line_limit = (
        _stuck_math_audit_plan_prompt_view(
            repo_path=repo_path,
            request=request,
            audit_plan=review_contract.get("audit_plan"),
        )
    )
    # Option A audit-plan visibility unification (kernel commit 1387341):
    # when the kernel has no live `audit_plan` but does have a snapshot
    # (`previous_audit_plan_snapshot`), it inserts the advisory
    # `review/common/29c_last_audit_plan.md` fragment which references
    # `{{previous_audit_plan_snapshot_json}}`. Source the value the same
    # way the StuckMathAudit build does so the placeholder always
    # resolves. Mirrors bridge_prompts.py:2100 (SMA).
    previous_audit_plan_snapshot = (
        review_contract.get("previous_audit_plan_snapshot")
        if "previous_audit_plan_snapshot" in review_contract
        else request.get("previous_audit_plan_snapshot")
    )
    prompt_request_summary = dict(request_summary)
    if "audit_plan" in prompt_request_summary:
        prompt_request_summary["audit_plan"] = {
            "see_section": "Current Audit Plan",
            "report_path": audit_report_path,
            "report_prompt_line_limit": audit_report_prompt_line_limit,
        }
    context = {
        "repo_path": str(repo_path),
        "trellis_scheme_reference_path": str(_scheme_reference_path(repo_path)),
        "paper_tex_path": _paper_tex_path(repo_path),
        "scratch_workspace_path": str(worker_scratch_dir(repo_path)),
        "stuck_math_audit_scratch_path": str(stuck_math_audit_scratch_path),
        "stuck_math_audit_json": _json_fence(request_summary.get("stuck_math_audit")),
        "audit_plan_json": _json_fence(audit_plan_prompt_view),
        "previous_audit_plan_snapshot_json": _json_fence(previous_audit_plan_snapshot),
        "audit_report_path": audit_report_path,
        "audit_report_prompt_line_limit": audit_report_prompt_line_limit,
        "filespec_path": str(_filespec_path(repo_path)),
        "project_invariants_json": _json_fence(project_invariants),
        "request_summary_json": _json_fence(prompt_request_summary),
        "deterministic_worker_rejection_reasons_json": _json_fence(
            request_summary.get("deterministic_worker_rejection_reasons", [])
        ),
        "latest_review_rejection_reasons_json": _json_fence(
            request_summary.get("latest_review_rejection_reasons", [])
        ),
        "deterministic_worker_rejection_artifacts_text": _deterministic_rejection_artifacts_text(
            repo_path=repo_path,
            runtime_root=runtime_root,
            deterministic_rejection_reasons=request_summary.get(
                "deterministic_worker_rejection_reasons", []
            ),
        ),
        "blocker_choices_summary": blocker_choices_summary_text,
        "verifier_evidence_json": verifier_evidence_block,
        "contract_json": _json_fence(_prompt_facing_review_contract(review_contract)),
        "artifact_delivery_json": _json_fence(_prompt_facing_artifact_delivery(artifact_delivery)),
        "raw_output_path": artifact_delivery["raw_output_path"],
        "done_path": artifact_delivery["done_path"],
        "json_check_command": artifact_delivery["json_check_command"],
        "acceptance_check_command": artifact_delivery["acceptance_check_command"],
        "acceptance_check_guidance": artifact_delivery["acceptance_check_guidance"],
        "acceptance_check_block": artifact_delivery["acceptance_check_block"],
        "theorem_initial_dag_size_guidance": theorem_initial_dag_size_guidance,
    }
    # Reviewer source-recourse: when the kernel sees both env vars
    # populated, it includes `review/common/05_source_recourse.md` in the
    # fragment list (see `reviewer_source_recourse_available` in
    # kernel/src/request_contracts.rs). The fragment references
    # `{{reviewer_source_snapshot_path}}` and `{{reviewer_source_sha}}`,
    # so we mirror the same env-var check here. If unset, the fragment
    # is absent from the kernel-emitted list and these keys are
    # consequently never read — silent degradation by design.
    snapshot = os.environ.get("TRELLIS_REVIEWER_SOURCE_SNAPSHOT", "").strip()
    source_sha = os.environ.get("TRELLIS_REVIEWER_SOURCE_SHA", "").strip()
    if snapshot and source_sha:
        context["reviewer_source_snapshot_path"] = snapshot
        context["reviewer_source_sha"] = source_sha
    # Reviewer's structured request file is the kernel-emitted
    # `.request.json` (sibling of `.context.json`). The `.context.json`
    # is the bridge-side scrubbed view; `.request.json` is the
    # authoritative kernel emission with all fields un-truncated.
    context["structured_request_path"] = str(_structured_request_path_for(raw_output_path))
    return _render_prompt_from_fragments(
        contract_prompt_fragments(review_contract),
        context,
    )

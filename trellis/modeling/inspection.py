"""Focused agent context and read-only navigation of modeling evidence."""
from __future__ import annotations

from pathlib import Path
import json
import re

from trellis.modeling.observations import read_json


class ContextPreviewBudgetError(ValueError):
    """The requested preview structure cannot fit its character budget."""


def _symbol_rank(symbol: str, *identifiers: str) -> int:
    if symbol in identifiers:
        return 0
    return 1 if any(name.endswith((f".{symbol}", f"::{symbol}")) for name in identifiers) else 2


def context_preview(context: dict, limit: int = 16000, *, details: str = "full request file",
                    preserve_keys: bool = False, structured_arrays: bool = False) -> str:
    from trellis.runtime.bridge_prompts import _truncate_long_arrays

    def short_strings(value, text_limit):
        if isinstance(value, str) and len(value) > text_limit:
            return value[:text_limit] + f"... [truncated; {len(value) - text_limit} more characters in {details}]"
        if isinstance(value, dict):
            return {k: short_strings(v, text_limit) for k, v in value.items()}
        if isinstance(value, list):
            return [short_strings(v, text_limit) for v in value]
        return value

    # A completed route adds several nested records. Even after arrays are
    # shortened, a 500-character floor per string can exceed the space left
    # by the role instructions. Continue shrinking prose while keeping keys;
    # the complete context remains in the referenced request file.
    budgets = [(15, 4000), (8, 4000), (4, 4000), (2, 2000), (1, 1000),
               (0, 500), (0, 250), (0, 100), (0, 0)]
    for threshold, text_limit in budgets:
        omissions = {} if structured_arrays else None
        trimmed = _truncate_long_arrays(context, threshold, preserve_keys=preserve_keys, array_omissions=omissions)
        for path, count in (omissions or {}).items():
            key = {("declarations",): "declarations_omitted",
                   ("diagnostics",): "diagnostic_groups_omitted",
                   ("reference_candidates_from",): "reference_candidates_omitted"}.get(path)
            if key:
                trimmed[key] = trimmed.get(key, 0) + count
            else:
                pointer = "/" + "/".join(str(key).replace("~", "~0").replace("/", "~1") for key in path)
                trimmed.setdefault("array_items_omitted", {})[pointer] = count
        preview = short_strings(trimmed, text_limit)
        rendered = json.dumps(preview, indent=2)
        for kind in ("items", "keys"):
            rendered = rendered.replace(f"more {kind} in full request file", f"more {kind} in {details}")
        if len(rendered) <= limit:
            return rendered
    raise ContextPreviewBudgetError("Context preview budget is too small")


def cli_preview(context: dict, limit: int = 16000, *, details: str) -> str:
    return context_preview(context, limit, details=details, preserve_keys=True, structured_arrays=True)


def observation_summary(observation: dict, *, diagnostic_details: bool = True) -> dict:
    fields = ("work_fingerprint", "observation_file", "source_diff", "declaration_index", "composed_index",
              "combined_model", "error", "stable", "placeholders", "elaboration_placeholders",
              "extraction_arguments", "elaboration_options", "axiom_dependencies")
    summary = {key: observation[key] for key in fields if key in observation}
    summary["checks"] = [{**{key: step[key] for key in ("name", "module", "exit_code", "cached", "seconds", "log") if key in step},
                          "timeout": step.get("timeout", step.get("execution", {}).get("timeout", False)),
                          "timeout_seconds": step.get("timeout_seconds", step.get("execution", {}).get("timeout_seconds"))}
                         for step in observation.get("steps", [])]
    summary["remaining_axioms"] = len(observation.get("remaining_axioms", observation.get("axioms", [])))
    diagnostics = observation.get("diagnostics", [])
    failing = next((step for step in observation.get("steps", []) if step.get("exit_code") != 0), None)

    def priority(diagnostic):
        failed_stage = bool(failing and diagnostic.get("stage") == failing["name"]
                            and diagnostic.get("module") == failing.get("module"))
        error = bool(re.search(r"\berror\b|Could not translate", diagnostic.get("text", ""), re.I))
        return (not failed_stage, not error)

    summary["diagnostic_count"] = len(diagnostics)
    groups = {}
    for diagnostic in sorted(diagnostics, key=priority):
        headline = re.sub(r"^.*?:\d+:\d+:\s*", "", diagnostic.get("text", ""))
        source = diagnostic.get("source") or {}
        key = (diagnostic.get("stage"), diagnostic.get("module"), headline,
               tuple(diagnostic.get("declarations", [])),
               tuple(source.get(field) for field in ("path", "start", "end")))
        if key not in groups:
            groups[key] = {**diagnostic, "occurrences": 0}
        groups[key]["occurrences"] += 1
    summary["diagnostic_group_count"] = len(groups)
    summary["diagnostics"] = list(groups.values())[:8]
    if not diagnostic_details:
        summary["diagnostics"] = [{key: item[key] for key in
                                   ("stage", "module", "line", "text", "source", "occurrences") if key in item}
                                  for item in summary["diagnostics"]]
    summary["diagnostic_groups_omitted"] = max(0, len(groups) - 8)
    return {key: summary[key] for key in ("checks", "diagnostic_count", "diagnostic_group_count", "diagnostics",
                                        "diagnostic_groups_omitted", "remaining_axioms", *fields)
            if key in summary}


def _approved_setup_reopening(state: dict) -> dict | None:
    if state["stage"] != "setup" or "setup" in state.get("audits", {}):
        return None
    for event in reversed(state.get("history", [])):
        task, response = event.get("task", {}), event.get("response", {})
        reason, sequence = response.get("reason"), event.get("sequence")
        if (event.get("stage") == "setup" and task.get("role") == "audit"
                and task.get("kind") == "advance" and task.get("stage") == "setup"
                and response.get("decision") == "accept" and event.get("feedback") == ""):
            return None
        # Driver.apply records the accepted reason as feedback only after the
        # adaptation-to-setup transition succeeds. Rejected attempts stay in
        # history too, so the response's decision alone is not sufficient.
        if (event.get("stage") == "adaptation" and task.get("role") == "audit"
                and task.get("kind") == "reopen" and response.get("decision") == "accept"
                and isinstance(reason, str) and reason.strip() and event.get("feedback") == reason
                and type(sequence) is int and sequence > 0):
            return {"audit_sequence": sequence, "from_stage": "adaptation", "to_stage": "setup",
                    "decision": "accept", "reason": reason}
    return None


def role_context(state: dict, task: dict) -> dict:
    role = task["role"]
    context = {key: state[key] for key in ("stage", "status", "declined", "feedback")}
    reopening = _approved_setup_reopening(state)
    if reopening:
        context["approved_setup_reopening"] = reopening
        if context["feedback"] == reopening["reason"]:
            context["feedback"] = ""
    context["observation"] = observation_summary(state["observation"])
    if state.get("export_check"):
        context["export_check"] = state["export_check"]
    if role in {"reviewer", "worker"}:
        context["stage_approvals"] = {key: {"decision": value["decision"], "basis": value["basis"]}
                                      for key, value in state["audits"].items()}
        latest = next((event for event in reversed(state["history"]) if event["task"]["role"] == "worker"), None)
        if latest:
            context["latest_work"] = latest["response"]
    for key in ("route_required", "scope", "limitations"):
        if state.get(key): context[key] = state[key]
    if state.get("route_decisions"):
        latest = state["route_decisions"][-1]
        context["approved_route"] = {"resolution": latest["resolution"], "assessment": latest["assessment"], "sequence": latest["sequence"]}
    return context


def inspect_observation(observation_file: Path, *, symbol: str = "", source: str = "", module: str = "") -> dict:
    observation = read_json(observation_file)
    index_path = observation.get("composed_index") or observation.get("declaration_index")
    if not index_path:
        return {**observation_summary(observation), "full_index": None,
                "index_status": "unavailable: the Aeneas extraction stage produces the declaration index",
                "query": {"symbol": symbol, "source": source, "module": module}}
    index = read_json(Path(index_path)) if index_path else {"declarations": [], "imports": {}}
    selected = [item for item in index["declarations"]
                if (not symbol or symbol in item["name"] or symbol in (item.get("source_symbol") or ""))
                and (not source or source in (item.get("source") or {}).get("path", ""))
                and (not module or module == item["module"])]
    if symbol:
        selected.sort(key=lambda item: _symbol_rank(symbol, item["name"], item.get("source_symbol") or ""))
    names = {item["name"] for item in selected}
    if symbol or source or module:
        observation = {**observation, "diagnostics": [item for item in observation.get("diagnostics", [])
                       if (not module or item.get("module") == module)
                       and (not (symbol or source) or names.intersection(item.get("declarations", []))
                            or (source and source in item.get("source", {}).get("path", "")))]}
    result = {"full_index": index_path, "index_status": "available", "declaration_count": len(selected),
              "query": {"symbol": symbol, "source": source, "module": module},
              **observation_summary(observation)}
    if symbol or source or module:
        fields = ("name", "kind", "module", "file", "line", "source_symbol", "source",
                  "is_opaque", "can_fail", "can_diverge")
        result["declarations"] = [{key: item[key] for key in fields if key in item} for item in selected[:20]]
        result["declarations_omitted"] = max(0, len(selected) - 20)
        references = [item["name"] for item in index["declarations"]
                      if names.intersection(item.get("reference_candidates", []))]
        result["reference_candidates_from"] = references[:20]
        result["reference_candidates_omitted"] = max(0, len(references) - 20)
    result["imports"] = index["imports"]
    return result


def inspect_ir(observation_file: Path, *, tools: dict, output: Path, symbol: str = "",
               source_symbols: tuple[str, ...] = ()) -> dict:
    """Locate headers in Charon's own readable IR; these are navigation hints."""
    from trellis.json_io import save_json
    from trellis.modeling.environment import run_source_command, source_environment
    from trellis.pv_execution import execution_timeout

    observation = read_json(observation_file)
    raw = observation_file.resolve().parent / "Generated/m.llbc"
    for step in observation.get("steps", []):
        argv = step.get("argv", [])
        if step.get("name") == "charon" and "--dest-file" in argv:
            position = argv.index("--dest-file") + 1
            if position < len(argv):
                raw = Path(argv[position])
                break
    result = {"observation_file": str(observation_file.resolve()), "raw_file": str(raw),
              "query": {"symbol": symbol}, "status": "unavailable"}
    if not raw.is_file():
        return {**result, "error": "No retained Charon output at this path"}
    output = output.resolve()
    pretty = output / "charon.llbc.txt"
    execution = run_source_command([tools["charon"], "pretty-print", str(raw)], cwd=output,
                                   env=source_environment(tools, output), stdout=pretty,
                                   stderr=output / "stderr", timeout=execution_timeout(tools, "charon"))
    record = output / "execution.json"
    save_json(record, execution)
    result.update(pretty_file=str(pretty), execution=str(record), stderr=execution["stderr"],
                  exit_code=execution["exit_code"], timeout=execution["timeout"], timeout_seconds=execution["timeout_seconds"])
    if execution["exit_code"] != 0 or execution["timeout"]:
        return {**result, "status": "failed", "error": "Charon could not format the retained IR"}

    # Full-name comments precede shortened headers. Otherwise the printed
    # signature carries the name. Do not interpret bodies or serialized ASTs.
    header = re.compile(r'^(?:pub )?(?:unsafe )?(?:extern "[^"]+" )?'
                        r'(?:(?:opaque )?(?:fn|type|struct|enum|trait)|global|const|static|thread_local|impl)\b')
    entries = []
    named = False
    with pretty.open(errors="replace") as stream:
        for line, text in enumerate(stream, 1):
            text = text.rstrip("\n")
            if text.startswith("// Full name: "):
                entries.append({"name": text.removeprefix("// Full name: "), "line": line, "text": text})
                named = True
            elif match := header.match(text):
                if not named:
                    name = re.match(r'(?:\{[^}]*\}|[^\s(<])+', text[match.end():].lstrip())
                    entries.append({"name": name[0].rstrip(":") if name else "", "line": line, "text": text})
                named = False
    index = output / "headers.json"
    save_json(index, {"raw_file": str(raw), "pretty_file": str(pretty), "headers": entries})
    queries = (symbol, *source_symbols) if symbol else ()
    selected = [entry for entry in entries if not queries or any(query in entry["name"] for query in queries)]
    if queries:
        selected.sort(key=lambda entry: min(_symbol_rank(query, entry["name"]) for query in queries))
    return {**result, "status": "available", "full_index": str(index),
            "header_count": len(selected), "headers": selected}

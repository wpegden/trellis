"""Shared extraction arguments for preparation checks and native bootstrap."""
from __future__ import annotations


def extraction_arguments(plan: dict, *, emit_json: bool = False) -> dict[str, list[str]]:
    if not isinstance(plan, dict):
        raise ValueError("Extraction configuration must be a JSON object")
    arguments = {}
    for field in ("cargo_args", "charon_args", "aeneas_args"):
        value = plan.get(field, [])
        if not isinstance(value, list) or not all(isinstance(arg, str) for arg in value):
            raise ValueError(f"{field} must be a string list")
        arguments[field] = list(value)
    for field in ("charon_args", "aeneas_args"):
        if any(arg.split("=", 1)[0] in {"-dest", "-backend", "--dest-file", "--dest", "--"}
               for arg in arguments[field]):
            raise ValueError("The driver owns the backend and generated output paths")
    aeneas = arguments["aeneas_args"]
    if "-split-files" in aeneas and "-subdir" not in aeneas:
        aeneas += ["-subdir", "M"]
    if emit_json and "-emit-json" not in aeneas:
        aeneas.append("-emit-json")
    return arguments

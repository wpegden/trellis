"""Immutable extraction evidence and mechanical-to-process model lineage.

Records and blobs are content addressed. Export never repairs or writes its source;
legacy recovery is explicitly partial and occurs only in the export destination.
No Lean declarations are parsed here: model membership comes from the existing
extraction-model and model-support registries.
"""
from __future__ import annotations

import difflib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import tempfile
import time

from scripts.pv_source_tree import canonical_json, source_tree_manifest

SCHEMA = "trellis-model-history/v1"
HISTORY = Path(".trellis/modeling/history")
BASELINE = Path("modeling-input/mechanical-model-baseline")
EXCLUDED = {"target", "cargo-target", "cargo-home", "prewarm-home", "home", "tmp",
            ".git", ".lake", "__pycache__", "cache", "caches", "extraction-history"}
COMPILED = {".olean", ".ilean", ".ir", ".o", ".a", ".so", ".pyc", ".rlib", ".rmeta"}


def _tool_projection(tools: dict) -> dict:
    allowed = {"cargo", "charon", "aeneas", "lean", "lean_paths", "aeneas_lean_lib",
               "lean_toolchain", "rust_toolchain", "timeout", "timeouts", "opam_switch"}
    result = {name: value for name, value in tools.items() if name in allowed}
    if tools.get("env"):
        result["environment_value_sha256"] = {name: _sha(str(value).encode())
                                              for name, value in tools["env"].items()}
    if tools.get("profiles"):
        result["profiles"] = {name: _tool_projection(value) for name, value in tools["profiles"].items()}
    return result


def _extraction_projection(config: dict) -> dict:
    result = {key: config[key] for key in ("cargo_args", "charon_args", "aeneas_args", "preserve_history") if key in config}
    if "source_tools" in config:
        result["source_tools"] = _tool_projection(config["source_tools"])
    return result


def _sha(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _read(path: Path) -> bytes:
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f"History evidence is not a regular file: {path}")
    # Reject linked parents too; evidence must not depend on external mutable paths.
    if any(parent.is_symlink() for parent in path.parents):
        raise ValueError(f"History evidence has a symlink parent: {path}")
    return path.read_bytes()


def _json(path: Path) -> dict:
    result = json.loads(_read(path))
    if not isinstance(result, dict):
        raise ValueError(f"History object expected: {path}")
    return result


def _relative(name: str) -> str:
    path = Path(name)
    if path.is_absolute() or not path.parts or any(p in {".", ".."} for p in path.parts) or "\\" in name:
        raise ValueError(f"Invalid history path: {name}")
    if path.as_posix() != name:
        raise ValueError(f"Noncanonical history path: {name}")
    return name


def _put(path: Path, raw: bytes) -> None:
    """Publish immutable bytes atomically, checking any previously published copy."""
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists() or path.is_symlink():
        if _read(path) != raw:
            raise ValueError(f"Stored history evidence changed: {path}")
        return
    fd, temporary = tempfile.mkstemp(prefix=".history-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        try:
            os.link(temporary, path)
        except FileExistsError:
            if _read(path) != raw:
                raise ValueError(f"Stored history evidence changed: {path}")
    finally:
        os.unlink(temporary)


def _tree(root: Path, prefix: str, *, source: bool = False) -> dict[str, bytes]:
    if not root.exists():
        return {}
    from scripts.extract_pv_model import SOURCE_INPUT_IGNORED
    manifest = source_tree_manifest(root, excluded_names=SOURCE_INPUT_IGNORED if source else EXCLUDED, allow_empty=True)
    result = {}
    for item in manifest["files"]:
        name = item["relative_path"]
        if not source and Path(name).suffix in COMPILED:
            continue
        raw = _read(root / name)
        if _sha(raw) != item["sha256_of_raw_bytes"]:
            raise ValueError(f"Evidence changed during snapshot: {root / name}")
        result[f"{prefix}/{name}"] = raw
    return result


def _seal(store: Path, kind: str, metadata: dict, files: dict[str, bytes]) -> dict:
    entries = []
    for name, raw in sorted(files.items()):
        _relative(name)
        digest = _sha(raw)
        _put(store / "blobs" / digest, raw)
        entries.append({"path": name, "sha256": digest, "size": len(raw)})
    record = {"schema": SCHEMA, "kind": kind, "metadata": metadata, "files": entries}
    raw = canonical_json(record)
    identity = _sha(raw)
    _put(store / "records" / f"{identity}.json", raw)
    _catalog_add(store, identity)
    return {"id": identity, "record": f"records/{identity}.json", "kind": kind,
            "coverage": metadata.get("coverage", "captured")}


def _catalog_add(store: Path, identity: str) -> None:
    """A durable inventory makes deleted records detectable at export."""
    with (store / ".catalog.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        path = store / "catalog.json"
        catalog = _json(path) if path.exists() else {"schema": SCHEMA, "records": []}
        if identity in catalog["records"]:
            return
        catalog["records"] = sorted([*catalog["records"], identity])
        fd, temporary = tempfile.mkstemp(prefix=".catalog-", dir=store)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(canonical_json(catalog))
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)


def _verified(store: Path) -> list[tuple[dict, dict]]:
    records = store / "records"
    if not records.exists():
        if store.exists():
            raise ValueError(f"History record inventory is missing: {store}")
        return []
    if records.is_symlink():
        raise ValueError(f"Linked history records: {records}")
    catalog = _json(store / "catalog.json")
    if catalog.get("schema") != SCHEMA or catalog.get("records") != sorted(p.stem for p in records.iterdir()):
        raise ValueError(f"Stored history record inventory changed: {store}")
    result = []
    for path in sorted(records.iterdir()):
        if not re.fullmatch(r"[0-9a-f]{64}\.json", path.name):
            raise ValueError(f"Unexpected history record: {path}")
        raw = _read(path)
        if _sha(raw) != path.stem:
            raise ValueError(f"Stored history record changed: {path}")
        record = json.loads(raw)
        if record.get("schema") != SCHEMA:
            raise ValueError(f"Unsupported history schema: {path}")
        seen = set()
        for entry in record["files"]:
            name = _relative(entry["path"])
            if name in seen or not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]):
                raise ValueError(f"Invalid history file entry: {path}")
            seen.add(name)
            data = _read(store / "blobs" / entry["sha256"])
            if _sha(data) != entry["sha256"] or len(data) != entry["size"]:
                raise ValueError(f"Stored history blob changed: {entry['sha256']}")
        result.append(({"id": path.stem, "record": f"records/{path.name}",
                        "kind": record["kind"], "coverage": record["metadata"].get("coverage", "unknown")}, record))
    return result


def capture_inputs(root: Path) -> dict[str, bytes]:
    """Capture configuration before invoking tools, rather than after an edit."""
    result = {}
    for name in ("work/extraction.json", "work/SETUP.md", "tool-identities.json"):
        path = root / name
        if path.exists():
            result[f"inputs/{name}"] = _read(path)
    if (root / "config.json").exists():
        config = _json(root / "config.json")
        result["inputs/tool-configuration.json"] = canonical_json(_tool_projection(config.get("tools", {})))
        if "paper_input" in config:
            result["inputs/paper-targets.json"] = canonical_json(config["paper_input"])
            for name in config["paper_input"]["paper_files"]:
                result[f"inputs/{name}"] = _read(root / name)
    return result


def initialize_history(preparation: Path) -> dict:
    """Mark recording from creation; never backfill this marker on older runs."""
    if (preparation / HISTORY).exists():
        raise ValueError("History origin may only be recorded once at preparation creation")
    return _seal(preparation / HISTORY, "recording-origin", {
        "coverage": "captured", "recorded_ns": time.time_ns(), "from_preparation_creation": True,
    }, {})


def start_attempt(store: Path, kind: str, *, inputs: dict[str, bytes], metadata: dict) -> str:
    """Publish before external work so SIGKILL cannot erase an attempted run."""
    metadata = dict(metadata)
    if "extraction_config" in metadata:
        metadata["extraction_config"] = _extraction_projection(metadata["extraction_config"])
    return _seal(store, "attempt-start", {
        **metadata, "coverage": "captured", "attempt_kind": kind,
        "recorded_ns": time.time_ns(),
    }, inputs)["id"]


def _observation_files(output: Path) -> dict[str, bytes]:
    files = _tree(output / "source", "source", source=True)
    files.update(_tree(output / "Generated", "raw-generated"))
    files.update(_tree(output / "Lean", "composed-lean"))
    for path in sorted(output.iterdir()):
        if path.is_file() and path.suffix in {".json", ".log", ".diff", ".lean"}:
            files[f"observation/{path.name}"] = _read(path)
    return files


def record_observation(root: Path, output: Path, observation: dict, *, inputs: dict[str, bytes],
                       attempt_start_id: str | None = None) -> dict:
    files = {**inputs, **_observation_files(output)}
    return _seal(root / HISTORY, "observation", {
        "coverage": "captured", "recorded_ns": time.time_ns(),
        "attempt_start_id": attempt_start_id,
        "observation_file": str(output / "observation.json"), "observation": observation,
        "source_inputs_available": (output / "source").is_dir(),
        "raw_output_available": (output / "Generated").is_dir(),
    }, files)


def record_extractor_attempt(work_root: Path, scratch: Path, *, metadata: dict,
                             inputs: dict[str, bytes] | None = None,
                             attempt_start_id: str | None = None) -> dict:
    """Called before extract_once destroys scratch, including failed partial runs."""
    metadata = dict(metadata)
    if "extraction_config" in metadata:
        metadata["extraction_config"] = _extraction_projection(metadata["extraction_config"])
    return _seal(work_root / "extraction-history", "extractor-attempt", {
        "coverage": "captured", "recorded_ns": time.time_ns(), **metadata,
        "attempt_start_id": attempt_start_id,
    }, {**(inputs or {}), **_tree(scratch / "crate", "source", source=True),
        **_tree(scratch / "gen", "raw-generated")})


def record_bootstrap(root: Path, staging: Path, repo: Path, *, inputs: dict[str, bytes],
                     argv: list[str], exit_code: int | None, tool_identities: dict,
                     attempt_start_id: str | None = None) -> dict:
    files = {**inputs, **_tree(staging / "extraction", "bootstrap-extraction")}
    # The low-level extractor journal retains raw LLBC and failed partial output
    # before the bootstrap destroys its scratch directory.
    raw_store = staging / "extraction/extraction-history"
    low_level = _verified(raw_store)
    for reference, record in low_level:
        _copy_record(raw_store, root / HISTORY, reference, record)
    for name in ("setup.log",):
        if (staging / name).exists():
            files[f"bootstrap/{name}"] = _read(staging / name)
    if (staging / "extraction-config.json").exists():
        files["bootstrap/extraction-config.json"] = canonical_json(_extraction_projection(_json(staging / "extraction-config.json")))
    for name in ("generated.lean", "pv_tablet.json", "tcb_manifest.json", "extraction_evidence.json"):
        if (repo / name).exists():
            files[f"bootstrap-output/{name}"] = _read(repo / name)
    return _seal(root / HISTORY, "native-bootstrap", {
        "coverage": "captured" if low_level else "captured_no_extraction",
        "recorded_ns": time.time_ns(), "argv": argv, "exit_code": exit_code,
        "attempt_start_id": attempt_start_id,
        "tool_identities": tool_identities, "extractor_records": [r["id"] for r, _ in low_level],
        "raw_attempts_available": bool(low_level),
        "config_template_sha256": _sha(_read(staging / "config-template.json")) if (staging / "config-template.json").exists() else None,
        "limitation": "" if low_level else "No low-level extractor attempt was recorded (bootstrap may have failed before extraction).",
    }, files)


def _copy_record(source: Path, destination: Path, reference: dict, record: dict) -> None:
    for entry in record["files"]:
        raw = _read(source / "blobs" / entry["sha256"])
        if _sha(raw) != entry["sha256"] or len(raw) != entry["size"]:
            raise ValueError(f"Stored history blob changed during export: {entry['sha256']}")
        _put(destination / "blobs" / entry["sha256"], raw)
    raw = _read(source / reference["record"])
    if _sha(raw) != reference["id"] or json.loads(raw) != record:
        raise ValueError("Stored history record changed during export")
    _put(destination / reference["record"], raw)
    _catalog_add(destination, reference["id"])


def export_history(preparation: Path, destination: Path, *, recover_available: bool = True) -> dict:
    """Export verified records; optionally recover available legacy observations.

    The preparation is read only. Recovery cannot establish that deleted older
    observations ever existed, so its coverage is never described as complete.
    """
    preparation, destination = Path(preparation), Path(destination)
    if destination.resolve().is_relative_to(preparation.resolve()):
        raise ValueError("History export destination must be outside preparation")
    records = _verified(preparation / HISTORY)
    known = set()
    for reference, record in records:
        _copy_record(preparation / HISTORY, destination, reference, record)
        if record["kind"] == "observation":
            known.add(record["metadata"].get("observation_file"))
    # A killed bootstrap may not have copied its child's durable journal into
    # the preparation. Import that journal without writing to its old workspace.
    if recover_available:
        ids = {ref["id"] for ref, _ in records}
        finished = {record["metadata"].get("attempt_start_id") for _, record in records
                    if record["kind"] == "native-bootstrap"}
        for reference, record in list(records):
            metadata = record["metadata"]
            if (record["kind"] == "attempt-start" and metadata["attempt_kind"] == "native-bootstrap"
                    and reference["id"] not in finished):
                child_store = Path(metadata["output_path"]) / "extraction/extraction-history"
                for ref, child in _verified(child_store):
                    if ref["id"] not in ids:
                        _copy_record(child_store, destination, ref, child)
                        records.append((ref, child))
                        ids.add(ref["id"])
    references = [reference for reference, _ in records]
    if recover_available:
        paths = set((preparation / "observations").glob("*/observation.json"))
        state = preparation / ".trellis/modeling/state.json"
        if state.exists():
            name = _json(state).get("observation", {}).get("observation_file")
            if name:
                paths.add(Path(name))
        for path in sorted(paths):
            if str(path) in known or not path.is_file():
                continue
            observation = _json(path)
            references.append(_seal(destination, "observation", {
                "coverage": "recovered_partial", "observation_file": str(path),
                "observation": observation,
                "limitation": "Recovered currently available artifacts; original tool invocation inputs and prior overwritten/deleted attempts may be missing.",
            }, _observation_files(path.parent)))
    starts = {ref["id"]: record for ref, record in records if record["kind"] == "attempt-start"}
    completed = set()
    for _, record in records:
        start_id = record["metadata"].get("attempt_start_id")
        if start_id and record["kind"] in {"observation", "native-bootstrap", "extractor-attempt"}:
            if start_id not in starts or starts[start_id]["metadata"]["attempt_kind"] != record["kind"]:
                raise ValueError("Extraction completion has a missing or mismatched attempt start")
            completed.add(start_id)
    interrupted = []
    for start_id in sorted(starts.keys() - completed):
        metadata = starts[start_id]["metadata"]
        partial = {}
        output = Path(metadata["output_path"])
        if recover_available and output.exists():
            if metadata["attempt_kind"] == "observation":
                partial = _observation_files(output)
            elif metadata["attempt_kind"] == "extractor-attempt":
                partial = {**_tree(output / "crate", "source", source=True),
                           **_tree(output / "gen", "raw-generated")}
            elif metadata["attempt_kind"] == "native-bootstrap":
                partial = _tree(output / "extraction", "bootstrap-extraction")
                if (output / "setup.log").exists():
                    partial["bootstrap/setup.log"] = _read(output / "setup.log")
                generated = Path(metadata["repo_path"]) / "generated.lean"
                if generated.exists():
                    partial["bootstrap-output/generated.lean"] = _read(generated)
        recovered = None
        if partial:
            recovered = _seal(destination, "interrupted-attempt-artifacts", {
                "coverage": "recovered_partial", "attempt_start_id": start_id,
                "status": "interrupted_or_inflight",
                "limitation": "Available bytes recovered without claiming tool completion.",
            }, partial)
            references.append(recovered)
        interrupted.append({"start_id": start_id, "kind": metadata["attempt_kind"],
                            "status": "interrupted_or_inflight", "partial_artifacts": recovered,
                            "artifact_coverage": "recovered_partial" if recovered else "unavailable"})
    complete = (not interrupted and any(record["kind"] == "recording-origin" for _, record in records)
                and all(ref["coverage"] in {"captured", "captured_no_extraction"} for ref in references))
    manifest = {"schema": SCHEMA, "records": sorted(references, key=lambda r: r["id"]),
                "coverage": "interrupted_or_inflight_attempts" if interrupted else "recorded_from_creation" if complete else "recorded_history_with_possible_legacy_gaps" if records else "recovered_partial" if references else "unavailable",
                "history_complete": complete,
                "interrupted_attempts": interrupted,
                "limitations": [] if complete else ["Capture covers recorded attempts. No claim is made about attempts before history recording was installed or artifacts already deleted."]}
    _put(destination / "manifest.json", canonical_json(manifest))
    return manifest


def _model_files(repo: Path) -> tuple[dict[str, bytes], dict]:
    from trellis.modeling.modules import imports
    from trellis.pv_model_support import _validate_manifest, empty_model_support

    config = _json(repo / "trellis.config.json")
    models = config.get("pv_tablet", {}).get("extraction_models", [])
    registry = repo / "Tablet/model-support.json"
    support = _json(registry) if registry.exists() else empty_model_support()
    roots, _ = _validate_manifest(support)
    nodes = {item.get("node", item.get("id")) for item in models} | set(roots)
    nodes.discard(None)
    for node in ("Preamble", "Assumptions"):
        if (repo / "Tablet" / f"{node}.lean").exists():
            nodes.add(node)
    files = {}
    pending, seen = list(nodes), set()
    while pending:
        node = pending.pop()
        if node in seen:
            continue
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_']*", node):
            raise ValueError(f"Invalid registered model node: {node}")
        seen.add(node)
        path = repo / "Tablet" / f"{node}.lean"
        raw = _read(path)
        files[f"Tablet/{node}.lean"] = raw
        tex = path.with_suffix(".tex")
        if tex.exists():
            files[f"Tablet/{node}.tex"] = _read(tex)
        pending.extend(module[len("Tablet."):] for module in imports(raw.decode()) if module.startswith("Tablet."))
    for name in ("generated.lean", "Tablet/model-support.json", "modeling-input/manifest.json",
                 "modeling-input/extraction.json", "modeling-input/tool-identities.json"):
        if (repo / name).exists():
            files[name] = _read(repo / name)
    if "modeling-input/manifest.json" in files:
        provenance = json.loads(files["modeling-input/manifest.json"])
        if "effective_tools" in provenance:
            provenance["effective_tools"] = _tool_projection(provenance["effective_tools"])
        files["modeling-input/manifest.json"] = canonical_json(provenance)
    # Keep only the relevant config projection in the diff, avoiding unrelated
    # runtime configuration changes while preserving full model provenance.
    files["extraction-models.json"] = canonical_json(models)
    return files, {"extraction_models": models, "model_support": support, "nodes": sorted(seen)}


def record_model_baseline(preparation: Path, repo: Path) -> dict:
    provenance_path = repo / "modeling-input/manifest.json"
    if provenance_path.exists():
        provenance = _json(provenance_path)
        provenance["mechanical_model_baseline"] = str(BASELINE)
        provenance_path.write_bytes(canonical_json(provenance))
    files, registry = _model_files(repo)
    reference = _seal(repo / BASELINE, "mechanical-packaging-baseline", {
        "coverage": "captured", "registry": registry,
        "stage": "after deterministic native packaging, before process modeling and kernel initialization",
    }, files)
    _put(repo / BASELINE / "manifest.json", canonical_json({"schema": SCHEMA, "baseline": reference}))
    for ref, record in _verified(repo / BASELINE):
        _copy_record(repo / BASELINE, preparation / HISTORY, ref, record)
    return reference


def snapshot_model_lineage(repo: Path, destination: Path) -> dict:
    """Snapshot current registered models and byte diffs from sealed packaging."""
    repo, destination = Path(repo), Path(destination)
    if destination.resolve().is_relative_to(repo.resolve()):
        raise ValueError("Lineage export destination must be outside native repo")
    baseline_store = repo / BASELINE
    baseline_records = _verified(baseline_store)
    before = {}
    baseline = None
    if (baseline_store / "manifest.json").exists():
        baseline = _json(baseline_store / "manifest.json")["baseline"]
        matches = [(ref, record) for ref, record in baseline_records if ref["id"] == baseline["id"]]
        if len(matches) != 1:
            raise ValueError("Mechanical baseline record is missing")
        ref, record = matches[0]
        _copy_record(baseline_store, destination, ref, record)
        before = {entry["path"]: _read(destination / "blobs" / entry["sha256"]) for entry in record["files"]}
    elif baseline_store.exists() or ((repo / "modeling-input/manifest.json").exists()
                                    and _json(repo / "modeling-input/manifest.json").get("mechanical_model_baseline")):
        raise ValueError("Mechanical baseline manifest is missing")
    after, registry = _model_files(repo)
    current = _seal(destination, "process-model-snapshot", {"coverage": "captured", "registry": registry}, after)
    changes, diff = [], []
    if baseline:
        for name in sorted(before.keys() | after.keys()):
            old, new = before.get(name), after.get(name)
            if old == new:
                continue
            changes.append({"path": name, "before_sha256": _sha(old) if old is not None else None,
                            "after_sha256": _sha(new) if new is not None else None})
            diff.extend(difflib.unified_diff((old or b"").decode(errors="replace").splitlines(True),
                                           (new or b"").decode(errors="replace").splitlines(True),
                                           fromfile=f"mechanical/{name}", tofile=f"process/{name}"))
    manifest = {"schema": SCHEMA, "baseline": baseline, "current": current,
                "coverage": "baseline_and_process" if baseline else "current_only_baseline_unavailable",
                "history_complete": bool(baseline), "changes": changes, "diff": "process-model.diff" if baseline else None,
                "limitations": [] if baseline else ["No contemporaneous mechanical packaging baseline exists; historical process edits cannot be reconstructed faithfully."]}
    if baseline:
        _put(destination / "process-model.diff", "".join(diff).encode())
    _put(destination / "manifest.json", canonical_json(manifest))
    return manifest

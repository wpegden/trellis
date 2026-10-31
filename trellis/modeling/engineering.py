"""Paper-first engineering states within the preparation Driver.

These receipts confer engineering readiness only. Provider delivery needs a
closed file view; the existing formal-preparation wrapper is intentionally not
an eligible transport. The workflow can be exercised with local fixture agents.
"""
from __future__ import annotations

import copy
from contextlib import contextmanager
import hashlib
from pathlib import Path
import shutil
import uuid

from scripts.extract_pv_model import copy_source_inputs
from trellis.modeling import history, performance
from trellis.modeling.observations import read_json, write_json
from trellis.paper_implementation import canonical_json, evidence_path, preparation_paper_digest, sha256, PAPER_UPLOAD_LIMIT, validate_example_inputs

STORE = Path(".trellis/modeling/engineering")
FRAGMENTS = Path(__file__).resolve().parents[1] / "prompt_fragments/engineering"
ADAPTATION_NOTE = "work/crate/ENGINEERING_ADAPTATION.md"
BASELINE_REPORT = STORE / "baseline-assessment.md"
_STAGED_BLOB_BATCH_BYTES = 64 * 1024 * 1024
CHECK_COMMANDS = [
    {"name": "cargo-check", "argv": ["cargo", "check", "--locked", "--offline", "--all-targets"],
     "cwd": "implementation", "kind": "build"},
    {"name": "cargo-test", "argv": ["cargo", "test", "--locked", "--offline", "--all-targets"],
     "cwd": "implementation", "kind": "run"}]


def read_input(path: Path, *, notes: bool = False) -> bytes:
    """Local contracts retain their nonempty UTF-8 input policy.

    Notes share the viewer's UTF-8, NUL-free text and upload-size limits.
    Input paths may be links; the copied evidence is an ordinary frozen file.
    """
    if notes:
        with path.open("rb") as stream:
            raw = stream.read(PAPER_UPLOAD_LIMIT + 1)
        if len(raw) > PAPER_UPLOAD_LIMIT or b"\0" in raw:
            raise ValueError("Implementation notes require NUL-free UTF-8 text of at most 20 MiB")
    else:
        raw = path.read_bytes()
    if not raw.decode("utf-8-sig" if notes else "utf-8").strip():
        raise ValueError("Engineering inputs require nonempty UTF-8 text")
    return raw


def digest(value) -> str:
    return hashlib.sha256(canonical_json(value)).hexdigest()


def initialize(root: Path, contract: Path, implementation_notes: Path | None = None) -> None:
    """Opt in immediately after creating a fresh, unseeded paper preparation."""
    state = read_json(root / ".trellis/modeling/state.json")
    config = read_json(root / "config.json")
    if state["sequence"] or state["stage"] != "setup" or state["audits"]:
        raise ValueError("Engineering requires fresh paper intake")
    paper = config.get("paper_input", {})
    if paper.get("route") != "rust_aeneas" or paper.get("modeling_choices"):
        raise ValueError("Engineering intake requires an original-paper-only Rust preparation")
    validate_example_inputs(root / 'paper', paper)
    raw = read_input(contract)
    history._put(root / STORE / "contract.md", raw)
    state["engineering"] = {"input": {"paper": preparation_paper_digest(root),
                                      "contract": sha256(root / STORE / "contract.md")},
                            "candidates": {}, "checks": {}}
    if implementation_notes is not None:
        history._put(root / STORE / "implementation-notes.md", read_input(implementation_notes, notes=True))
        state["engineering"]["input"]["implementation_notes"] = sha256(root / STORE / "implementation-notes.md")
    state.update(stage="engineering", next={"role": "reviewer", "purpose": "source_reading"})
    write_json(root / ".trellis/modeling/state.json", state)
    with (root / ".gitignore").open("a") as stream:
        stream.write("!/engineering/\n!/.trellis/modeling/engineering/\n/.trellis/modeling/engineering/*\n"
                     "!/.trellis/modeling/engineering/contract.md\n"
                     "!/.trellis/modeling/engineering/implementation-notes.md\n")


def input_identity(root: Path, state: dict) -> dict:
    actual = {"paper": preparation_paper_digest(root), "contract": hashlib.sha256(history._read(root / STORE / "contract.md")).hexdigest()}
    notes = root / STORE / "implementation-notes.md"
    if notes.exists() or notes.is_symlink():
        actual["implementation_notes"] = hashlib.sha256(history._read(notes)).hexdigest()
    if actual != state["engineering"]["input"]:
        raise ValueError("Engineering paper, contract or implementation notes changed; fresh intake required")
    return actual


def publish_inputs(root: Path, identity: dict, destination: Path) -> None:
    for key, name in (("contract", "contract.md"), ("implementation_notes", "implementation-notes.md")):
        if key not in identity:
            continue
        raw = history._read(root / STORE / name)
        if hashlib.sha256(raw).hexdigest() != identity[key]:
            raise ValueError("Engineering input changed before publication: " + name)
        history._put(destination / name, raw)
        if hashlib.sha256(history._read(destination / name)).hexdigest() != identity[key]:
            raise ValueError("Engineering input changed during publication: " + name)


def input_guard(root: Path) -> str:
    return digest({name: sha256(root / name) for name in
                   ("config.json", "work/extraction.json", "trellis.config.json") if (root / name).exists()})


def has_references(paper: dict) -> bool:
    return bool(paper.get("reference_files") or any(name == "paper/TRELLIS_REFERENCES.json" or name.startswith("paper/references/")
                                                   for name in paper.get("paper_files", [])))


def _text(value, name: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{name} requires nonempty text")
    return value


def read_plan(path: Path) -> dict:
    plan = read_json(path)
    fields = {"requirements", "interpretations", "unspecified", "departures"}
    if set(plan) != fields:
        raise ValueError("Plan requires requirements, interpretations, unspecified and departures")
    for field, items in plan.items():
        if not isinstance(items, list) or (field == "requirements" and not items):
            raise ValueError(f"{field} must be a list; requirements must be nonempty")
        for item in items:
            if not isinstance(item, dict) or set(item) != {"source", "description", "consequences"}:
                raise ValueError(f"{field} entries require source, description and consequences")
            for name, value in item.items():
                _text(value, name)
    return plan


def check(root: Path, state: dict, *, operation: Path | None = None, identity: dict | None = None) -> dict:
    """Use the existing bounded measurement runner for locked Cargo checks."""
    input_identity(root, state)
    harness = root / "benchmarks/harness"
    harness.mkdir(parents=True, exist_ok=True)
    if not any(harness.iterdir()):
        (harness / "README.md").write_text("Engineering check harness.\n")
    key = performance.current_basis(root)
    previous = state["engineering"].setdefault("checks", {}).get(key)
    if previous and operation is None:
        try:
            return validate_check(root, previous)
        except ValueError:
            pass
    record = performance.run_measurement(root, state, {
        "action": "measure", "reason": "Check and test the candidate with its committed dependency lock.",
        "commands": CHECK_COMMANDS}, **({"operation": operation, "identity": identity} if operation is not None else {}))
    state["engineering"]["checks"][key] = record["id"]
    return record


def _check_passed(record: dict) -> bool:
    return (record.get("fresh_after_measurement") and not record.get("evidence_error")
            and [{key: item.get(key) for key in expected} for item, expected in zip(record["commands"], CHECK_COMMANDS)] == CHECK_COMMANDS
            and len(record["commands"]) == len(CHECK_COMMANDS)
            and all(item["exit_code"] == 0 and not item["timeout"] and not item.get("output_limit_exceeded")
                    and not item.get("execution_error")
                    for item in record["commands"]))


def validate_check(root: Path, run_id: str, expected: str | None = None) -> dict:
    """Bind source checks to their original inputs, independently of later benchmarks."""
    record = performance.recorded_evidence(root, run_id)
    if expected is not None and record["record_sha256"] != expected:
        raise ValueError("Approved Cargo receipt changed")
    if record["basis"] != performance.current_basis(root) or not _check_passed(record):
        raise ValueError("Cargo evidence requires successful locked checks of the current source and tools")
    return record


def current_approval(root: Path, state: dict) -> None:
    input_identity(root, state)
    engineering = state["engineering"]
    if read_plan(root / "engineering/plan.json") != engineering.get("plan"):
        raise ValueError("Current plan differs from the independently reviewed plan")
    if engineering.get("plan_approval", {}).get("plan") != digest(engineering.get("plan")):
        raise ValueError("Engineering plan requires independent approval")
    audit = state.get("audits", {}).get("engineering", {})
    if (audit.get("decision") != "accept" or audit.get("basis") != performance.current_basis(root)
            or audit.get("plan") != digest(engineering["plan"])):
        raise ValueError("Current code requires independent correctness/readiness review")
    if not audit.get("checks_sha256"):
        raise ValueError("Code review must bind its Cargo receipt")
    validate_check(root, audit["checks"], audit["checks_sha256"])


def reopen(root: Path, state: dict, reason: str) -> None:
    state.get("audits", {}).pop("engineering", None)
    state.update(status="active", feedback=reason, next={"role": "planner", "purpose": "plan", "instruction": reason})


def apply(root: Path, state: dict, task: dict, result: dict) -> None:
    """Interpret existing worker/audit envelopes in their active engineering task."""
    input_identity(root, state)
    engineering = state["engineering"]
    purpose = task["purpose"]
    state["feedback"] = ""
    engineering.pop("retry_feedback", None)
    if purpose in {"source_reading", "plan", "implementation"}:
        _text(result.get("summary"), "summary")
        if set(result) != {"summary"}:
            raise ValueError("Engineering worker response uses the summary envelope")
    if purpose == "source_reading":
        reading = read_plan(root / "engineering/reading.json")
        engineering["reading"] = reading
        state["next"] = {"role": "planner", "purpose": "plan"}
    elif purpose == "plan":
        if "reading" not in engineering:
            raise ValueError("Independent source reading must precede candidate review")
        engineering["plan"] = read_plan(root / "engineering/plan.json")
        engineering.pop("plan_approval", None)
        state["next"] = {"role": "reviewer", "purpose": "plan_review"}
    elif purpose in {"plan_review", "code_review"}:
        if read_plan(root / "engineering/plan.json") != engineering["plan"]:
            raise ValueError("Review requires the current proposed plan")
        if set(result) != {"decision", "reason"} or result.get("decision") not in {"accept", "revise", "review"}:
            raise ValueError("Engineering review uses the audit decision/reason envelope")
        _text(result["reason"], "reason")
        if result["decision"] != "accept":
            if purpose == "code_review" and result["decision"] == "revise":
                state["next"] = {"role": "worker", "purpose": "implementation", "instruction": result["reason"]}
            else:
                reopen(root, state, result["reason"])
            return
        if purpose == "plan_review":
            engineering["plan_approval"] = {**result, "plan": digest(engineering["plan"]),
                                           "reading": digest(engineering["reading"]),
                                           "input": input_identity(root, state)}
            state["next"] = {"role": "worker", "purpose": "implementation"}
        else:
            if engineering.get("plan_approval", {}).get("plan") != digest(engineering["plan"]):
                raise ValueError("Code review requires an approved plan")
            record = check(root, state)
            if not _check_passed(record):
                raise ValueError("Code review requires successful current Cargo check/test --locked")
            state["audits"]["engineering"] = {**result, "basis": performance.current_basis(root),
                                                "plan": digest(engineering["plan"]), "checks": record["id"],
                                                "checks_sha256": record["record_sha256"]}
            from trellis.modeling.performance_loop import begin
            begin(root, state)
    elif purpose == "implementation":
        if read_plan(root / "engineering/plan.json") != engineering["plan"]:
            raise ValueError("New departures require plan review before adoption")
        state["observation"] = check(root, state)
        state["next"] = {"role": "reviewer", "purpose": "code_review"}
    else:
        raise ValueError("Unknown engineering task")


def retain(root: Path, state: dict, record: dict) -> None:
    """Keep each measured, reviewed source generation available for selection."""
    current_approval(root, state)
    files = history._tree(root / "work/crate", "source", source=True)
    files.update({"harness/" + name: history._read(root / "benchmarks/harness" / name)
                  for name in performance._inventory(root / "benchmarks/harness")})
    metadata = {"input": input_identity(root, state), "basis": performance.current_basis(root),
                "reading": state["engineering"]["reading"],
                "plan": state["engineering"]["plan"], "plan_approval": state["engineering"]["plan_approval"],
                "audit": state["audits"]["engineering"], "measurement": record["id"],
                "modes": {name: (root / ("work/crate" if name.startswith("source/") else "benchmarks/harness")
                                 / name.split("/", 1)[1]).stat().st_mode & 0o777 for name in files}}
    reference = history._seal(root / STORE / "candidates", "engineering-candidate", metadata, files)
    state["engineering"]["candidates"][record["id"]] = reference["id"]


def candidate(root: Path, identity: str) -> dict:
    for reference, record in history._verified(root / STORE / "candidates"):
        if reference["id"] == identity:
            return record
    raise ValueError("Unknown retained engineering candidate")


def _restore(root: Path, record: dict) -> None:
    store = root / STORE / "candidates"
    for prefix, target in (("source", root / "work/crate"), ("harness", root / "benchmarks/harness")):
        if target.exists():
            shutil.rmtree(target)
        target.mkdir(parents=True)
        for entry in record["files"]:
            if entry["path"].startswith(prefix + "/"):
                path = target / entry["path"].split("/", 1)[1]
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(history._read(store / "blobs" / entry["sha256"]))
                path.chmod(record["metadata"]["modes"][entry["path"]])


@contextmanager
def select(root: Path, state: dict, evidence: list[str], *, settle=None):
    """Keep rollback ownership until selection and acceptance both complete."""
    if not isinstance(evidence, list) or not evidence:
        raise ValueError("Select measured evidence for the engineering baseline")
    identity = state["engineering"]["candidates"].get(evidence[0])
    if not identity:
        raise ValueError("Selection requires this run's reviewed measured candidate")
    selected = candidate(root, identity)
    if selected["metadata"]["input"] != input_identity(root, state):
        raise ValueError("Selected candidate has different original inputs")
    backup = root / STORE / "selection-backup"
    if backup.exists():
        raise ValueError("An interrupted candidate selection requires recovery")
    from trellis.modeling.performance_loop import ACCEPTED, begin
    controls = [root / "engineering/plan.json", root / ACCEPTED,
                root / STORE / "baseline.json", root / BASELINE_REPORT]
    saved = {path: (history._read(path), path.stat().st_mode & 0o777) if path.exists() else None
             for path in controls}
    old = copy.deepcopy(state)
    backup.mkdir(parents=True)
    write_json(backup / "state.json", old)
    for index, (path, value) in enumerate(saved.items()):
        if value is not None:
            (backup / f"control-{index}").write_bytes(value[0])
    write_json(backup / "controls.json", [{"path": str(path.relative_to(root)),
               "backup": f"control-{index}" if value else None, "mode": value[1] if value else None}
               for index, (path, value) in enumerate(saved.items())])
    moved = []
    try:
        # Renames retain every file and mode, including cache-shaped harness
        # names and source build products. Failed recovery keeps this backup.
        for name, target in (("source", root / "work/crate"), ("harness", root / "benchmarks/harness")):
            target.rename(backup / name)
            moved.append((name, target))
        _restore(root, selected)
        metadata = selected["metadata"]
        if performance.current_basis(root) != metadata["basis"]:
            raise ValueError("Selected candidate tools or configuration changed")
        performance.validate_evidence(root, state, evidence)
        state["engineering"].update(plan=metadata["plan"], plan_approval=metadata["plan_approval"])
        write_json(root / "engineering/plan.json", metadata["plan"])
        state["audits"]["engineering"] = metadata["audit"]
        current_approval(root, state)
        if settle is None:
            begin(root, state)
        state["engineering"]["selected"] = identity
        yield
        if settle is not None and settle() != 'committed':
            raise RuntimeError('Native selection has no settled consumption')
    except BaseException as original:
        if settle is not None:
            try:
                settlement = settle()
            except BaseException:
                original.add_note('Native selection settlement unavailable; backup retained')
                raise original
            if settlement == 'committed':
                shutil.rmtree(backup)
                raise
            if settlement != 'uncommitted':
                original.add_note('Native selection settlement uncertain; backup retained')
                raise
        for name, target in moved:
            if target.exists():
                shutil.rmtree(target)
            (backup / name).rename(target)
        for path, value in saved.items():
            if value is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(value[0])
                path.chmod(value[1])
        state.clear()
        state.update(old)
        shutil.rmtree(backup)
        raise
    else:
        shutil.rmtree(backup)


def freeze(root: Path, state: dict, *, transition: bool = True) -> None:
    from trellis.modeling.performance_loop import validate_acceptance
    approval, records = validate_acceptance(root, state)
    checks = state["audits"]["engineering"]["checks"]
    report = history._read(evidence_path(root, approval["report"]))
    if hashlib.sha256(report).hexdigest() != approval["report_sha256"]:
        raise ValueError("Accepted engineering assessment changed")
    history._put(root / BASELINE_REPORT, report)
    baseline = {"candidate": state["engineering"]["selected"], "performance": approval,
                "assessment": {"path": BASELINE_REPORT.as_posix(), "sha256": approval["report_sha256"]},
                "checks": {checks: state["audits"]["engineering"]["checks_sha256"]},
                "records": {record["id"]: record["record_sha256"] for record in records}}
    history._put(root / STORE / "baseline.json", canonical_json(baseline))
    state["engineering"]["baseline"] = baseline
    if transition:
        state.update(status="engineering_ready", next={})


def handoff(root: Path, state: dict, *, native_ready=None) -> None:
    from trellis.modeling.performance_loop import validate_acceptance
    if state["stage"] != "engineering" or state["status"] != "engineering_ready":
        raise ValueError("Handoff requires a frozen engineering baseline")
    validate_acceptance(root, state)
    baseline = read_json(root / STORE / "baseline.json")
    if baseline != state["engineering"]["baseline"]:
        raise ValueError("Engineering baseline changed")
    candidate(root, baseline["candidate"])
    _baseline_records(root, baseline)
    # Preserve both sides if publication or the subsequent checkpoint fails.
    attempt = root / STORE / "handoffs" / uuid.uuid4().hex
    write_json(attempt / "before-state.json", state)
    state["engineering"]["handoff_attempt"] = attempt.relative_to(root).as_posix()
    staged, previous = attempt / "source", attempt / "previous-upstream"
    copy_source_inputs(root / "work/crate", staged)
    if native_ready is not None:
        record = dict(ready_state_sha256=native_ready['state_sha256'],
            baseline_sha256=digest(native_ready['state']['engineering']['baseline']),
            handoff_attempt=attempt.relative_to(root).as_posix(),
            before_state_sha256=sha256(attempt / 'before-state.json'),
            downstream_source_sha256=_native_handoff_source(staged),
            previous_source_sha256=native_handoff_source(root))
        history._put(attempt / 'native-transfer.json', canonical_json(record))
        state['engineering']['native_handoff_record'] = native_record(root, record)
    (root / "upstream").rename(previous)
    try:
        staged.rename(root / "upstream")
    except BaseException:
        previous.rename(root / "upstream")
        raise
    _handoff_state(state, attempt.relative_to(root).as_posix())
    if native_ready is not None:
        history._put(attempt / 'prepared-state.json', canonical_json(state))


def _handoff_state(state, attempt):
    # Formal preparation obtains its own approvals and measurements.
    state['engineering']['handoff_attempt'] = attempt
    state["engineering"]["handed_off"] = True
    state["engineering"]["handoff_sequence"] = state["sequence"]
    state["engineering"]["performance"] = state.pop("performance")
    state.update(stage="setup", status="active", audits={}, observation={},
                 next={"role": "worker", "instruction":
                       "Adapt the frozen engineering baseline through the existing setup and source review stages. "
                       f"Record reasons for changes in {ADAPTATION_NOTE}; retain its regression harness and measure the changes."})


def pending_handoff(root: Path, state: dict) -> bool:
    """An explicit, checkpointed phase transition can continue without recovery."""
    eng = state.get("engineering", {})
    if (state.get("status") != "active" or state.get("stage") != "setup"
            or not eng.get("handed_off") or eng.get("handoff_sequence") != state.get("sequence")):
        return False
    if read_json(root / 'config.json').get('engineering_protocol') == 'native-v1':
        if not native_handoff_complete(root, native_load(root)):
            return False
    import subprocess
    result = subprocess.run(["git", "show", "HEAD:.trellis/modeling/state.json"], cwd=root,
                            capture_output=True, check=True)
    import json
    return json.loads(result.stdout) == state


def checkpoint(root: Path, state: dict) -> None:
    """Retain the baseline's existing content-addressed receipts in checkpoints."""
    from trellis.modeling.driver import commit_checkpoint, runtime_root
    from trellis.modeling.performance_loop import ACCEPTED
    import subprocess
    baseline = state.get("engineering", {}).get("baseline")
    if baseline:
        candidate(root, baseline["candidate"])
        _baseline_records(root, baseline)
        paths = [STORE / "baseline.json", BASELINE_REPORT, ACCEPTED]
        paths.extend(Path("benchmarks/harness") / name for name in performance._inventory(root / "benchmarks/harness"))
        paths.append(Path(baseline["performance"]["report"]))
        history_root = STORE / "candidates"
        paths.append(history_root / "catalog.json")
        for reference, record in history._verified(root / history_root):
            paths.append(history_root / reference["record"])
            paths.extend(history_root / "blobs" / entry["sha256"] for entry in record["files"])
        for run in {**baseline["records"], **baseline["checks"]}:
            receipt = performance.recorded_evidence(root, run)
            paths.extend(performance.STORE / "runs" / run / name
                         for name in ["record.json", "record.sha256", *receipt["evidence_files"]])
        for path in paths:
            evidence_path(root, path.as_posix())
        subprocess.run(["git", "add", "--force", "--", *[p.as_posix() for p in paths]], cwd=root, check=True)
    commit_checkpoint({"root": str(runtime_root(root)), "event_count": state["sequence"],
                       "metadata": {"repo_path": str(root), "config_path": str(root / "trellis.config.json")},
                       "checkpoint": {"cycle": state["sequence"], "phase": "modeling"}, "state": state})


def _baseline_records(root: Path, baseline: dict) -> list[dict]:
    if (baseline["assessment"]["path"] != BASELINE_REPORT.as_posix()
            or sha256(evidence_path(root, BASELINE_REPORT.as_posix())) != baseline["assessment"]["sha256"]
            or baseline["assessment"]["sha256"] != baseline["performance"]["report_sha256"]):
        raise ValueError("Baseline engineering assessment changed")
    records = []
    for run, expected in {**baseline["records"], **baseline["checks"]}.items():
        path = evidence_path(root, (performance.STORE / "runs" / run / "record.json").as_posix())
        if sha256(path) != expected:
            raise ValueError("Baseline measurement receipt changed")
        record = performance.recorded_evidence(root, run)
        records.append(record)
    return records


def validate_handoff(root: Path, state: dict) -> None:
    """Additional baseline accounting; all existing formal gates still apply."""
    if not state.get("engineering"):
        return
    if not state["engineering"].get("handed_off"):
        raise ValueError("Engineering readiness requires explicit formal handoff")
    baseline = read_json(root / STORE / "baseline.json")
    if baseline != state["engineering"].get("baseline"):
        raise ValueError("Engineering baseline changed")
    original = candidate(root, baseline["candidate"])
    _baseline_records(root, baseline)
    _text(evidence_path(root, ADAPTATION_NOTE).read_text(), "Adaptation rationale")
    for entry in original["files"]:
        if entry["path"].startswith("harness/"):
            path = evidence_path(root / "benchmarks", entry["path"])
            if (sha256(path) != entry["sha256"]
                    or path.stat().st_mode & 0o777 != original["metadata"]["modes"][entry["path"]]):
                raise ValueError("Adaptation must retain the baseline regression/measurement harness")
    if not _check_passed(check(root, state)):
        raise ValueError("Adaptation requires current locked Cargo regression checks")
    from trellis.modeling.performance_loop import validate_acceptance
    _, records = validate_acceptance(root, state)
    baseline_commands = [read_json(root / performance.STORE / "runs" / run / "request.json")["commands"]
                         for run in baseline["records"]]
    current_commands = [read_json(root / performance.STORE / "runs" / record["id"] / "request.json")["commands"]
                        for record in records]
    if any(commands not in current_commands for commands in baseline_commands):
        raise ValueError("Adaptation must remeasure the baseline workloads")


def export_evidence(root: Path, state: dict, destination: Path) -> list[str]:
    if not state.get("engineering"):
        return []
    validate_handoff(root, state)
    identity = input_identity(root, state)
    base = destination / "paper-implementation/engineering"
    publish_inputs(root, identity, base)
    baseline = state["engineering"]["baseline"]
    record = candidate(root, baseline["candidate"])
    reference = {"id": baseline["candidate"], "record": f"records/{baseline['candidate']}.json"}
    history._copy_record(root / STORE / "candidates", base / "candidate", reference, record)
    for source, name in ((root / STORE / "baseline.json", "baseline.json"),
                         (root / BASELINE_REPORT, "baseline-assessment.md"),
                         (root / ADAPTATION_NOTE, "adaptation.md")):
        history._put(base / name, history._read(source))
    for record in _baseline_records(root, baseline):
        source = root / performance.STORE / "runs" / record["id"]
        for name in ["record.json", *record["evidence_files"]]:
            history._put(base / "measurements" / record["id"] / name, history._read(source / name))
    if read_json(root / 'config.json').get('engineering_protocol') == 'native-v1':
        _copy_outputs(root, implementation_outputs(root, native_load(root)['state']['engineering']), base)
    return sorted(path.relative_to(destination).as_posix() for path in base.rglob("*") if path.is_file())


def copy_harness(source: Path, destination: Path) -> None:
    files = performance._inventory(source)
    modes = performance._modes(source, files)
    destination.mkdir(parents=True)
    for name in files:
        path = destination / name
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(evidence_path(source, name), path)
    if performance._inventory(destination) != files or performance._modes(destination, files) != modes:
        raise ValueError("Harness changed during copying")


def _output_entries(record: dict) -> list[dict]:
    """Select auxiliary bytes from the original returned-view seal."""
    metadata = record['metadata']
    inventory = {name: value for name, value in metadata['inventory'].items()
                 if name == 'outputs' or name.startswith('outputs/')}
    entries = {entry['path'][5:]: entry for entry in record['files']
               if entry['path'] == 'view/outputs' or entry['path'].startswith('view/outputs/')}
    if set(inventory) != set(entries) or 'outputs' in inventory:
        raise ValueError('Implementation output inventory differs')
    selected = []
    for name, entry in sorted(entries.items()):
        history._relative(name)
        mode = metadata['modes'][name]
        if (set(Path(name).parts) & {'.git', '.codex', '.agents', '.trellis'}
                or Path(name).suffix in {'.lean', '.llbc', '.olean'}
                or inventory[name] != entry['sha256'] or type(mode) is not int or not 0 <= mode <= 0o777
                or type(entry['size']) is not int or entry['size'] < 0):
            raise ValueError('Implementation output is outside its sealed contract')
        selected.append(dict(path=name.removeprefix('outputs/'), sha256=entry['sha256'],
                             size=entry['size'], mode=mode))
    return selected


def _outputs_from_checked(root: Path, checked: dict, host: dict) -> dict | None:
    request = host['request']
    if 'outputs' not in request['engineering']['output_paths']:
        return None  # Historical requests do not acquire a new output contract.
    projection = STORE / 'submissions' / str(checked['request_id']) / str(checked['turn']) / checked['candidate_sha256']
    raw = history._read(root / projection / 'return.json')
    if hashlib.sha256(raw).hexdigest() != checked['candidate_sha256']:
        raise ValueError('Implementation output seal changed')
    import json
    record = json.loads(raw)
    if (record['schema'] != history.SCHEMA or record['kind'] != 'engineering-return'
            or record['metadata']['request'] != request or record['metadata']['turn'] != checked['turn']
            or host['sealed_candidate'] != checked['candidate_sha256']):
        raise ValueError('Implementation output seal binding differs')
    files = _output_entries(record)
    directory = root / projection / 'outputs'
    actual = performance._inventory(directory) if directory.exists() else {}
    if actual != {item['path']: item['sha256'] for item in files}:
        raise ValueError('Implementation output projection differs')
    for item in files:
        if len(history._read(directory / item['path'])) != item['size']:
            raise ValueError('Implementation output length differs')
    return dict(checked_sha256=digest(checked), seal_sha256=checked['candidate_sha256'],
                candidate_sha256=digest(checked['outcome']['candidate']), files=files,
                projection=projection.as_posix())


def implementation_outputs(root: Path, native: dict, value: dict | None = None) -> dict | None:
    value = native.get('candidate') if value is None else value
    if value is None:
        return None
    for _, checked, host in _native_consumptions(root, native):
        if (host['request']['engineering']['purpose'] == 'implementation'
                and checked['outcome'].get('outcome') == 'implementation'
                and checked['outcome']['candidate'] == value):
            return _outputs_from_checked(root, checked, host)
    raise ValueError('Implementation outputs have no checked candidate')


def _copy_outputs(root: Path, outputs: dict | None, destination: Path) -> None:
    if outputs is None:
        return
    public = {key: value for key, value in outputs.items() if key != 'projection'}
    history._put(destination / 'implementation-outputs.json', canonical_json(public))
    for item in outputs['files']:
        raw = history._read(root / outputs['projection'] / 'outputs' / item['path'])
        if hashlib.sha256(raw).hexdigest() != item['sha256'] or len(raw) != item['size']:
            raise ValueError('Implementation output changed during delivery')
        target = destination / 'outputs' / item['path']
        history._put(target, raw)
        import os
        from trellis.agents.engineering_exec import open_regular
        with open_regular(target) as stream:
            if os.fstat(stream.fileno()).st_nlink != 1:
                raise ValueError('Implementation output delivery is aliased')
            os.fchmod(stream.fileno(), item['mode'])
            os.fsync(stream.fileno())


def performance_budget(root: Path, native: dict) -> dict:
    """Derive optimization slots and reset provenance from consumed history only."""
    used=0;baseline=None;measurements={};by_reference={}
    current=digest(native['candidate']) if native.get('candidate') else None
    review=native.get('current_reviewed_candidate_sha256')
    for position,(_,checked,host) in enumerate(reversed(list(_native_consumptions(root,native)))):
        outcome=checked['outcome'];kind=outcome['outcome']
        if host['request']['engineering']['purpose']!='performance':continue
        if kind=='measure':
            value=outcome['measurement'];reference=value['record_sha256']
            if native.get('measurements',{}).get(reference)!=value:
                raise RuntimeError('Budget measurement differs from native history')
            retained,record=_native_measurement(root,value)
            reviewed=native['reviewed'][value['reviewed_candidate_sha256']]
            candidate=reviewed['candidate']
            if digest(reviewed)!=value['reviewed_candidate_sha256'] or digest(candidate)!=value['candidate_sha256']:
                raise RuntimeError('Budget measurement has no exact reviewed implementation')
            run=record['id']
            if run in measurements:raise RuntimeError('Budget measurement run identity is ambiguous')
            row=dict(measurement_sha256=reference,position=position,
                group=[value[key] for key in ('candidate_sha256','reviewed_candidate_sha256','harness_sha256')]+[record['basis']],
                implementation=[candidate[key] for key in ('source_sha256','build_sha256','lock_sha256')],
                eligible=bool(record.get('fresh_after_measurement') and not record.get('evidence_error')
                    and value['exit_codes'] and all(code==0 for code in value['exit_codes'])),
                successful_run=record['successful_run'],
                current=value['candidate_sha256']==current and value['reviewed_candidate_sha256']==review)
            measurements[run]=row;by_reference[reference]=run
            if baseline is None and row['eligible'] and row['successful_run']:
                baseline=dict(evidence=[run],metric=None,group=row['group'],implementation=row['implementation'],after_position=position,fixed=False)
        elif kind in {'improve','propose'}:used+=1
        elif kind=='reset_budget':
            runs=[by_reference[reference] for reference in outcome['current_measurement_sha256']]
            if not runs or len(set(runs))!=len(runs) or len({tuple(measurements[run]['group']) for run in runs})!=1:
                raise RuntimeError('Consumed performance reset evidence differs')
            row=measurements[runs[0]]
            baseline=dict(evidence=sorted(runs),metric=outcome['metric'],group=row['group'],implementation=row['implementation'],after_position=position,fixed=True)
            used=0
    previous=native.get('last_consumption')
    return dict(schema='trellis-native-performance-budget/v1',predecessor_sha256=digest(previous) if previous else None,
        used=used,remaining=max(0,4-used),hard_limit=4,guidance=3,baseline=baseline,measurements=measurements,
        current_candidate_sha256=current,current_reviewed_candidate_sha256=review)


def _measurement_views(root: Path, state: dict, purpose: str, native_context: bool):
    """One positive measurement selection for delivery and allocation admission."""
    if not (purpose in {'code_review', 'performance'} or native_context and purpose == 'implementation'):
        return
    run_ids = list(state['engineering'].get('checks', {}).values())
    if purpose == 'performance' or native_context and purpose == 'implementation':
        run_ids += state.get('performance', {}).get('runs', [])
    for run in dict.fromkeys(run_ids):
        record = performance.recorded_evidence(root, run)
        public = {key: copy.deepcopy(record[key]) for key in ('id', 'basis', 'harness_sha256', 'record_sha256',
            'machine', 'commands', 'successful_run', 'reason', 'fresh_after_measurement', 'limits')}
        if record.get('evidence_error'):
            public['evidence_error'] = record['evidence_error']
        public['current_source'] = record['basis'] == performance.current_basis(root)
        public['current_harness'] = record['harness_sha256'] == performance.harness_fingerprint(root)
        source = root / performance.STORE / 'runs' / run
        public['source_files'] = read_json(source / 'basis.json')['implementation_files']
        public['candidate'] = state['engineering'].get('candidates', {}).get(run)
        visible = {name for name in record['evidence_files'] if name.startswith('artifacts/output/') or name == 'generated-final.json'}
        for command in public['commands']:
            command['executable'] = {key: value for key, value in command.get('executable', {}).items() if key == 'sha256'}
            for field in ('stdout', 'stderr', 'generated_inputs', 'retained_entrypoint'):
                if command.get(field):
                    visible.add(command[field])
        public['evidence_files'] = {name: record['evidence_files'][name] for name in sorted(visible)}
        yield run, record, public


def delivery_allocation(root: Path, state: dict, native: dict) -> dict:
    """Price the same copied inputs, plus predictable private copy expansion."""
    import json
    from scripts.extract_pv_model import source_input_manifest
    purpose = state['next']['purpose']
    identity = input_identity(root, state)
    files = {}
    def selected(name, path):
        files[name] = evidence_path(path.parent, path.name).stat().st_size
    def generated(name, value):
        files[name] = len((json.dumps(value, indent=2, ensure_ascii=False) + '\n').encode())
        files[name + '.lock'] = 0
    for name in read_json(root / 'config.json')['paper_input']['paper_files']:
        selected(name, root / name)
    for key, name in (('contract', 'contract.md'), ('implementation_notes', 'implementation-notes.md')):
        if key in identity:
            selected(name, root / STORE / name)
    source, harness = {}, {}
    if purpose in {'implementation', 'code_review', 'performance'}:
        source = {row['relative_path']: row['byte_length'] for row in source_input_manifest(root / 'work/crate')['files']}
        files.update(('source/' + name, size) for name, size in source.items())
        if (root / 'benchmarks/harness').exists():
            harness = {name: evidence_path(root / 'benchmarks/harness', name).stat().st_size
                       for name in performance._inventory(root / 'benchmarks/harness')}
            files.update(('benchmarks/harness/' + name, size) for name, size in harness.items())
    for run, _, public in _measurement_views(root, state, purpose, True):
        directory = root / performance.STORE / 'runs' / run
        for name in public['evidence_files']:
            selected('measurements/' + run + '/' + name, directory / name)
        generated('measurements/' + run + '/record.json', public)
    if purpose in {'code_review', 'performance'}:
        outputs = implementation_outputs(root, native['engineering'])
        if outputs is not None:
            files['implementation-outputs.json'] = len(canonical_json({key: value for key, value in outputs.items() if key != 'projection'}))
            files.update(('outputs/' + row['path'], row['size']) for row in outputs['files'])
    index, records, _ = _native_public_history(root, native['engineering'], purpose)
    files['history/index.json'] = len(canonical_json(index))
    files.update(('history/records/' + key + '.json', len(canonical_json(value))) for key, value in records.items())
    if purpose=='performance':files['performance-budget.json']=len(canonical_json(performance_budget(root,native['engineering'])))
    if purpose != 'source_reading':
        for key in ('reading', 'plan', 'plan_approval'):
            if key in state['engineering']:
                generated(key + '.json', state['engineering'][key])
        if purpose == 'plan' and state['engineering'].get('proposal'):
            generated('proposal.json', state['engineering']['proposal'])
    def allocation(entries):
        parents = {parent.as_posix() for name in entries for parent in Path(name).parents if parent != Path('.')}
        return sum(((size + 4095) // 4096) * 4096 for size in entries.values()) + 4096 * len(parents), len(entries) + len(parents)
    size, count = allocation(files)
    source_size, source_count = allocation(source)
    harness_size, harness_count = allocation(harness)
    turns = native['worker_submission_policy']['max_submissions']
    # Initial view plus each returned-view seal. Imports, Cargo snapshots,
    # submitted-source projections and Git objects retain independent bytes.
    copies = 1 + turns
    writable_copies = 4 * turns if purpose == 'implementation' else 2 if purpose == 'performance' else 0
    mutable_size = source_size + (harness_size if purpose == 'performance' else 0)
    mutable_count = source_count + (harness_count if purpose == 'performance' else 0)
    return dict(bytes=size * copies + mutable_size * writable_copies,
                inodes=count * copies + mutable_count * writable_copies,
                delivery_bytes=size, delivery_inodes=count, files=files)


def retry_feedback(state: dict, task: dict, result: dict) -> None:
    """Public response guidance; host exception text stays in host records."""
    purpose = task.get("purpose", "performance")
    if purpose == "performance":
        guidance = {"accept": "Acceptance failed. Supply a nonempty report under benchmarks/ and recorded evidence for one reviewed candidate.",
                    "measure": "Measurement failed. Check the command fields and inspect this run's delivered measurement evidence.",
                    "improve": "The improvement request needs a reason and an implementation instruction.",
                    "fail": "The failure decision needs a nonempty reason."}
        message = guidance.get(result.get("action"), "Return a performance action with its reason and required fields.")
    elif purpose.endswith("review"):
        message = "Review failed. Return an accept, revise or review decision with a reason; approval requires the current plan and successful source checks."
    else:
        message = "The result was rejected. Supply the required summary and current reading, plan or implementation files for this task."
    state["engineering"]["retry_feedback"] = message


def _profiling_tools(root: Path) -> dict:
    """Check a fixed Callgrind workload through the existing measurement runner."""
    import re
    import tempfile
    from trellis.agents.engineering_exec import open_regular

    selection, roots = performance._selected_tools(root)
    environment = performance._environment({**selection, 'executables': {
        name: dict(path=path) for name, path in selection['executables'].items()}})
    commands = {
        'valgrind': dict(argv=['valgrind', '--version'], cwd='.'),
        'callgrind_annotate': dict(argv=['callgrind_annotate', '--auto=yes', '--inclusive=yes', '--tree=both', '/output/callgrind.out'], cwd='.'),
        'workload': dict(argv=['/usr/bin/true'], cwd='.')}
    result = dict(available=False, metric='Ir', metric_meaning='Executed instructions in the compiled workload.',
        attribution='Instructions and calls; source lines require matching debug and source information.',
        probe='fixed_synthetic_workload', identity_scope='command entrypoints', commands={},
        profile_argv=['valgrind', '--tool=callgrind', '--callgrind-out-file=/output/callgrind.out', '/build/benchmark'],
        annotate_argv=commands['callgrind_annotate']['argv'])

    def text(path):
        try:
            with open_regular(path) as stream:
                raw = stream.read(1024 * 1024 + 1)
        except FileNotFoundError:
            return None
        if len(raw) > 1024 * 1024:
            return None
        try:
            return raw.decode('utf-8')
        except UnicodeError:
            return None

    with tempfile.TemporaryDirectory(prefix='trellis-callgrind-') as scratch:
        workspace = Path(scratch)
        for name in ('implementation', 'harness', 'build', 'output'):
            (workspace / name).mkdir()
        identities = {name: performance._executable(command, workspace, environment, roots)[0]
                      for name, command in commands.items()}
        result['commands'] = {name: dict(sha256=identity['sha256']) for name, identity in identities.items()}

        def execute(name, command):
            observed = performance._run_bounded(performance._sandbox(workspace, roots, environment, command),
                cwd=workspace, env={}, stdout=workspace / (name + '.stdout'), stderr=workspace / (name + '.stderr'), timeout=20)
            if observed['timeout']:
                return 'timeout'
            if observed.get('output_limit_exceeded'):
                return 'output_limit'
            if observed['exit_code'] != 0 or observed.get('execution_error'):
                return 'failed_execution'
            return None

        def probe():
            if any(identity['sha256'] is None for identity in identities.values()):
                return 'missing_tools'
            failure = execute('version', commands['valgrind'])
            if failure:
                return failure
            version = text(workspace / 'version.stdout')
            if version is None or not re.fullmatch(r'valgrind-[0-9]{1,3}(?:\.[0-9]{1,3}){1,2}\s*', version):
                return 'invalid_version'
            result['version'] = version.strip()
            failure = execute('profile', dict(argv=['valgrind', '--tool=callgrind',
                '--callgrind-out-file=/output/callgrind.out', '/usr/bin/true'], cwd='.'))
            if failure:
                return failure
            profile = text(workspace / 'output/callgrind.out')
            if profile is None or re.findall(r'(?m)^events: (.+)$', profile) != ['Ir']:
                return 'invalid_profile'
            totals = re.findall(r'(?m)^summary: ([0-9]+)$', profile)
            if len(totals) != 1 or len(totals[0]) > 20 or not 0 < int(totals[0]) < 2**64:
                return 'invalid_profile'
            instruction_count = int(totals[0])
            failure = execute('annotation', commands['callgrind_annotate'])
            if failure:
                return 'failed_annotation' if failure == 'failed_execution' else failure
            annotation = text(workspace / 'annotation.stdout')
            totals = re.findall(r'(?m)^\s*([0-9][0-9,]*)\s+(?:\([ 0-9.]+%\)\s+)?PROGRAM TOTALS\s*$', annotation or '')
            if (annotation is None or '/output/callgrind.out' not in annotation
                    or len(totals) != 1 or len(totals[0]) > 26
                    or int(totals[0].replace(',', '')) != instruction_count):
                return 'invalid_annotation'
            return None

        failure = probe()
        for name, command in commands.items():
            if performance._executable(command, workspace, environment, roots)[0] != identities[name]:
                raise RuntimeError('Profiling command identity changed during its capability probe')
        result.update(available=failure is None, status=failure or 'available')
    return result


def delivery(root: Path, state: dict, task: dict, destination: Path, *, native: dict | None = None) -> dict:
    """Render a positive file inventory, separate from host bookkeeping.

    This is a delivery contract, not a sandbox or a provider transport.
    """
    identity = input_identity(root, state)
    if destination.exists():
        raise ValueError("Role delivery requires a fresh directory")
    destination.mkdir(parents=True)
    config = read_json(root / "config.json")
    for name in config["paper_input"]["paper_files"]:
        path = destination / name
        raw = history._read(evidence_path(root, name))
        if hashlib.sha256(raw).hexdigest() != config["paper_input"]["files"][name]:
            raise ValueError("Original paper changed before delivery")
        history._put(path, raw)
    publish_inputs(root, identity, destination)
    purpose = task.get("purpose", "performance")
    engineering = state["engineering"]
    context = None
    if state.get('native_engineering'):
        request = native.get('in_flight_request') if native else None
        contract = request.get('engineering') if request else None
        if (native is None or digest(native) != state['native_engineering']['state_sha256']
                or native['request_seq'] != state['sequence'] or request is None
                or request['id'] != state['sequence'] or contract['purpose'] != purpose
                or native['engineering']['purpose'] != purpose
                or contract['subject'] != native['engineering']['subject']
                or native['engineering']['subject'] != native_subject(root)
                or contract.get('assignment_feedback') != native['engineering'].get('assignment_feedback')):
            raise RuntimeError('Native delivery requires its exact owned assignment snapshot')
        _check_issued_feedback(root, contract.get('assignment_feedback'), native['engineering'].get('last_consumption'))
        context = _native_public_history(root, native['engineering'], purpose)
        index, records, _ = context
        for identity, record in records.items():
            history._put(destination / 'history/records' / (identity + '.json'), canonical_json(record))
        history._put(destination / 'history/index.json', canonical_json(index))
    if purpose != "source_reading":
        for name in ("reading", "plan", "plan_approval"):
            if name in engineering:
                write_json(destination / f"{name}.json", engineering[name])
        if purpose == "plan" and engineering.get("proposal"):
            write_json(destination / "proposal.json", engineering["proposal"])
    if purpose in {"implementation", "code_review", "performance"}:
        copy_source_inputs(root / "work/crate", destination / "source")
    if purpose in {"implementation", "code_review", "performance"} and (root / "benchmarks/harness").exists():
        copy_harness(root / "benchmarks/harness", destination / "benchmarks/harness")
    if context is not None and purpose in {'code_review', 'performance'}:
        _copy_outputs(root, implementation_outputs(root, native['engineering']), destination)
    if context is not None and purpose=='performance':
        history._put(destination/'performance-budget.json',canonical_json(performance_budget(root,native['engineering'])))
    if purpose in {"implementation", "performance"}:
        history._put(destination / 'profiling-tools.json', canonical_json(_profiling_tools(root)))
    for run, record, public in _measurement_views(root, state, purpose, context is not None):
        source = root / performance.STORE / 'runs' / run
        for name, expected in public['evidence_files'].items():
            path = destination / 'measurements' / run / name
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(evidence_path(source, name), path)
            if sha256(path) != expected or path.stat().st_mode & 0o777 != record['evidence_modes'][name]:
                raise ValueError('Measurement evidence changed during delivery')
        write_json(destination / 'measurements' / run / 'record.json', public)
    # Only engineering-owned public reports enter this view. Provider logs,
    # host errors and the full preparation state are deliberately absent.
    feedback = {"instruction": task.get("instruction", ""), "validation": engineering.get("retry_feedback", "")}
    if purpose == 'code_review' and state.get('native_engineering'):
        feedback['implementation_summary'] = _text(engineering['implementation_summary'], 'Implementation summary')
    if context is not None:
        expected = _issued_feedback(contract.get('assignment_feedback'))
        if purpose == 'code_review':
            expected['implementation_summary'] = context[2][contract['candidate_sha256']]
        if feedback != expected:
            raise RuntimeError('Native issued feedback differs from its retained request')
    write_json(destination / "feedback.json", feedback)
    role_text = ('Review the retained checker feedback. Return {"decision":"revise|fail","reason":"assessment and required changes"}.'
                 if purpose == 'retry_review' else (FRAGMENTS / f"{purpose}.md").read_text())
    if purpose=='performance':
        role_text=role_text.replace('{performance_budget_guidance}',
            "Aim for three rounds, each consisting of an implementation change and its performance evaluation; four are the hard limit between resets. "
            "When cumulative improvement reaches 10% relative to the last reset's comparable baseline, declare reset_budget with the measurements, metric, gain in basis points and comparison reason. "
            "At exhaustion, select the best eligible measured candidate, declare a qualifying reset or report a blocker." if context is not None else '')
        role_text=role_text.replace('{performance_budget_reset_action}',
            '- {"action":"reset_budget","reason":"comparable improvement","baseline_evidence":["measurement ID"],"current_evidence":["measurement ID"],"metric":"metric name","improvement_bps":1000}'
            if context is not None else '')
    guidance = ('\nhistory/index.json records prior work and the feedback issued to each assignment.'
                if context is not None else '')
    prompt = (FRAGMENTS / "common.md").read_text().replace('{history_guidance}', guidance) + "\n" + role_text
    if purpose in {"implementation", "performance"}:
        prompt += '\nprofiling-tools.json describes the checked Callgrind capability and command syntax.\n'
    if context is not None and purpose in {'code_review', 'performance'} and (destination / 'implementation-outputs.json').exists():
        prompt += '\nimplementation-outputs.json indexes empirical outputs from the selected implementation.\n'
    if context is not None and purpose=='performance':
        prompt += '\nperformance-budget.json records the remaining implementation rounds and reset baseline.\n'
    if 'paper/TRELLIS_REFERENCES.json' in config['paper_input']['paper_files']:
        prompt += '\npaper/TRELLIS_REFERENCES.json indexes supplied example inputs.\n'
    if "implementation_notes" in state["engineering"]["input"]:
        prompt += "\nimplementation-notes.md supplies guidance subordinate to the paper.\n"
    if purpose in {"source_reading", "plan"}:
        prompt += "\n" + (FRAGMENTS / "record.md").read_text()
    (destination / "prompt.md").write_text(prompt)
    return {"fresh_context": True, "files": performance._inventory(destination),
            "artifact_kind": "preparation-performance-result" if purpose == "performance" else
                             "preparation-audit-result" if purpose.endswith("review") else "preparation-worker-result"}


def native_record(root: Path, value: dict) -> str:
    """Retain the exact host object named by a native evidence reference."""
    identity = digest(value)
    history._put(root / STORE / 'native-records' / (identity + '.json'), canonical_json(value))
    return identity


def native_read(root: Path, identity: str) -> dict:
    import re
    if not isinstance(identity, str) or not re.fullmatch('[0-9a-f]{64}', identity):
        raise RuntimeError('Invalid native engineering evidence reference')
    value = history._json(root / STORE / 'native-records' / (identity + '.json'))
    if digest(value) != identity:
        raise RuntimeError('Native engineering evidence changed')
    return value


def native_stage_evidence(root: Path) -> None:
    """Retain public content-addressed evidence referenced by native checkpoints."""
    import re
    import stat
    import subprocess
    from trellis.agents.host_files import safe_directory
    from trellis.dispatch import candidate
    from trellis.modeling.driver import runtime_root
    from trellis.paper_implementation import paper_source_path
    paths, source_paths, runs = [], [], set()

    def submitted_source(checked_sha, checked):
        host = native_read(root, checked['host_receipt_sha256'])
        request = host['request']
        if request['engineering']['purpose'] != 'implementation':
            return
        request_id, turn_number = checked['request_id'], checked['turn']
        if (type(request_id) is not int or request_id < 1 or type(turn_number) is not int or turn_number < 1
                or request['id'] != request_id or request['cycle'] != checked['cycle']
                or digest(request) != checked['request_sha256'] or host['turn'] != turn_number
                or host['sealed_candidate'] != checked['candidate_sha256']
                or request['engineering']['basis_sha256'] != checked['basis_sha256']):
            raise ValueError('Submitted engineering source binding differs')
        turn = Path(host['turn_path'])
        runtime = runtime_root(root)
        directory = turn.parent.parent
        if (turn.as_posix() != host['turn_path'] or directory.parent != runtime / 'worker-attempts'
                or not re.fullmatch(str(request_id) + r'-[0-9a-f]{16}', directory.name)
                or turn != directory / 'turns' / str(turn_number)
                or not re.fullmatch(re.escape((STORE / 'deliveries').as_posix()) + '/'
                                    + str(request_id) + r'-[0-9a-f]{32}', host['attempt'])):
            raise ValueError('Submitted engineering source is outside its runtime turn')
        relative = turn.relative_to(runtime).as_posix()
        saved = history._json(evidence_path(runtime, relative + '/execution/engineering-request.json'))
        original = evidence_path(root, host['attempt'] + '/request.json')
        original_request = history._json(original)
        if (sha256(original) != host['request_sha256']
                or sha256(evidence_path(root, host['attempt'] + '/basis.json')) != host['basis_sha256']
                or saved['submission']['engineering_request'] != request or saved['submission']['turn'] != turn_number
                or saved['submission']['native_runtime'] != str(runtime)
                or saved['request']['work_dir'] != str(root / host['attempt'] / 'view')
                or saved['request']['burst_home'] != original_request['burst_home']
                or saved['request']['execution_record'] != str(turn / 'execution/clock')):
            raise ValueError('Submitted engineering source admission differs')
        evidence_path(runtime, relative + '/candidate/catalog.json')
        sealed = history._verified(turn / 'candidate')
        if len(sealed) != 1 or sealed[0][0]['id'] != checked['candidate_sha256']:
            raise ValueError('Submitted engineering source seal differs')
        reference, record = sealed[0]
        metadata = record['metadata']
        completion = metadata['completion']
        if (record['kind'] != 'engineering-return' or metadata['request'] != request
                or metadata['turn'] != turn_number or completion['turn'] != turn_number
                or completion['response_sha256'] != candidate.digest(candidate.canonical_json(metadata['response']))):
            raise ValueError('Submitted engineering source completion differs')
        inventory = metadata['inventory']
        source_names = {name for name in inventory if name == 'source' or name.startswith('source/')}
        entries = {entry['path'][5:]: entry for entry in record['files']
                   if entry['path'] == 'view/source' or entry['path'].startswith('view/source/')}
        if source_names != set(entries):
            raise ValueError('Submitted engineering source inventory differs')
        projection = STORE / 'submissions' / str(request_id) / str(turn_number) / reference['id']
        if checked['outcome']['outcome']=='implementation' and 'outputs' in request['engineering']['output_paths']:
            outputs = _output_entries(record)
            for item in outputs:
                raw = history._read(turn / 'candidate/blobs' / item['sha256'])
                if hashlib.sha256(raw).hexdigest() != item['sha256'] or len(raw) != item['size']:
                    raise ValueError('Submitted implementation output blob differs')
                name = projection / 'outputs' / item['path']
                history._put(root / name, raw)
                paths.append(name)
            raw = history._read(turn / 'candidate' / reference['record'])
            history._put(root / projection / 'return.json', raw)
            paths.append(projection / 'return.json')
        if not entries:
            return
        prepared, omitted, files = [], [], []
        for name, entry in sorted(entries.items()):
            relative_source = Path(name)
            mode = metadata['modes'][name]
            if inventory[name] != entry['sha256'] or type(mode) is not int or not 0 <= mode <= 0o777:
                raise ValueError('Submitted engineering source metadata differs')
            reason = None
            if name == 'source':
                reason = 'source_root_file'
            elif (set(relative_source.parts) & {'.git', '.codex', '.agents', '.trellis', 'target', '__pycache__'}
                    or relative_source.suffix in {'.lean', '.llbc', '.olean'}):
                reason = 'source_output_path'
            else:
                try:
                    paper_source_path(name)
                except ValueError:
                    reason = 'git_control_path'
            if reason:
                omitted.append(dict(path=name, reason=reason))
            else:
                prepared.append((name, entry, mode))
        for name, entry, mode in prepared:
            raw = history._read(turn / 'candidate/blobs' / entry['sha256'])
            if hashlib.sha256(raw).hexdigest() != entry['sha256'] or len(raw) != entry['size']:
                raise ValueError('Submitted engineering source blob differs')
            destination = root / projection / name
            with safe_directory(destination.parent):
                existed = destination.exists() or destination.is_symlink()
                if existed:
                    existing = evidence_path(root, (projection / name).as_posix())
                    if stat.S_IMODE(existing.stat().st_mode) != mode:
                        raise ValueError('Archived engineering source mode differs')
                history._put(destination, raw)
                if not existed:
                    destination.chmod(mode)
            files.append(dict(path=name, sha256=entry['sha256'], size=entry['size'], mode=mode))
        receipt = dict(request_id=request_id, turn=turn_number, seal_sha256=reference['id'],
            checked_sha256=checked_sha, host_receipt_sha256=checked['host_receipt_sha256'],
            outcome=checked['outcome']['outcome'], complete=not omitted, omitted=omitted, files=files)
        with safe_directory(root / projection):
            history._put(root / projection / 'record.json', canonical_json(receipt))
        source_paths.extend([projection / 'record.json', *(projection / item['path'] for item in files)])

    records = root / STORE / 'native-records'
    names = sorted(records.iterdir())
    if len(names) > 4096:
        raise RuntimeError('Native engineering evidence exceeds its checkpoint bound')
    for path in names:
        if not re.fullmatch('[0-9a-f]{64}\\.json', path.name):
            raise RuntimeError('Unexpected native engineering evidence')
        value = native_read(root, path.stem)
        paths.append(path.relative_to(root))
        if 'host_receipt_sha256' in value:
            submitted_source(path.stem, value)
        if value.get('schema') == 'trellis-paper-performance-run/v1':
            runs.add(value['id'])
        if 'run_id' in value and 'retained_candidate' in value:
            runs.add(value['run_id'])
        if 'handoff_attempt' in value:
            attempt = Path(value['handoff_attempt'])
            if not re.fullmatch(r'\.trellis/modeling/engineering/handoffs/[0-9a-f]{32}', attempt.as_posix()):
                raise RuntimeError('Native handoff attempt path differs')
            paths.extend(attempt / name for name in ('before-state.json', 'native-transfer.json', 'prepared-state.json'))
    candidate_store = STORE / 'candidates'
    for reference, record in history._verified(root / candidate_store):
        paths.extend([candidate_store / 'catalog.json', candidate_store / reference['record']])
        paths.extend(candidate_store / 'blobs' / entry['sha256'] for entry in record['files'])
    for run in sorted(runs):
        record = performance.recorded_evidence(root, run)
        paths.extend(performance.STORE / 'runs' / run / name
                     for name in ['record.json', 'record.sha256', *record['evidence_files']])
    from trellis.modeling.performance_loop import ACCEPTED
    for path in (STORE / 'baseline.json', BASELINE_REPORT, ACCEPTED):
        if (root / path).exists() or (root / path).is_symlink():
            paths.append(path)
    if paths or source_paths:
        names = [path.as_posix() for path in sorted(set(paths) | set(source_paths))]
        expected = {name: sha256(evidence_path(root, name)) for name in names}
        lengths = {name: evidence_path(root, name).stat().st_size for name in names}
        subprocess.run(['git', 'update-index', '--add', '-z', '--stdin'],
                       cwd=root, input=b''.join(name.encode() + b'\0' for name in names),
                       check=True, capture_output=True, timeout=30)
        staged = subprocess.run(['git', 'ls-files', '--stage', '-z'], cwd=root,
                                check=True, capture_output=True, timeout=30).stdout
        entries = {}
        for entry in staged.split(b'\0'):
            if entry:
                metadata, name = entry.split(b'\t', 1)
                entries[name.decode()] = metadata.split()
        for name in names:
            mode = b'100755' if evidence_path(root, name).stat().st_mode & stat.S_IXUSR else b'100644'
            if name not in entries or len(entries[name]) != 3 or entries[name][0] != mode or entries[name][2] != b'0':
                raise ValueError('Engineering evidence Git mode differs')
        checks = subprocess.run(['git', 'cat-file', '--batch-check'], cwd=root,
            input=b''.join(entries[name][1] + b'\n' for name in names),
            check=True, capture_output=True, timeout=30).stdout
        rows = checks.splitlines()
        if len(rows) != len(names) or not checks.endswith(b'\n'):
            raise ValueError('Engineering evidence Git object inventory differs')
        for name, row in zip(names, rows, strict=True):
            if row.split() != [entries[name][1], b'blob', str(lengths[name]).encode()]:
                raise ValueError('Engineering evidence Git bytes differ (object size or type)')
        # Peak capture is one batch or one larger validated blob, plus framing.
        # Working-file sizes alone are insufficient: clean filters may expand.
        batch, size = [], 0
        def verify(batch):
            blobs = subprocess.run(['git', 'cat-file', '--batch'], cwd=root,
                input=b''.join(entries[name][1] + b'\n' for name in batch),
                check=True, capture_output=True, timeout=30).stdout
            offset = 0
            for name in batch:
                end = blobs.find(b'\n', offset)
                length = lengths[name];after = end + 1 + length
                if (end < offset or blobs[offset:end].split() != [entries[name][1], b'blob', str(length).encode()]
                        or after >= len(blobs) or blobs[after:after + 1] != b'\n'
                        or hashlib.sha256(memoryview(blobs)[end + 1:after]).hexdigest() != expected[name]):
                    raise ValueError('Engineering evidence Git bytes differ or response framing is invalid')
                offset = after + 1
            if offset != len(blobs):
                raise ValueError('Engineering evidence Git response has extra bytes')
        for name in names:
            if batch and size + lengths[name] > _STAGED_BLOB_BATCH_BYTES:
                verify(batch);batch, size = [], 0
            batch.append(name);size += lengths[name]
        if batch:verify(batch)


def _native_original_inputs(root: Path) -> dict:
    config = read_json(root / 'config.json')
    paper = config['paper_input']
    subject = dict(route=paper['route'], paper_sha256=preparation_paper_digest(root),
        targets_sha256=digest(paper['targets']), contract_sha256=sha256(root / STORE / 'contract.md'),
        notes_sha256=None)
    notes = root / STORE / 'implementation-notes.md'
    if notes.exists() or notes.is_symlink():
        subject['notes_sha256'] = hashlib.sha256(history._read(notes)).hexdigest()
    return subject


def _native_options(root: Path, inputs: dict | None = None) -> str:
    from trellis.paper_implementation import preparation_launch_config
    inputs = read_json(root / 'config.json') if inputs is None else inputs
    return digest(dict(inputs=inputs, roles=preparation_launch_config(root, inputs['launch_config'], inputs['paper_input'])))


def native_subject(root: Path) -> dict:
    subject = _native_original_inputs(root)
    subject.update(options_sha256=_native_options(root), tools_sha256=digest(performance._basis(root)[0]['tools']))
    return subject


def native_initialize(root: Path, policy: dict) -> dict:
    from trellis.modeling.driver import load_config, preparation_provider, runtime_root
    from trellis.runtime.kernel_cli import run_kernel_cli
    from trellis.runtime.bridge import _policy
    config = load_config(root / 'trellis.config.json')
    inputs = read_json(root / 'config.json')
    if inputs.get('engineering_protocol') != 'native-v1':
        raise ValueError('Native engineering requires its launch configuration')
    profiles = {}
    for purpose, role in NATIVE_ROLES.items():
        if purpose == 'retry_review':
            continue
        provider = preparation_provider(config, inputs, role)
        if provider.provider != 'codex' or provider.extra_args or not provider.model or not provider.effort:
            raise ValueError('Engineering requires explicit supported role profiles')
        profiles[purpose] = dict(provider=provider.provider, model=provider.model, reasoning_effort=provider.effort,
                                 allowance_ms=int(_policy(config).timing.burst_timeout_seconds * 1000))
    return run_kernel_cli(dict(action='engineering_runtime_init', root=str(runtime_root(root)), repo=str(root),
                               subject=native_subject(root), profiles=profiles, policy=policy))


NATIVE_ROLES = {'source_reading': 'reviewer', 'plan': 'planner', 'plan_review': 'reviewer',
                'implementation': 'worker', 'code_review': 'reviewer', 'performance': 'performance',
                'retry_review': 'reviewer'}


def _native_operation_put(root: Path, path: Path, value: dict) -> None:
    """Publish a handoff witness durably through its existing attempt ancestors."""
    from trellis.dispatch.candidate import _sync_dir
    history._put(path, canonical_json(value))
    directory = path.parent
    while True:
        _sync_dir(directory)
        if directory == root:
            break
        if not directory.is_relative_to(root):
            raise ValueError('Native operation is outside its preparation')
        directory = directory.parent


def native_operation_guard(root: Path) -> None:
    """Unknown owned invocations forbid even native journal recovery."""
    import re
    from trellis.agents.host_files import safe_directory
    from trellis.modeling.driver import runtime_root
    base = root / STORE / 'handoffs'
    def directories(path):
        try:
            with safe_directory(path, create=False) as (_, fd), __import__('os').scandir(fd) as scan:
                entries = []
                for entry in scan:
                    if len(entries) >= 4096:
                        raise RuntimeError('Native handoff operation scan exceeded its bound')
                    entries.append((entry.name, entry.is_dir(follow_symlinks=False)))
        except FileNotFoundError:
            return []
        if any(not re.fullmatch('[0-9a-f]{32}', name) or not directory for name, directory in entries):
            raise RuntimeError('Native handoff operation directory differs')
        return [path / name for name, _ in entries]
    for attempt in directories(base):
        for operation in directories(attempt / 'operations'):
            try:
                intent = history._json(operation / 'intent.json')
                claim = history._json(operation / 'claim.json')
                closure = history._json(operation / 'closure.json')
                if (set(intent) != {'request', 'source_identity', 'binary_sha256'}
                        or intent['request'].get('action') not in {'engineering_prepare_handoff_checkpoint', 'engineering_runtime_handoff'}
                        or intent['request'].get('root') != str(runtime_root(root))
                        or not re.fullmatch('trellis-worker-acceptance-v2:sha256:[0-9a-f]{64}', intent['source_identity'])
                        or not re.fullmatch('[0-9a-f]{64}', intent['binary_sha256'])
                        or set(claim) != {'intent_sha256', 'pid'} or claim['intent_sha256'] != digest(intent)
                        or type(claim['pid']) is not int or claim['pid'] < 1):
                    raise ValueError('Operation identity differs')
                if closure != dict(intent_sha256=digest(intent), claim_sha256=digest(claim), kernel_quiescent=True):
                    raise ValueError('Closure differs')
            except (OSError, ValueError, TypeError, KeyError, AttributeError):
                raise RuntimeError('Native handoff operation closure is unknown') from None


def _native_owned_handoff(root: Path, attempt: Path, request: dict) -> dict:
    from trellis.acceptance_identity import record_deployment
    from trellis.modeling.driver import REPO
    from trellis.runtime.kernel_cli import kernel_cli_command, run_kernel_cli
    native_operation_guard(root)
    command = kernel_cli_command()
    if len(command) != 1 or not Path(command[0]).is_absolute():
        raise RuntimeError('Native handoff requires its pinned kernel executable')
    binding = record_deployment(REPO, Path(command[0]), attempt / 'checkpoint-build.json')
    operation = attempt / 'operations' / uuid.uuid4().hex
    intent = dict(request=request, source_identity=binding['source_identity'], binary_sha256=binding['binary_sha256'])
    _native_operation_put(root, operation / 'intent.json', intent)
    payload = dict(request, operation=dict(path=str(operation), intent_sha256=digest(intent)))
    def close():
        claim = history._json(operation / 'claim.json')
        _native_operation_put(root, operation / 'closure.json',
                              dict(intent_sha256=digest(intent), claim_sha256=digest(claim), kernel_quiescent=True))
    try:
        result = run_kernel_cli(payload, owned=True, timeout_seconds=120)
    except BaseException as error:
        if getattr(error, 'kernel_quiescent', False) is True:
            try:
                close()
            except BaseException:
                pass
        raise
    close()
    return result


def native_prepare_handoff_checkpoint(root: Path, predecessor: str, record_sha: str, prepared_sha: str) -> dict:
    """Mechanical child of the owned native action; parent retains both locks."""
    import subprocess
    from trellis.modeling.driver import runtime_root
    current = history._json(runtime_root(root) / 'protocol_state.json')
    record = native_read(root, record_sha)
    attempt = root / record['handoff_attempt']
    prepared_path = attempt / 'prepared-state.json'
    state = history._json(prepared_path)
    if (digest(current) != predecessor or current['stage'] != 'EngineeringReady'
            or current['engineering']['subject'] != native_subject(root)
            or record['ready_state_sha256'] != predecessor or sha256(prepared_path) != prepared_sha
            or state['engineering']['native_handoff_record'] != record_sha
            or native_handoff_source(root) != record['downstream_source_sha256']):
        raise RuntimeError('Native prepare handoff bindings changed')
    from trellis.modeling.performance_loop import validate_acceptance
    before_path = attempt / 'before-state.json'
    before = history._json(before_path)
    if (sha256(before_path) != record['before_state_sha256']
            or record['baseline_sha256'] != digest(current['engineering']['baseline'])
            or before['native_engineering']['state_sha256'] != predecessor):
        raise RuntimeError('Native handoff predecessor evidence changed')
    validate_acceptance(root, before)
    write_json(root / '.trellis/modeling/state.json', state)
    native_stage_evidence(root)
    shown = subprocess.run(['git', 'show', 'HEAD:.trellis/modeling/state.json'], cwd=root, capture_output=True).stdout
    state_raw = history._read(root / '.trellis/modeling/state.json')
    if shown != state_raw:
        checkpoint(root, state)
    commit = subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=root, capture_output=True, text=True, check=True).stdout.strip()
    receipt = dict(predecessor_sha256=predecessor, baseline_sha256=record['baseline_sha256'], record_sha256=record_sha,
                   downstream_commit=commit, downstream_state_sha256=hashlib.sha256(state_raw).hexdigest(),
                   downstream_source_sha256=record['downstream_source_sha256'])
    _native_operation_put(root, attempt / 'downstream-checkpoint.json', receipt)
    return receipt


def validate_native_selection(root: Path, config: dict) -> None:
    from trellis.modeling.driver import runtime_root
    selected = config.get('engineering_protocol')
    if selected not in (None, 'native-v1'):
        raise ValueError('Unknown engineering protocol selection')
    path = runtime_root(root) / 'protocol_state.json'
    try:
        protocol = history._json(path)
    except FileNotFoundError:
        protocol = {}
    native = protocol.get('phase') in ('Engineering', 'engineering')
    if native != (selected == 'native-v1'):
        raise ValueError('Engineering protocol selection differs from its native runtime')
    try:
        projected = read_json(root / '.trellis/modeling/state.json')
    except FileNotFoundError:
        projected = {}
    if selected is None and 'native_engineering' in projected:
        raise ValueError('Native engineering selection is missing')
    if selected is not None and 'native_engineering' not in projected:
        raise ValueError('Native engineering initialization is incomplete')


def native_status(root: Path, preparation: dict, *, runner_active: bool = False) -> dict | None:
    """Read the native wait/assignment evidence without loading or recovering it."""
    from trellis.modeling.driver import runtime_root, REPO
    from trellis.modeling.engineering_transport import native_execution_guard
    import shlex
    import subprocess
    root = root.resolve()
    result = dict(stage='engineering', status='unresolved', sequence=preparation.get('sequence', 0),
                  role=None, purpose=None, attempt=None, attempt_phase=None, handed_off=False,
                  resume_blocked_reason='Engineering evidence is unresolved; inspect the retained attempt.',
                  handoff_command=None)
    try:
        try:
            config = read_json(root / 'config.json')
        except FileNotFoundError:
            config = {}
        validate_native_selection(root, config)
        if config.get('engineering_protocol') is None:
            return None
        protocol = history._json(runtime_root(root) / 'protocol_state.json')
        native = protocol['engineering']
        if native['subject']['options_sha256'] != _native_options(root, config):
            raise ValueError('Native launch settings changed')
        if any(native['subject'][key] != value for key, value in _native_original_inputs(root).items()):
            raise ValueError('Native original inputs changed')
        native_operation_guard(root)
        if not runner_active or protocol.get('in_flight_request') is None:
            native_execution_guard(root)
        if native.get('handoff') is not None:
            if not native_handoff_complete(root, dict(state=protocol)):
                raise ValueError('Native transfer is incomplete')
            return dict(stage=preparation.get('stage'), status=preparation.get('status'),
                        sequence=preparation.get('sequence'), role=(preparation.get('next') or {}).get('role'),
                        handed_off=True, resume_blocked_reason=None, handoff_command=None)
        from trellis.runtime.bridge import any_halt_marker_present
        if (any_halt_marker_present(runtime_root(root))
                or preparation.get('host_failure', {}).get('operation') in {'engineering_execution', 'engineering_handoff'}
                or next((root / STORE / 'deliveries').glob('*/execution-failure.json'), None) is not None):
            raise ValueError('Engineering execution is unresolved')
        request = protocol.get('in_flight_request') or {}
        attempt = (protocol.get('worker_attempt') or {}).get('attempt') or {}
        purpose = native['purpose']
        result.update(sequence=request.get('id', protocol['request_seq']), purpose=purpose,
                      role=NATIVE_ROLES[purpose], attempt=attempt.get('turn'),
                      attempt_phase=(attempt.get('phase') or {}).get('phase'))
        if native.get('halt') or native.get('failed'):
            from trellis.modeling.engineering_transport import native_finished_transport
            exhausted = native_finished_transport(root,protocol)
            if exhausted is not None:
                result.update(status='halted',provider_retry_exhausted=exhausted,
                    resume_blocked_reason='Provider transport retry budget exhausted; preserve the retained attempt for review.')
            return result
        transport_proof = None
        if protocol.get('in_flight_request') is not None and not runner_active:
            from trellis.modeling.engineering_transport import native_transport_restart_proof
            transport_proof = native_transport_restart_proof(root)
            if transport_proof is None:
                raise ValueError('Native assignment has no live controller')
        ready = protocol['stage'] == 'EngineeringReady' and native.get('baseline') is not None
        result.update(status='engineering_ready' if ready else 'active', resume_blocked_reason=None)
        if not ready and attempt.get('phase', {}).get('phase') == 'transport_ready':
            from trellis.dispatch import worker_transport
            from trellis.modeling.engineering_transport import native_transport_restart_proof
            transport_proof = transport_proof or native_transport_restart_proof(root)
            if transport_proof is None:
                raise ValueError('Native transport wait evidence is unresolved')
            status = transport_proof['status']
            result.update(status='provider_backoff', provider_retry=worker_transport.public_status(status,
                resume_allowed=runner_active and not (root / '.trellis/modeling/pause').exists()
                    and not (root / '.trellis-stop-after-checkpoint').exists()))
        if ready:
            result['handoff_command'] = shlex.join(['python3', str(REPO / 'scripts/paper_implementation.py'), 'handoff', str(root)])
        return result
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError):
        result.update(status='unresolved',
            resume_blocked_reason='Engineering evidence is unresolved; inspect the retained attempt.')
        result.pop('provider_retry', None)
        return result


def native_load(root: Path) -> dict:
    from trellis.modeling.driver import runtime_root
    from trellis.runtime.kernel_cli import run_kernel_cli
    native_operation_guard(root)
    from trellis.modeling.engineering_transport import native_execution_guard
    native_execution_guard(root)
    result = run_kernel_cli(dict(action='engineering_runtime_load', root=str(runtime_root(root))))
    native = result['state']['engineering']
    if native.get('handoff') is None:
        if native['subject'] != native_subject(root):
            raise RuntimeError('Native engineering launch inputs or tools changed')
    else:
        if (any(native['subject'][key] != value for key, value in _native_original_inputs(root).items())
                or native['subject']['options_sha256'] != _native_options(root)):
            raise RuntimeError('Native engineering original inputs changed after handoff')
        baseline = native_read(root, native['baseline']['selection_receipt_sha256'])
        if read_json(root / STORE / 'baseline.json') != baseline:
            raise RuntimeError('Native engineering frozen baseline changed after handoff')
        candidate(root, baseline['candidate'])
        _baseline_records(root, baseline)
    return result


def _native_handoff_source(directory: Path) -> str:
    files = performance._inventory(directory)
    return digest(dict(files=files, modes=performance._modes(directory, files)))


def native_handoff_source(root: Path) -> str:
    return _native_handoff_source(root / 'upstream')


def native_handoff_complete(root: Path, current: dict) -> bool:
    """The one native receipt permanently transfers routing to preparation."""
    import subprocess
    handoff = current['state']['engineering'].get('handoff')
    if handoff is None:
        return False
    record = native_read(root, handoff['record_sha256'])
    if (record['ready_state_sha256'] != handoff['predecessor_sha256']
            or record['baseline_sha256'] != handoff['baseline_sha256']):
        raise RuntimeError('Native engineering handoff record changed')
    shown = subprocess.run(['git', 'show', handoff['downstream_commit'] + ':.trellis/modeling/state.json'],
                           cwd=root, capture_output=True, check=True).stdout
    state = read_json(root / '.trellis/modeling/state.json')
    initial = __import__('json').loads(shown)
    if (hashlib.sha256(shown).hexdigest() != handoff['downstream_state_sha256']
            or initial['engineering'].get('native_handoff_record') != handoff['record_sha256']
            or state.get('engineering', {}).get('native_handoff_record') != handoff['record_sha256']
            or state['engineering'].get('baseline') != initial['engineering']['baseline']
            or not state['engineering'].get('handed_off')
            or subprocess.run(['git', 'merge-base', '--is-ancestor', handoff['downstream_commit'], 'HEAD'],
                              cwd=root, capture_output=True).returncode != 0):
        raise RuntimeError('Native engineering handoff has no downstream preparation lineage')
    return True


def native_handoff(root: Path, state: dict) -> dict:
    """Complete the explicit locked two-checkpoint transfer, retaining both trees."""
    import subprocess
    from trellis.modeling.driver import REPO, runtime_root
    current = native_load(root)
    if native_handoff_complete(root, current):
        return current
    if current['state']['stage'] != 'EngineeringReady':
        raise ValueError('Native handoff requires engineering readiness')
    matches = []
    for path in sorted((root / STORE / 'handoffs').glob('*/native-transfer.json')):
        record = history._json(path)
        if record['ready_state_sha256'] == current['state_sha256']:
            if native_read(root, digest(record)) != record:
                raise RuntimeError('Native handoff evidence changed')
            matches.append(record)
    if len(matches) > 1:
        raise RuntimeError('Native handoff has inconsistent retained attempts')
    if not matches:
        projected = native_project(root, state, current)
        if projected != state:
            raise ValueError('Native handoff requires its current readiness projection')
        handoff(root, state, native_ready=current)
        record = native_read(root, state['engineering']['native_handoff_record'])
    else:
        record = matches[0]
        attempt = root / record['handoff_attempt']
        before = history._json(attempt / 'before-state.json')
        if (sha256(attempt / 'before-state.json') != record['before_state_sha256']
                or record['baseline_sha256'] != digest(current['state']['engineering']['baseline'])
                or before['native_engineering']['state_sha256'] != current['state_sha256']):
            raise RuntimeError('Native handoff predecessor evidence changed')
        # Validate the original frozen baseline before completing retained moves.
        from trellis.modeling.performance_loop import validate_acceptance
        validate_acceptance(root, before)
        staged, previous = attempt / 'source', attempt / 'previous-upstream'
        if staged.exists():
            if _native_handoff_source(staged) != record['downstream_source_sha256']:
                raise RuntimeError('Native handoff retained source changed')
            if not previous.exists():
                if native_handoff_source(root) != record['previous_source_sha256']:
                    raise RuntimeError('Native handoff original upstream changed')
                (root / 'upstream').rename(previous)
            elif (root / 'upstream').exists():
                raise RuntimeError('Native handoff source publication is inconsistent')
            staged.rename(root / 'upstream')
        if (not previous.is_dir() or _native_handoff_source(previous) != record['previous_source_sha256']
                or native_handoff_source(root) != record['downstream_source_sha256']):
            raise RuntimeError('Native handoff retained upstream evidence changed')
        prepared = copy.deepcopy(before)
        prepared['engineering']['handoff_attempt'] = record['handoff_attempt']
        prepared['engineering']['native_handoff_record'] = digest(record)
        _handoff_state(prepared, record['handoff_attempt'])
        history._put(attempt / 'prepared-state.json', canonical_json(prepared))
        if state != before and state != prepared:
            raise RuntimeError('Native handoff preparation state changed')
        if state != prepared:
            state.clear()
            state.update(prepared)
    attempt = root / record['handoff_attempt']
    prepared_sha = sha256(attempt / 'prepared-state.json')
    prepared_result = _native_owned_handoff(root, attempt, dict(action='engineering_prepare_handoff_checkpoint',
        root=str(runtime_root(root)), repo=str(root), source_root=str(REPO),
        expected_state_sha256=current['state_sha256'], record_sha256=digest(record), prepared_state_sha256=prepared_sha))
    receipt = prepared_result['receipt']
    if history._json(attempt / 'downstream-checkpoint.json') != receipt:
        raise RuntimeError('Native downstream checkpoint receipt differs')
    try:
        result = _native_owned_handoff(root, attempt, dict(action='engineering_runtime_handoff', root=str(runtime_root(root)),
            source_root=str(REPO), expected_state_sha256=current['state_sha256'], handoff=receipt))
    except BaseException:
        # Recovery may establish a committed native receipt after interrupted persistence.
        try:
            recovered = native_load(root)
            if recovered['state']['engineering'].get('handoff') == receipt:
                native_handoff_complete(root, recovered)
        except BaseException:
            pass
        raise
    if not native_handoff_complete(root, result):
        raise RuntimeError('Native engineering handoff remains unresolved')
    return result


def _native_consumptions(root: Path, native: dict):
    """Newest first, retaining the checkpointed request and checked bindings."""
    cursor, seen = native.get('last_consumption'), set()
    while cursor is not None:
        key = digest(cursor)
        if key in seen or len(seen) >= 4096:
            raise RuntimeError('Native engineering history is inconsistent or exceeds its bound')
        seen.add(key)
        if native_read(root, key) != cursor:
            raise RuntimeError('Native engineering consumption changed')
        checked = native_read(root, cursor['checked_sha256'])
        if any(checked[name] != cursor[name] for name in ('request_id', 'turn', 'request_sha256', 'candidate_sha256')):
            raise RuntimeError('Native engineering history binding differs')
        host = native_read(root, checked['host_receipt_sha256'])
        request = host['request']
        if digest(request) != checked['request_sha256']:
            raise RuntimeError('Native engineering request evidence differs')
        contract = request['engineering']
        if (request['id'] != checked['request_id'] or request['cycle'] != checked['cycle']
                or contract['basis_sha256'] != checked['basis_sha256'] or contract['subject'] != native['subject']):
            raise RuntimeError('Native engineering historical request binding differs')
        yield cursor, checked, host
        cursor = native_read(root, cursor['predecessor_sha256']) if cursor['predecessor_sha256'] else None


def _issued_feedback(feedback: dict | None) -> dict:
    outcome = feedback['outcome'] if feedback else {}
    return dict(instruction=outcome.get('instruction') or outcome.get('reason', ''),
                validation='\n'.join(item['path'] + ': ' + item['rule'] for item in outcome.get('diagnostics', [])))


def _check_issued_feedback(root: Path, feedback: dict | None, predecessor: dict | None) -> None:
    if feedback and (predecessor is None or feedback['checked_sha256'] != predecessor['checked_sha256']
            or native_read(root, feedback['checked_sha256'])['outcome'] != feedback['outcome']):
        raise RuntimeError('Issued feedback differs from its preceding checked outcome')


def _native_measurement(root: Path, measurement: dict):
    retained = native_read(root, measurement['record_sha256'])
    record = performance.recorded_evidence(root, retained['run_id'])
    if (record['record_sha256'] != retained['record_sha256']
            or record['harness_sha256'] != measurement['harness_sha256']):
        raise RuntimeError('Native measurement evidence changed')
    return retained, record


def _native_public_history(root: Path, native: dict, purpose: str):
    """Project checkpointed public work once, without old views or turn files."""
    assignments, records, implementations, predecessor = [], {}, {}, None
    for cursor, checked, host in reversed(list(_native_consumptions(root, native))):
        contract = host['request']['engineering']
        previous_purpose = contract['purpose']
        outcome = checked['outcome']
        _check_issued_feedback(root, contract.get('assignment_feedback'), predecessor)
        predecessor = cursor
        issued = _issued_feedback(contract.get('assignment_feedback'))
        if previous_purpose == 'code_review':
            try:
                issued['implementation_summary'] = implementations[contract['candidate_sha256']]
            except KeyError:
                raise RuntimeError('Historical code review has no preceding implementation summary') from None
        if outcome['outcome'] == 'implementation':
            implementations[digest(outcome['candidate'])] = _text(outcome['summary'], 'Implementation summary')
        if purpose == 'source_reading' and not (previous_purpose == 'source_reading'
                or previous_purpose == 'retry_review' and contract['retry_review']['failed_purpose'] == 'source_reading'):
            continue
        public = {key: copy.deepcopy(outcome[key]) for key in ('outcome', 'summary', 'decision', 'reason', 'instruction',
                  'baseline_measurement_sha256','current_measurement_sha256','metric','improvement_bps',
                  'candidate', 'measurement', 'baseline', 'diagnostics') if key in outcome}
        if outcome['outcome'] == 'implementation':
            outputs = _outputs_from_checked(root, checked, host)
            if outputs is not None:
                public['implementation_outputs'] = {key: value for key, value in outputs.items() if key != 'projection'}
                public['implementation_outputs']['delivered_index'] = ('implementation-outputs.json'
                    if purpose in {'code_review', 'performance'} and native.get('candidate') == outcome['candidate'] else None)
        if outcome['outcome'] == 'measure':
            measurement = outcome['measurement']
            retained = native_read(root, measurement['record_sha256'])
            run = retained['run_id']
            if not isinstance(run, str) or not performance._RUN_ID.fullmatch(run):
                raise RuntimeError('Historical measurement run identity differs')
            supplied = (purpose in {'implementation', 'performance'}
                        and native.get('measurements', {}).get(measurement['record_sha256']) == measurement)
            public['measurement'].update(run_id=run, evidence_sha256=retained['record_sha256'],
                delivered_record=('measurements/' + run + '/record.json' if supplied else None))
        if 'record' in outcome:
            identity = digest(outcome['record'])
            if native_read(root, identity) != outcome['record']:
                raise RuntimeError('Historical public record differs')
            records[identity] = outcome['record']
            public['record_sha256'] = identity
        if host.get('proposal'):
            identity = host['proposal']
            records[identity] = native_read(root, identity)
            public['proposal_sha256'] = identity
        assignment = dict(sequence=cursor['request_id'], purpose=previous_purpose, turn=cursor['turn'],
            checked_sha256=cursor['checked_sha256'], subject_sha256=digest(contract['subject']),
            generation=contract['generation'], basis_sha256=contract['basis_sha256'],
            issued_feedback=issued, outcome=public)
        for key in ('reading_sha256', 'plan_sha256', 'plan_approval_sha256', 'candidate_sha256', 'reviewed_candidate_sha256'):
            assignment[key] = contract.get(key)
        if contract.get('retry_review'):
            assignment['retry_review'] = {key: copy.deepcopy(contract['retry_review'][key]) for key in
                ('failed_purpose', 'basis_sha256', 'request_sha256', 'candidate_sha256', 'checked_sha256', 'diagnostics')}
        assignments.append(assignment)
    index = dict(subject_sha256=digest(native['subject']), assignments=assignments,
                 records={identity: 'history/records/' + identity + '.json' for identity in sorted(records)})
    return index, records, implementations


def native_project(root: Path, previous: dict, current: dict) -> dict:
    """Resolve checked evidence for delivery; the native state owns all decisions."""
    state = copy.deepcopy(previous)
    state['stage'] = 'engineering'
    protocol = current['state']
    native = protocol['engineering']
    subject = native['subject']
    identity = dict(paper=subject['paper_sha256'], contract=subject['contract_sha256'])
    if subject['notes_sha256'] is not None:
        identity['implementation_notes'] = subject['notes_sha256']
    projected = dict(input=identity, candidates={}, checks={})
    for key in ('reading', 'plan'):
        if native.get(key + '_sha256'):
            projected[key] = native_read(root, native[key + '_sha256'])
    feedback = native.get('assignment_feedback')
    if feedback and feedback['outcome']['outcome'] == 'propose':
        checked = native_read(root, feedback['checked_sha256'])
        host = native_read(root, checked['host_receipt_sha256'])
        projected['proposal'] = native_read(root, host['proposal'])
    if native.get('plan_approval_sha256'):
        checked = native_read(root, native['plan_approval_sha256'])
        projected['plan_approval'] = dict(plan=digest(projected['plan']), **checked['outcome'])
    # The journaled native consumption chain supplies one row per assignment.
    entries = list(_native_consumptions(root, native))
    rows = [dict(sequence=cursor['request_id'], stage='engineering', role=NATIVE_ROLES[host['request']['engineering']['purpose']],
                 purpose=host['request']['engineering']['purpose'], result=checked['outcome'], native_consumption=cursor,
                 engineering_attempt=host['attempt']) for cursor, checked, host in entries]
    runs = []
    for reference, measurement in native['measurements'].items():
        if reference != measurement['record_sha256']:
            raise RuntimeError('Native measurement reference differs')
        retained, record = _native_measurement(root, measurement)
        candidate(root, retained['retained_candidate'])
        runs.append(record['id'])
        projected['candidates'][record['id']] = retained['retained_candidate']
    state['audits'] = {}
    if native.get('candidate'):
        value = native['candidate']
        record = native_read(root, value['checks'][0]['receipt_sha256'])
        actual = performance.recorded_evidence(root, record['id'])
        if actual != record:
            raise RuntimeError('Native Cargo evidence changed')
        if native['purpose'] == 'code_review':
            implementation = next((row['result'] for row in rows if row['purpose'] == 'implementation'
                and row['result'].get('outcome') == 'implementation' and row['result']['candidate'] == value), None)
            if implementation is None:
                raise RuntimeError('Native implementation summary has no current candidate evidence')
            projected['implementation_summary'] = _text(implementation['summary'], 'Implementation summary')
        projected['checks'][record['basis']] = record['id']
        state['observation'] = record
        if native.get('current_reviewed_candidate_sha256'):
            reviewed = native['reviewed'][native['current_reviewed_candidate_sha256']]
            decision = native_read(root, reviewed['review_sha256'])['outcome']
            state['audits']['engineering'] = dict(decision='accept', reason=decision['reason'],
                basis=record['basis'], plan=value['plan_sha256'], checks=record['id'], checks_sha256=record['record_sha256'])
    state['engineering'] = projected
    state['performance'] = dict(runs=runs, history=[])
    if state['audits'].get('engineering'):
        from trellis.modeling.performance_loop import _audit_hash
        state['performance'].update(basis=state['audits']['engineering']['basis'], audit_sha256=_audit_hash(state))
    if native.get('baseline'):
        baseline = native_read(root, native['baseline']['selection_receipt_sha256'])
        if read_json(root / STORE / 'baseline.json') != baseline:
            raise RuntimeError('Native engineering baseline changed')
        projected.update(baseline=baseline, selected=baseline['candidate'])
        state['performance']['approval'] = baseline['performance']
    purpose = native['purpose']
    issued = _issued_feedback(feedback)
    instruction = issued['instruction']
    if issued['validation']:
        projected['retry_feedback'] = issued['validation']
    state.update(stage='engineering', sequence=protocol['request_seq'],
                 status='engineering_ready' if native.get('baseline') else 'halted' if native.get('halt') else 'failed' if native['failed'] else 'active',
                 next=dict(role=NATIVE_ROLES[purpose], purpose=purpose, instruction=instruction),
                 native_engineering=dict(state_sha256=current['state_sha256'], event_count=current['event_count']),
                 history=list(reversed(rows)))
    if state['status'] != 'active':
        state['next'] = {}
    return state

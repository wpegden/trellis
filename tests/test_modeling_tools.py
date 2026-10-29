import json
import sys
from pathlib import Path

import pytest

from trellis.modeling.cache import Cache
from trellis.modeling.comparison import compare
from trellis.modeling.inspection import cli_preview, context_preview, inspect_observation, observation_summary, role_context
from trellis.modeling.modules import assemble, attach_diagnostics, declaration_index, order_modules
from trellis.modeling.observations import log_diagnostics, observe, passed, write_json
from trellis.modeling.probe import probe_source


def test_route_context_preview_fits_remaining_prompt_budget_without_losing_full_record():
    # Approved routes contain many named prose fields. Keeping 500 characters
    # per field exceeded the 6,682 characters left in a production reviewer
    # prompt even after every array had been shortened.
    prose = "Detailed route requirement. " * 150
    context = {"task": {"role": "reviewer"}, "state": {
        "approved_route": {"resolution": {f"requirement_{i}": prose for i in range(16)},
                           "assessment": prose, "sequence": 11},
        "observation": {"remaining_axioms": 19},
    }, "stage_check": "lean", "goal_ids": [f"goal_{i}" for i in range(32)]}
    original = json.dumps(context)
    preview = context_preview(context, limit=6682, preserve_keys=True)
    record = json.loads(preview)
    assert len(preview) <= 6682
    assert record["task"]["role"] == "reviewer"
    assert record["stage_check"] == "lean"
    assert record["state"]["approved_route"]["sequence"] == 11
    assert record["state"]["observation"]["remaining_axioms"] == 19
    assert set(record["state"]["approved_route"]["resolution"]) == set(context["state"]["approved_route"]["resolution"])
    assert "truncated" in preview and "full request file" in preview
    assert json.dumps(context) == original


def test_transcript_pages_bound_large_records_and_find_failed_commands(tmp_path):
    from trellis.chat_history import inspect_transcript
    path = tmp_path / "output.log"
    records = [{"type": "item.completed", "item": {"type": "command_execution", "command": "cargo test",
               "exit_code": 1, "aggregated_output": "x" * 1000000 + "needle" + "\\\n" * 10000}},
               {"type": "item.completed", "item": {"type": "agent_message", "text": "Read the result."}}]
    path.write_text("\n".join(json.dumps(item) for item in records) + "\n")
    found = inspect_transcript(path, kind="failure", contains="needle", max_chars=2000)
    assert len(json.dumps(found, ensure_ascii=False)) <= 2000
    assert "needle" in found["entries"][0]["text"]
    first = inspect_transcript(path, limit=1)
    assert first["has_more"] and first["next_line"] == 1
    second = inspect_transcript(path, after_line=first["next_line"])
    assert [item["text"] for item in second["entries"]] == ["Read the result."]

    # Raw Codex sessions retain tool calls/results absent from flattened logs.
    timestamp = "2026-09-10T02:44:57.000Z"
    payloads = [
        {"type": "reasoning", "summary": [], "encrypted_content": "ENCRYPTED PRIVATE DATA"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Checking now."}]},
        {"type": "function_call", "namespace": "collaboration", "name": "wait_agent",
         "call_id": "wait-1", "arguments": '{"timeout_ms":180000}'},
        {"type": "function_call_output", "call_id": "wait-1", "output": '{"message":"Wait timed out.","timed_out":true}'},
        {"type": "custom_tool_call", "name": "exec", "call_id": "exec-1",
         "input": 'const r = await tools.exec_command({cmd:"cargo test"}); text(r.output);'},
        {"type": "custom_tool_call_output", "call_id": "exec-1",
         "output": [{"type": "input_text", "text": "Script running with cell ID 9"},
                    {"type": "input_text", "text": "session_id: 12\n" + "\\\n" * 10000}]},
    ]
    raw_records = [{"type": "response_item", "timestamp": timestamp, "payload": payload}
                   for payload in payloads]
    raw_records.append({"type": "event_msg", "timestamp": timestamp,
                        "payload": {"type": "item_completed", "item": {
                            "type": "CommandExecution", "command": ["bash", "-lc", "cargo test"],
                            "aggregated_output": "test failed", "exit_code": 1, "process_id": "12"}}})
    raw_path = tmp_path / "rollout.jsonl"
    raw_path.write_text("\n".join(json.dumps(record) for record in raw_records) + "\n")
    found = inspect_transcript(raw_path, contains="wait_agent", limit=1)
    assert found["entries"][0]["call_id"] == "wait-1"
    assert found["entries"][0]["timestamp"] == timestamp
    assert '"timeout_ms":180000' in found["entries"][0]["text"]
    assert found["has_more"]
    following = inspect_transcript(raw_path, contains="wait_agent", after_line=found["next_line"])
    assert len(following["entries"]) == 1
    assert following["entries"][0]["title"] == "Tool output"
    assert "timed_out" in following["entries"][0]["text"]
    messages = inspect_transcript(raw_path, kind="message")
    assert [entry["text"] for entry in messages["entries"]] == ["Checking now."]
    failure = inspect_transcript(raw_path, kind="failure")["entries"][0]
    assert failure["exit_code"] == 1 and failure["process_id"] == "12"
    cursor, raw_entries = 0, []
    while True:
        page = inspect_transcript(raw_path, after_line=cursor, max_chars=2000)
        encoded = json.dumps(page, ensure_ascii=False)
        assert len(encoded) <= 2000 and "ENCRYPTED PRIVATE DATA" not in encoded
        raw_entries.extend(page["entries"])
        assert page["next_line"] > cursor
        cursor = page["next_line"]
        if not page["has_more"]:
            break
    assert len(raw_entries) == 6
    assert any(entry["title"] == "Tool call" and "text(r.output)" in entry["text"] for entry in raw_entries)
    assert any("session_id: 12" in entry["text"] for entry in raw_entries)


def test_generated_module_graph_retains_templates_without_compiling_them(tmp_path):
    raw, output = (tmp_path / name for name in ("raw", "Lean"))
    (raw / "Demo").mkdir(parents=True)
    types = "set_option autoImplicit true\nnamespace Demo\nstructure Word where\n  value : Nat\nend Demo\n"
    (raw / "Demo/Types.lean").write_text(types)
    (raw / "Demo/FunsExternal_Template.lean").write_text("axiom Demo.missing : Demo.Word\n")
    (raw / "Demo/Funs.lean").write_text("import Demo.Types\ndef Demo.call (w : Demo.Word) := w.value\n")
    modules = assemble(raw, output)
    order = order_modules(modules)
    assert order.index("Demo.Types") < order.index("Demo.Funs") < order.index("Combined")
    assert "Demo.FunsExternal_Template" not in order
    assert (raw / "Demo/FunsExternal_Template.lean").read_text().startswith("axiom")
    assert (raw / "Demo/Types.lean").read_text() == types
    assert (output / "Demo/Types.lean").read_text() == types.replace("autoImplicit true", "autoImplicit false")
    assert (output / "Demo/Funs.lean").read_text().count("set_option autoImplicit false") == 1


def test_module_graph_rejects_cycles(tmp_path):
    a, b = tmp_path / "A.lean", tmp_path / "B.lean"
    a.write_text("import B\n")
    b.write_text("import A\n")
    with pytest.raises(ValueError, match="Cyclic"):
        order_modules({"A": a, "B": b})


def test_navigation_retains_extractor_source_locations(tmp_path, capsys):
    source = tmp_path / "M.lean"
    source.write_text("namespace Demo\n/-- [crate::f]:\nSource: 'lib.rs', lines 3:0-5:1 -/\ndef f := 1\ndef g := f\nend Demo\n")
    index = declaration_index(tmp_path)
    write_json(tmp_path / "index.json", index)
    write_json(tmp_path / "observation.json", {"declaration_index": str(tmp_path / "index.json")})
    found = inspect_observation(tmp_path / "observation.json", symbol="crate::f")
    assert found["declarations"][0]["name"] == "Demo.f"
    assert found["declarations"][0]["source"]["start"] == "3:0"
    assert found["reference_candidates_from"] == ["Demo.g"]
    diagnostics = [{"text": f"{source}:4:2: error: invalid definition"},
                   {"text": "Source: 'lib.rs', lines 3:0-4:1"}]
    attach_diagnostics(diagnostics, index["declarations"])
    assert all(item["declarations"] == ["Demo.f"] for item in diagnostics)
    observation = {"steps": [{"name": "cargo", "exit_code": 0}, {"name": "lean", "module": "M", "exit_code": 1}],
                   "diagnostics": [{"stage": "cargo", "text": "warning: unused import"}] * 20
                                  + [{"stage": "lean", "module": "M", "text": "error: invalid definition"}]}
    assert observation_summary(observation)["diagnostics"][0]["stage"] == "lean"
    target = index["declarations"][0]
    index["declarations"] = [{**target, "name": f"Demo.f.closure{i}", "source_symbol": f"crate::f::closure{i}"}
                             for i in range(21)] + index["declarations"]
    write_json(tmp_path / "index.json", index)
    for symbol in ("Demo.f", "crate::f", "f"):
        result = inspect_observation(tmp_path / "observation.json", symbol=symbol)
        assert result["declarations"][0]["name"] == "Demo.f"
        assert result["declaration_count"] == 22 and result["declarations_omitted"] == 2
        assert json.loads(context_preview(result, preserve_keys=True))["declarations"][0]["name"] == "Demo.f"
    index["declarations"] *= 100
    write_json(tmp_path / "index.json", index)
    result = inspect_observation(tmp_path / "observation.json", source="lib.rs")
    rendered = context_preview(result, preserve_keys=True, details="the full index")
    assert len(rendered) <= 16000
    assert json.loads(rendered)["full_index"] == str(tmp_path / "index.json")
    assert result["declaration_count"] > len(result["declarations"])

    from scripts.pv_modeling import _main
    _main(["inspect", str(tmp_path), "--observation", str(tmp_path / "observation.json"), "--source", "lib.rs"])
    typed = json.loads(capsys.readouterr().out)
    assert all(isinstance(item, dict) for item in typed["declarations"])
    assert typed["declarations_omitted"] + len(typed["declarations"]) == typed["declaration_count"]
    assert "truncated" in rendered  # Prompt previews retain their markers.

    (tmp_path / "generated.lean").write_text("namespace Demo\n" + "\n".join(
        f"/-- [crate::operation{i}] -/\naxiom operation{i} : Nat" for i in range(25)) + "\nend Demo\n")
    _main(["inspect-native", str(tmp_path)])
    native = json.loads(capsys.readouterr().out)
    assert all(isinstance(item, dict) for item in native["declarations"])
    assert native["declarations_omitted"] + len(native["declarations"]) == native["declaration_count"] == 25
    assert len(json.loads(Path(native["full_index"]).read_text())["declarations"]) == 25

    nested = json.loads(cli_preview({"rows": [list(range(20))] * 20,
                                     "imports": {"M": [f"Dependency{i}" for i in range(20)]}},
                                    details="full records"))
    assert nested["array_items_omitted"]["/rows"] == 5 and len(nested["rows"]) == 15
    assert nested["array_items_omitted"]["/rows/0"] == 5
    assert all(isinstance(value, int) for row in nested["rows"] for value in row)
    assert nested["array_items_omitted"]["/imports/M"] == 5
    assert all(isinstance(imports, list) for imports in nested["imports"].values())
    assert len(nested["imports"]["M"]) == 15

    raw = tmp_path / "Generated/m.llbc"
    raw.parent.mkdir()
    raw.write_text("retained serialized input")
    pretty = "".join(f"// Full name: crate::f::closure{i}\nfn closure{i}() {{}}\n" for i in range(30))
    pretty += ("// Full name: crate::f\nfn f() {}\nfn top_level(f: f) -> f {}\n"
               "fn f<T>() {}\nfn cfg::f::child() {}\nconst only_type: f = value\n")
    charon = tmp_path / "charon"
    charon.write_text(f"#!{sys.executable}\nimport sys\n"
                      f"assert sys.argv[1:] == {['pretty-print', str(raw)]!r}\n"
                      f"print({pretty!r}, end='')\n")
    charon.chmod(0o755)
    write_json(tmp_path / "config.json", {"tools": {"charon": str(charon)}})
    write_json(tmp_path / "work/extraction.json", {})
    assert _main(["inspect", str(tmp_path), "--observation", str(tmp_path / "observation.json"),
                  "--ir", "--symbol", "Demo.f"]) == 0
    with_lean = json.loads(capsys.readouterr().out)
    assert with_lean["headers"][0]["text"] == "// Full name: crate::f"
    assert with_lean["header_count"] == 31
    assert len(with_lean["headers"]) + with_lean["array_items_omitted"]["/headers"] == 31

    write_json(tmp_path / "observation.json", {"declaration_index": str(tmp_path / "missing-index.json"),
               "steps": [{"name": "charon", "exit_code": 0,
               "argv": [str(charon), "cargo", "--dest-file", str(raw)]}]})
    assert _main(["inspect", str(tmp_path), "--observation", str(tmp_path / "observation.json"),
                  "--ir", "--symbol", "f"]) == 0
    without_lean = json.loads(capsys.readouterr().out)
    assert without_lean["status"] == "available"
    assert [item["name"] for item in without_lean["headers"][:2]] == ["f", "crate::f"]
    assert without_lean["header_count"] == 33
    assert Path(without_lean["pretty_file"]).read_text() == pretty
    headers = json.loads(Path(without_lean["full_index"]).read_text())["headers"]
    assert len(headers) == 35 and headers[-1]["name"] == "only_type"
    assert raw.read_text() == "retained serialized input"


def test_native_translation_metadata_supplies_the_source_mapping(tmp_path):
    (tmp_path / "M.lean").write_text("namespace Demo\ndef f := 0\nend Demo\n")
    write_json(tmp_path / "translation.json", {"functions": [{"lean_name": "Demo.f", "rust_name": "source::f",
               "source": {"file": "source.rs", "begin_line": 12, "end_line": 14}, "can_fail": True}]})
    declaration = declaration_index(tmp_path)["declarations"][0]
    assert declaration["provenance"] == "aeneas"
    assert declaration["source_symbol"] == "source::f"
    assert declaration["source"]["start"] == 12
    assert declaration["can_fail"]


def test_diagnostics_show_distinct_failures_and_filter_by_declaration(tmp_path, monkeypatch, capsys):
    model = tmp_path / "M.lean"
    model.write_text("def first := 1\ndef second := 2\n")
    index = declaration_index(tmp_path)
    write_json(tmp_path / "index.json", index)
    log = tmp_path / "lean.log"
    log.write_text("".join(f"{model}:1:{i}: error: Unknown identifier `missing`\n" for i in range(20))
                   + f"{model}:2:1: error: Type mismatch\n  actual\nbut expected\n  other\n")
    diagnostics = log_diagnostics(log, stage="lean", module="M")
    attach_diagnostics(diagnostics, index["declarations"])
    observation = {"diagnostics": diagnostics, "declaration_index": str(tmp_path / "index.json")}
    summary = observation_summary(observation)
    assert summary["diagnostic_count"] == 21 and summary["diagnostic_group_count"] == 2
    assert summary["diagnostics"][0]["occurrences"] == 20
    assert "but expected" in summary["diagnostics"][1]["context"]
    write_json(tmp_path / "observation.json", observation)
    focused = inspect_observation(tmp_path / "observation.json", symbol="second")
    assert focused["diagnostic_count"] == 1
    assert "Type mismatch" in focused["diagnostics"][0]["text"]

    index["declarations"][0]["source"] = {"path": "error.rs", "start": 3, "end": 8}
    index["declarations"][1]["source"] = {"path": "other.rs", "start": 9, "end": 12}
    write_json(tmp_path / "index.json", index)
    log.write_text("[\x1b[91mError\x1b[39m] Unsupported source form\n"
                   "Source: 'error.rs', lines 5:0-7:1\n"
                   "[Error] Unsupported source form\n" + "long detail\n" * 15
                   + "Source: 'other.rs', lines 10:0-11:1\n"
                   "Compiler source: interp/Interp.ml, line 609\n")
    diagnostics = log_diagnostics(log, stage="aeneas")
    attach_diagnostics(diagnostics, index["declarations"])
    assert len(diagnostics) == 2
    assert [item["declarations"] for item in diagnostics] == [["first"], ["second"]]
    assert diagnostics[1]["source"]["path"] == "other.rs"
    assert "Source:" not in diagnostics[1]["context"]  # Beyond the bounded preview.
    observation = {
        "steps": [{"name": "aeneas", "exit_code": 1, "log": str(log)}],
        "diagnostics": diagnostics, "declaration_index": str(tmp_path / "index.json"),
        "observation_file": str(tmp_path / "observation.json"), "stable": True,
    }
    write_json(tmp_path / "observation.json", observation)
    focused = inspect_observation(tmp_path / "observation.json", source="error.rs")
    assert focused["diagnostic_count"] == 1
    assert focused["diagnostics"][0]["text"] == "[Error] Unsupported source form"
    assert "Source: 'error.rs'" in focused["diagnostics"][0]["context"]
    focused = inspect_observation(tmp_path / "observation.json", source="other.rs")
    assert focused["diagnostics"][0]["text"] == "[Error] Unsupported source form"

    from scripts import pv_modeling
    monkeypatch.setattr(pv_modeling, "observe", lambda *args, **kwargs: observation)
    monkeypatch.setattr(pv_modeling, "probe_source", lambda *args, **kwargs: observation)
    for argv in (["check", str(tmp_path), "--through", "aeneas"],
                 ["probe", str(tmp_path), "--source", str(tmp_path), "--through", "aeneas"]):
        assert pv_modeling._main(argv) == 1
        compact = json.loads(capsys.readouterr().out)
        assert compact["diagnostic_count"] == 2 and compact["diagnostic_group_count"] == 2
        assert compact["checks"] == [{"name": "aeneas", "exit_code": 1, "log": str(log),
                                      "timeout": False, "timeout_seconds": None}]
        assert compact["stable"] and compact["observation_file"] == str(tmp_path / "observation.json")
        for preview, original in zip(compact["diagnostics"], diagnostics):
            assert preview["source"] == original["source"] and preview["text"] == original["text"]
            assert preview["line"] == original["line"] and preview["occurrences"] == 1
            assert not {"context", "declarations", "log"}.intersection(preview)
    assert observation_summary(observation)["diagnostics"][0]["context"] == diagnostics[0]["context"]
    pv_modeling._main(["inspect", str(tmp_path), "--observation", str(tmp_path / "observation.json"),
                      "--symbol", "first"])
    rich = json.loads(capsys.readouterr().out)
    assert rich["diagnostics"][0]["context"] == diagnostics[0]["context"]
    assert rich["diagnostics"][0]["declarations"] == ["first"]

    write_json(tmp_path / "observation.json", {"steps": [{"name": "charon", "exit_code": 0}]})
    unavailable = inspect_observation(tmp_path / "observation.json", symbol="second")
    assert unavailable["index_status"].startswith("unavailable")
    assert "declaration_count" not in unavailable

    log.write_text("error[E0308]: mismatched types\n"
                   "   --> error.rs:5:7\n"
                   "    ::: other.rs:10:3\n"
                   "    = note: related definition\n"
                   "   --> other.rs:11:2\n"
                   "warning: related location only\n"
                   "    ::: other.rs:10:3\n")
    rust = log_diagnostics(log, stage="cargo")
    attach_diagnostics(rust, index["declarations"])
    assert rust[0]["source"] == {"path": "error.rs", "start": 5, "end": 5, "column": 7}
    assert rust[0]["declarations"] == ["first"]
    assert "source" not in rust[1]
    compact = observation_summary({"diagnostics": rust}, diagnostic_details=False)
    assert compact["diagnostics"][0]["source"] == rust[0]["source"]


def test_cache_ownership_is_explicit(tmp_path):
    trusted, scratch = tmp_path / "trusted", tmp_path / "scratch"
    source = tmp_path / "artifact"
    source.write_text("worker artifact")
    worker = Cache(scratch, [trusted])
    supervisor = Cache(trusted)
    worker.put("lean", "key", {"module": source})
    assert supervisor.get("lean", "key") is None
    source.write_text("supervisor artifact")
    supervisor.put("lean", "key", {"module": source})
    assert (worker.get("lean", "key") / "module").read_text() == "supervisor artifact"


def test_lean_success_requires_every_module(tmp_path):
    observation = {"modules": {"A": "A.lean", "B": "B.lean"},
                   "steps": [{"name": "lean", "module": "A", "exit_code": 0}]}
    assert not passed(observation, "lean")
    observation["steps"].append({"name": "lean", "module": "B", "exit_code": 1})
    assert not passed(observation, "lean")
    observation["steps"][-1]["exit_code"] = 0
    assert passed(observation, "lean")
    observation["elaboration_placeholders"] = 1
    assert not passed(observation, "lean")


def test_comparison_observes_independent_copies_and_times_out(tmp_path):
    a, b = tmp_path / "a", tmp_path / "b"
    a.mkdir(); b.mkdir()
    (a / "value").write_text("1")
    (b / "value").write_text("2")
    command = [sys.executable, "-c", "from pathlib import Path; import sys; print((Path(sys.argv[1])/'value').read_text())", "{source}"]
    result = compare(a, b, command, tmp_path / "different")
    assert not result["same_observation"]
    assert (a / "value").read_text() == "1"
    result = compare(a, a, command, tmp_path / "same")
    assert result["same_observation"]
    result = compare(a, a, [sys.executable, "-c", "import sys; sys.exit(3)", "{source}"], tmp_path / "failed")
    assert not result["commands_succeeded"] and not result["same_observation"]
    result = compare(a, b, [sys.executable, "-c", "import time; time.sleep(20)", "{source}"], tmp_path / "timeout", timeout=0.05)
    assert not result["same_observation"]
    assert result["baseline"]["timeout"] and result["candidate"]["timeout"]


def test_preparation_context_retains_current_work_without_replaying_history():
    state = {"stage": "adaptation", "status": "active", "declined": {}, "feedback": "", "observation": {},
             "audits": {},
             "history": [{"task": {"role": "worker"}, "response": {"summary": "current work"}},
                         {"task": {"role": "reviewer"}, "response": {"instruction": "old assignment"}}]}
    context = role_context(state, {"role": "reviewer"})
    assert context["latest_work"]["summary"] == "current work"
    assert "history" not in context
    state["stage"] = "setup"
    rejected = {"sequence": 3, "stage": "adaptation", "task": {"role": "audit", "kind": "reopen"},
                "response": {"decision": "accept", "reason": "Attempted approval"},
                "feedback": "Rejected audit response"}
    state["history"].append(rejected)
    assert "approved_setup_reopening" not in role_context(state, {"role": "audit"})
    rejected["response"]["decision"] = "revise"
    rejected["feedback"] = rejected["response"]["reason"]
    assert "approved_setup_reopening" not in role_context(state, {"role": "audit"})


def test_stage_cache_tracks_source_flags_and_library_dependencies(tmp_path):
    root = tmp_path / "run"
    (root / "work/crate").mkdir(parents=True)
    (root / "upstream").mkdir()
    (root / "work/crate/lib.rs").write_text("source")
    write_json(root / "work/extraction.json", {"crate": "crate"})
    binary = tmp_path / "bin"
    binary.mkdir()
    script = '''#!/usr/bin/env python3
import hashlib, json, pathlib, sys
name=pathlib.Path(sys.argv[0]).name
if name=='charon':
 pathlib.Path(sys.argv[sys.argv.index('--dest-file')+1]).write_bytes(pathlib.Path('lib.rs').read_bytes())
elif name=='aeneas':
 destination=pathlib.Path(sys.argv[sys.argv.index('-dest')+1])
 value=int.from_bytes(hashlib.sha256(pathlib.Path(sys.argv[-1]).read_bytes()).digest()[:2])
 (destination/'M.lean').write_text(f'def generated := {value}\\n')
elif name=='lean':
 if '--run' in sys.argv:
  pathlib.Path(sys.argv[-1]).write_text(json.dumps({'schema':'trellis-preparation-axioms/v1','module':sys.argv[-2],'complete':True,'declarations':[],'error_lines':[]}))
 else:
  pathlib.Path(sys.argv[sys.argv.index('-o')+1]).write_bytes(pathlib.Path(sys.argv[-1]).read_bytes())
'''
    for name in ("cargo", "charon", "aeneas", "lean"):
        path = binary / name
        path.write_text(script); path.chmod(0o755)
    tools = {name: str(binary / name) for name in ("cargo", "charon", "aeneas", "lean")}
    library = tmp_path / "library"
    library.mkdir()
    (library / "Dependency.olean").write_bytes(b"first compiled dependency")
    tools["lean_paths"] = [str(library)]
    write_json(root / "config.json", {"tools": tools, "goals": [{"id": "g", "text": "goal"}]})
    first = observe(root, root / "observations/first")
    assert passed(first, "lean")
    second = observe(root, root / "observations/second")
    assert all(step["cached"] for step in second["steps"])
    from trellis.pv_toolchain import setup_basis
    basis = setup_basis(root)
    tools.update(timeout=7200, timeouts={"aeneas": 14400})
    write_json(root / "config.json", {"tools": tools, "goals": [{"id": "g", "text": "goal"}]})
    assert setup_basis(root) == basis
    more_time = observe(root, root / "observations/more-time")
    assert all(step["cached"] for step in more_time["steps"])
    assert more_time["environment"] == second["environment"]
    assert more_time["steps"][2]["timeout_seconds"] == 14400
    (root / "work/crate/lib.rs").write_text("changed source")
    fourth = observe(root, root / "observations/fourth")
    assert all(not step["cached"] for step in fourth["steps"])
    write_json(root / "work/extraction.json", {"crate": "crate", "aeneas_args": ["-split-files"]})
    fifth = observe(root, root / "observations/fifth")
    assert [step["cached"] for step in fifth["steps"][:3]] == [True, True, False]
    (library / "Dependency.olean").write_bytes(b"changed compiled dependency")
    sixth = observe(root, root / "observations/sixth")
    assert all(not step["cached"] for step in sixth["steps"])
    probe = tmp_path / "scratch-crate"
    probe.mkdir()
    (probe / "lib.rs").write_text("a separate probe")
    result = probe_source(root, probe, root / ".trellis/scratch/probe")
    assert passed(result, "lean")
    assert (root / "work/crate/lib.rs").read_text() == "changed source"
    assert (root / ".trellis/scratch/probe/observation/source/lib.rs").read_text() == "a separate probe"
    alternate = tmp_path / "alternate-aeneas"
    alternate.write_text("#!/usr/bin/env python3\nprint('-emit-json')\n")
    alternate.chmod(0o755)
    tools["profiles"] = {"alternate": {"aeneas": str(alternate)}}
    write_json(root / "config.json", {"tools": tools})
    write_json(root / "work/extraction.json", {"crate": "crate", "resource_profile": "alternate"})
    selected = observe(root, root / "observations/selected", through="cargo")
    assert selected["tool_identities"]["aeneas"]["path"] == str(alternate)
    assert selected["tool_identities"]["aeneas"]["emit_json"]


def test_authored_support_binding_preserves_raw_model_and_tracks_definition(tmp_path):
    from scripts.extract_pv_model import split_generated_lean
    from trellis.pv_model_support import MANIFEST_PATH, boundary_inventory, empty_model_support, sync_model_support

    raw = ("import Aeneas\nnamespace demo\n/-- [core::identity] -/\n"
           "axiom identity {T : Type} (x : T) : T\n/-- [demo::run] -/\n"
           "def run (x : Nat) : Nat := identity x\nend demo\n")
    (tmp_path / "generated.lean").write_text(raw)
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    (tablet / "Identity.lean").write_text("import Aeneas\n-- [TABLET NODE: Identity]\n"
                                         "def Identity {T : Type} (x : T) : T :=\n-- BODY\n  x\n")
    (tablet / "Identity.tex").write_text(r"\begin{definition}Identity returns its input.\end{definition}")
    before = split_generated_lean(raw)
    for node in before.nodes:
        (tablet / f"{node.name}.lean").write_text(node.text)
    identity = boundary_inventory(raw)[0]
    binding = {key: identity[key] for key in ("name", "extracted_signature", "rust_symbol", "raw_declaration_sha256")}
    binding.update(node="Identity", declaration="Identity")
    manifest = empty_model_support()
    manifest["bindings"] = [binding]
    write_json(tmp_path / MANIFEST_PATH, manifest)
    result = sync_model_support(tmp_path, write=True)
    assert result["matches"] and result["changed"]
    assert "def identity : ∀ {T : Type} (x : T), T := _root_.Identity" in (tablet / "Preamble.lean").read_text()
    assert (tablet / "Preamble.lean").read_text().count("set_option autoImplicit false") == 1
    assert result["evidence"]["bindings"][0]["classification"] == "authored-model-binding"
    assert sync_model_support(tmp_path)["matches"]
    for node in before.nodes:
        assert (tablet / f"{node.name}.lean").read_text() == node.text
        assert "set_option autoImplicit false" in node.text.split("-- [TABLET NODE:", 1)[0]
        assert "set_option autoImplicit" not in node.region
    assert (tmp_path / "generated.lean").read_text() == raw
    (tablet / "Identity.tex").write_text(r"\begin{definition}The identity function on every type.\end{definition}")
    changed = sync_model_support(tmp_path)
    assert changed["matches"]
    assert changed["evidence"]["definitions"] != result["evidence"]["definitions"]
    (tablet / "Preamble.lean").write_text("def forged := 0\n")
    assert not sync_model_support(tmp_path)["matches"]
    for field, value in (("extracted_signature", "(T : Type) (x : T) : T"),
                         ("rust_symbol", "different::identity"), ("raw_declaration_sha256", "0" * 64),
                         ("name", "run"), ("declaration", "someOtherDefinition")):
        malformed = {**manifest, "bindings": [{**binding, field: value}]}
        with pytest.raises(ValueError):
            sync_model_support(tmp_path, manifest=malformed, write=True)


def test_foundational_support_requires_paired_nodes_and_acyclic_imports(tmp_path):
    from trellis.pv_model_support import compose_model_support, empty_model_support

    raw = "import Aeneas\nnamespace demo\n/-- [demo::entry] -/\ndef entry : Carrier := default\nend demo\n"
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    lean = "import Aeneas\n-- [TABLET NODE: Carrier]\ndef Carrier :=\n-- BODY\n  Nat\n"
    (tablet / "Carrier.lean").write_text(lean)
    manifest = {**empty_model_support(), "imports": ["Carrier"]}
    with pytest.raises(ValueError, match="ordinary paired definition"):
        compose_model_support(raw, manifest, repo=tmp_path)
    (tablet / "Carrier.tex").write_text(r"\begin{definition}Carrier is the natural numbers.\end{definition}")
    result = compose_model_support(raw, manifest, repo=tmp_path)
    assert result["preamble"].startswith("import Tablet.Carrier\n")
    (tablet / "Carrier.lean").write_text("import Tablet.Preamble\n" + lean)
    with pytest.raises(ValueError, match="cycle"):
        compose_model_support(raw, manifest, repo=tmp_path)


def test_support_binding_uses_pinned_extracted_types(tmp_path):
    from scripts.extract_pv_model import split_generated_lean
    from trellis.pv_model_support import TYPE_LAYOUT, boundary_inventory, empty_model_support, sync_model_support

    raw = ("import Aeneas\nnamespace demo\n/-- [demo::Packet] -/\n"
           "structure Packet where\n  payload : Carrier\n  count : Nat\n"
           "/-- [demo::inspect] -/\naxiom inspect (p : Packet) : Nat\n"
           "/-- [demo::run] -/\ndef run (p : Packet) : Nat := inspect p\nend demo\n")
    (tmp_path / "generated.lean").write_text(raw)
    tablet = tmp_path / "Tablet"
    tablet.mkdir()
    for name, lean in {
        "Carrier": "import Aeneas\nnamespace demo\n-- [TABLET NODE: Carrier]\nabbrev Carrier :=\n-- BODY\n  Nat\n",
        "Inspect": "import Tablet.Packet\nnamespace demo\n-- [TABLET NODE: Inspect]\ndef Inspect (p : Packet) : Nat :=\n-- BODY\n  p.count\n",
    }.items():
        (tablet / f"{name}.lean").write_text(lean)
        (tablet / f"{name}.tex").write_text(r"\begin{definition}" + name + r"\end{definition}")
    manifest = empty_model_support()
    identity = boundary_inventory(raw)[0]
    manifest["imports"] = ["Carrier"]
    manifest["bindings"] = [{**{k: identity[k] for k in
        ("name", "extracted_signature", "rust_symbol", "raw_declaration_sha256")},
        "node": "Inspect", "declaration": "demo.Inspect"}]
    write_json(tablet / "model-support.json", manifest)
    write_json(tmp_path / "modeling-input/manifest.json", {"model_support_layout": TYPE_LAYOUT})
    result = sync_model_support(tmp_path, write=True)
    assert result["matches"]
    assert "import Tablet.Carrier" in (tablet / "Packet.lean").read_text()
    assert "import Tablet.Preamble" not in (tablet / "Packet.lean").read_text()
    assert "import Tablet.Inspect" not in (tablet / "Packet.lean").read_text()
    extracted = next(item for item in result["evidence"]["definitions"] if item["node"] == "Packet")
    assert extracted["classification"] == "extraction_model"
    assert not (tablet / "Packet.tex").exists()
    assert sync_model_support(tmp_path)["matches"]
    # Declaration identity is independent of its packaging; tampering is caught.
    old = split_generated_lean(raw)
    new = split_generated_lean(raw, expose_types=True)
    assert old.nodes[0].region == next(n.region for n in new.nodes if n.name == "run")
    (tablet / "Packet.lean").write_text((tablet / "Packet.lean").read_text().replace("count : Nat", "count : Int"))
    assert not sync_model_support(tmp_path)["matches"]
    assert (tmp_path / "generated.lean").read_text() == raw


def test_compose_native_large_receipt_preserves_results_and_check_status(tmp_path, capsys):
    from scripts.pv_modeling import _main
    from trellis.pv_model_support import TYPE_LAYOUT

    # Named module hashes cannot fit a bounded preview merely by shortening
    # arrays or strings. Exercise real composition, including the --check gate.
    raw = "import Aeneas\nnamespace demo\n" + "".join(
        f"/-- [demo::operation_{i}] -/\ndef operation_{i} : Nat := {i}\n"
        for i in range(300)) + "end demo\n"
    (tmp_path / "generated.lean").write_text(raw)
    (tmp_path / "Tablet").mkdir()
    write_json(tmp_path / "modeling-input/manifest.json", {"model_support_layout": TYPE_LAYOUT})

    def run(*args):
        status = _main(["compose-native", str(tmp_path), *args])
        output = capsys.readouterr().out
        assert len(output) <= 16001
        preview = json.loads(output)
        full = json.loads(Path(preview["full_result"]).read_text())
        assert len(full["evidence"]["extracted_modules"]) == 300
        assert preview["evidence"]["extracted_module_count"] == 300
        assert "extracted_modules" not in preview["evidence"]
        assert preview["matches"] == full["matches"]
        return status, full

    status, result = run()
    assert status == 0 and result["matches"] and result["changed"]
    status, result = run("--check")
    assert status == 0 and result["matches"] and not result["changed"]
    path = tmp_path / "Tablet/operation_0.lean"
    path.write_text(path.read_text() + "-- changed after composition\n")
    modified = path.read_text()
    status, result = run("--check")
    assert status == 1 and not result["matches"]
    assert result["mismatched_files"] == ["Tablet/operation_0.lean"]
    assert path.read_text() == modified  # Checking must not repair the mismatch.
    assert (tmp_path / "generated.lean").read_text() == raw


def test_check_native_large_receipt_keeps_diagnostics_bounded(tmp_path, monkeypatch, capsys):
    from scripts.pv_modeling import _main
    import trellis.modeling.native as native
    result = {"passed": True, "checks": {"composition": True}, "report": str(tmp_path / "full.json"),
              "model_support": {"matches": True, "changed": False, "evidence": {
                  "extracted_modules": {f"module_{i}": "x" * 64 for i in range(300)}}}}
    write_json(tmp_path / "full.json", result)
    monkeypatch.setattr(native, "check_provenance", lambda *args, **kwargs: result)
    assert _main(["check-native", str(tmp_path)]) == 0
    preview = json.loads(capsys.readouterr().out)
    assert preview["passed"] and preview["model_support"]["matches"]
    assert len(Path(preview["report"]).read_text()) > 16000
    assert len(result["model_support"]["evidence"]["extracted_modules"]) == 300


def test_native_export_keeps_mutual_recursion_in_one_module(tmp_path, monkeypatch):
    from trellis.modeling.native import validate_export

    raw = tmp_path / "M.lean"
    raw.write_text("import Aeneas\nnamespace demo\n/-- [demo::seed] -/\ndef seed := 0\nmutual\n"
                   "/-- [demo::even] -/\n"
                   "def even (n : Nat) : Bool := match n with\n| 0 => true\n| n + 1 => odd n\n"
                   "/-- [demo::odd] -/\n"
                   "def odd (n : Nat) : Bool := match n with\n| 0 => false\n| n + 1 => even n\n"
                   "end\nend demo\n")
    # This test isolates packaging. Real axiom evidence is exercised separately.
    monkeypatch.setattr("trellis.modeling.axioms.compatibility", lambda *a, **kw: {"unapproved_axioms": []})
    result = validate_export({"raw_models": {"M": str(raw)}}, tmp_path / "export")
    assert result["passed"]
    report = json.loads(Path(result["report"]).read_text())
    assert {node["name"] for node in report["nodes"]} == {"seed", "even"}
    assert all(node["deps"] == [] for node in report["nodes"])


@pytest.mark.parametrize("change", ["macro", "import", "namespace", "option", "suffix", "missing"])
def test_native_composition_checks_whole_generated_modules(tmp_path, change):
    from trellis.pv_model_support import TYPE_LAYOUT, sync_model_support

    raw = ("import Aeneas\nnamespace demo\n/-- [demo::entry] -/\n"
           "def entry : Nat := 1\nend demo\n")
    (tmp_path / "generated.lean").write_text(raw)
    (tmp_path / "Tablet").mkdir()
    write_json(tmp_path / "modeling-input/manifest.json", {"model_support_layout": TYPE_LAYOUT})
    assert sync_model_support(tmp_path, write=True)["matches"]
    path = tmp_path / "Tablet/entry.lean"
    original = path.read_text()
    marker = original.index("-- [TABLET NODE:")
    edits = {
        "macro": original[:marker] + 'local macro "Nat" : term => `(Int)\n' + original[marker:],
        "import": "import Extra\n" + original,
        "namespace": original.replace("namespace demo", "namespace changed"),
        "option": original.replace("set_option autoImplicit false", "set_option autoImplicit true"),
        "suffix": original + "\naxiom injected : False\n",
    }
    if change == "missing":
        path.unlink()
    else:
        path.write_text(edits[change])
    checked = sync_model_support(tmp_path)
    assert not checked["matches"] and checked["mismatched_files"] == ["Tablet/entry.lean"]
    assert "entry" in checked["evidence"]["extracted_modules"]
    assert sync_model_support(tmp_path, write=True)["matches"]
    assert path.read_text() == original and (tmp_path / "generated.lean").read_text() == raw
    assert sync_model_support(tmp_path)["matches"]


@pytest.mark.parametrize("command", ["exec", "compare"])
def test_diagnostic_commands_forward_separator_and_literal_arguments(tmp_path, command, capsys):
    from scripts.pv_modeling import _main
    root = tmp_path / "run"
    root.mkdir()
    write_json(root / "config.json", {"tools": {}})
    write_json(root / "work/extraction.json", {})
    probe = [sys.executable, "-c", "import json,sys; print(json.dumps(sys.argv[1:]))", "--", "--offline", "a b", "$(literal)"]
    argv = [command, str(root)]
    if command == "exec":
        argv += ["--cwd", str(root)]
    else:
        source = tmp_path / "source"
        source.mkdir()
        argv += ["--baseline", str(source), "--candidate", str(source)]
        probe[-1] += "{source}"
    assert _main([*argv, "--command", *probe]) == (0 if command == "exec" else 1)
    result = json.loads(capsys.readouterr().out)
    if command == "compare":
        saved = json.loads(Path(result["comparison_file"]).read_text())
        for side in ("baseline", "candidate"):
            assert "environment" not in result[side]
            assert "PATH" in saved[side]["environment"]
        result = saved["baseline"]
    else:
        assert "environment" not in result
        assert "PATH" in json.loads(Path(result["record"]).read_text())["environment"]
    observed = json.loads(Path(result["stdout"]).read_text())
    assert observed[:3] == ["--", "--offline", "a b"]
    assert observed[3].startswith("$(literal)")
    if command == "exec":
        assert result["stdout_preview"] == Path(result["stdout"]).read_text()
        assert result["stderr_preview"] == ""
        output = "begin\n" + "\u03bb\U0001f33f" * 10000 + "\nfinal diagnostic\n"
        emit = "import sys; sys.stdout.write(sys.argv[1]); sys.stderr.write(sys.argv[1]); sys.exit(2)"
        assert _main([*argv, "--command", sys.executable, "-c", emit, output]) == 1
        result = json.loads(capsys.readouterr().out)
        for field in ("stdout", "stderr"):
            preview = result[field + "_preview"]
            assert preview.startswith("begin\n") and preview.endswith("\nfinal diagnostic\n")
            assert len(preview) <= 2000 and "\ufffd" not in preview
            beginning, rest = preview.split("\n... [", 1)
            count, ending = rest.split(" characters omitted] ...\n", 1)
            assert len(beginning) + int(count) + len(ending) == len(output)
            assert Path(result[field]).read_text() == output


def test_transcript_directory_pages_without_reading_records_and_bounds_argument_errors(tmp_path, capsys):
    from trellis.atomic_actions.cli import main
    from trellis.chat_history import inspect_transcript

    (tmp_path / "nested").mkdir()
    (tmp_path / "prompt.txt").write_text("Not a transcript candidate.")
    expected = {str(tmp_path / "nested")}
    for number in range(18):
        path = tmp_path / (f"{number:02d}-" + "\\" * 100 + ".log")
        path.write_text("SECRET RECORD CONTENT")
        expected.add(str(path))

    cursor, found = 0, set()
    while True:
        result = inspect_transcript(tmp_path, after_line=cursor, limit=50, max_chars=2000)
        encoded = json.dumps(result, ensure_ascii=False)
        assert len(encoded) <= 2000 and "SECRET RECORD CONTENT" not in encoded
        assert result["kind"] == "directory" and result["next_line"] > cursor
        assert all(entry["kind"] == "directory" or entry["bytes"] == 21 for entry in result["entries"])
        page = {entry["path"] for entry in result["entries"]}
        assert not found & page
        found.update(page)
        cursor = result["next_line"]
        if not result["has_more"]:
            break
    assert found == expected

    with pytest.raises(SystemExit) as error:
        main(["inspect-transcript", str(tmp_path), *["x" * 300] * 100])
    assert error.value.code == 2
    stderr = capsys.readouterr().err
    assert "pass one file or directory path" in stderr and len(stderr) < 3000


@pytest.mark.parametrize("kernel_failure", [False, True])
def test_fresh_native_bootstrap_installs_real_checker_before_kernel_init(tmp_path, monkeypatch, kernel_failure):
    import os
    import subprocess
    from trellis.modeling import native
    from trellis.pv_model_support import TYPE_LAYOUT, empty_model_support, sync_model_support

    repo, runtime = tmp_path / "native", tmp_path / "runtime"
    repo.mkdir()
    (repo / "generated.lean").write_text("import Aeneas\nnamespace demo\n/-- [demo::run] -/\ndef run := 1\nend demo\n")
    write_json(repo / "Tablet/model-support.json", empty_model_support())
    write_json(repo / "modeling-input/manifest.json", {"model_support_layout": TYPE_LAYOUT})
    sync_model_support(repo, write=True)
    assert not (repo / ".trellis/scripts/check.py").exists()
    original, selected = tmp_path / "original-kernel", tmp_path / "selected-kernel"
    original.write_text("old kernel bytes")
    selected.write_text("selected kernel bytes")
    # The kernel call is stubbed in this installation-order test; attestation
    # has separate tests against the compiled executable and query failures.
    monkeypatch.setattr("trellis.acceptance_identity.record_deployment", lambda *args: None)
    monkeypatch.setenv("TRELLIS_TRELLIS_KERNEL_CMD", str(original))
    calls = []

    def initialize(request):
        calls.append(request)
        assert request == {"action": "init_from_config", "root": str(runtime),
                           "config_path": str(repo / "trellis.config.json")}
        assert os.environ["TRELLIS_TRELLIS_KERNEL_CMD"] == str(selected)
        assert (repo / ".trellis/runtime/bin/trellis_runtime_cli").read_bytes() == selected.read_bytes()
        # Exercise exactly the repo-local subprocess contract used by the
        # kernel during init; there has not yet been an agent/checker setup.
        result = subprocess.run([sys.executable, str(repo / ".trellis/scripts/check.py"),
                                 "model-support-check", str(repo)], cwd=tmp_path,
                                capture_output=True, text=True)
        assert result.returncode == 0, result.stderr + result.stdout
        assert json.loads(result.stdout) == {"ok": True, "errors": []}
        generated = repo / "Tablet/run.lean"
        original = generated.read_text()
        generated.write_text(original.replace("-- [TABLET NODE:", 'local macro "Nat" : term => `(Int)\n-- [TABLET NODE:'))
        rejected = subprocess.run([sys.executable, str(repo / ".trellis/scripts/check.py"),
                                   "model-support-check", str(repo)], cwd=tmp_path,
                                  capture_output=True, text=True)
        assert rejected.returncode == 1
        assert "Tablet/run.lean" in json.loads(rejected.stdout)["errors"][0]
        generated.write_text(original)
        if kernel_failure:
            raise ValueError("initialization failed")
        return {"status": "ok"}

    monkeypatch.setattr(native, "run_kernel_cli", initialize)
    if kernel_failure:
        with pytest.raises(ValueError, match="initialization failed"):
            native.initialize_native_runtime(repo, runtime, str(selected))
    else:
        assert native.initialize_native_runtime(repo, runtime, str(selected))["status"] == "ok"
    assert len(calls) == 1
    assert os.environ["TRELLIS_TRELLIS_KERNEL_CMD"] == str(original)


@pytest.mark.parametrize("boundary", [None, "unowned", "paths", "initialized", "symlink"])
def test_failed_bootstrap_preserves_evidence_and_refuses_unrelated_output(tmp_path, boundary):
    from trellis.modeling.native import export_directory, preserve_failed_bootstrap
    root, repo, runtime = (tmp_path / name for name in ("prep", "native", "runtime"))
    repo.mkdir()
    (repo / "generated.lean").write_text("original extraction")
    runtime.mkdir()
    (runtime / "diagnostic.txt").write_text("original runtime evidence")
    staging = export_directory(root, repo)
    evidence = staging / "bootstrap-error.json"
    write_json(evidence, {"repo": str(repo), "runtime": str(runtime), "error": "missing checker"})
    write_json(root / ".trellis/modeling/state.json", {"host_failure": {
        "operation": "native_bootstrap", "evidence": str(evidence)}})
    if boundary == "unowned":
        write_json(root / ".trellis/modeling/state.json", {})
    elif boundary == "paths":
        write_json(evidence, {"repo": str(tmp_path / "unrelated"), "runtime": str(runtime)})
    elif boundary == "initialized":
        write_json(runtime / "protocol_state.json", {"phase": "Modeling"})
    elif boundary == "symlink":
        actual = repo.with_name("original")
        repo.rename(actual)
        repo.symlink_to(actual)
    if boundary:
        with pytest.raises((ValueError, FileNotFoundError)):
            preserve_failed_bootstrap(root, repo, runtime)
        assert (repo / "generated.lean").read_text() == "original extraction"
        assert (runtime / "diagnostic.txt").exists()
        assert evidence.exists()
        return
    archive = preserve_failed_bootstrap(root, repo, runtime)
    assert (archive / "bootstrap-error.json").is_file()
    assert (archive / "partial-repository/generated.lean").read_text() == "original extraction"
    assert (archive / "partial-runtime/diagnostic.txt").read_text() == "original runtime evidence"
    assert json.loads((archive / "preserved.json").read_text())["repo"] == str(repo)
    assert not any(path.exists() for path in (staging, repo, runtime))
    assert preserve_failed_bootstrap(root, repo, runtime) is None

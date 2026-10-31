import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile

import pytest

from scripts import pv_native_create as create
from trellis.config import load_config
from trellis.modeling.driver import initialize, preparation_provider
from trellis.modeling.goals import read_goal_collection, verify_inventory
from trellis.modeling.native import launch_template


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value) + "\n")


def archive(path, files):
    with tarfile.open(path, "w:gz", format=tarfile.USTAR_FORMAT) as tar:
        for name, content in files.items():
            data = content.encode()
            member = tarfile.TarInfo(name)
            member.size = len(data)
            tar.addfile(member, io.BytesIO(data))


@pytest.fixture
def job(tmp_path, monkeypatch):
    folder = tmp_path / "projects/.trellis-viewer/create-jobs/parser"
    folder.mkdir(parents=True)
    write_json(folder / "job.json", {"slug": "parser", "goal_format": "native",
                                     "repo": str(tmp_path / "projects/parser"),
                                     "runtime_root": str(tmp_path / "projects/parser-runtime")})
    write_json(folder / "GOAL.json", {"context": "The subject is f.", "goals": [
        {"id": "acceptance", "text": "f accepts all valid inputs."},
        {"id": "correctness", "text": "f returns the intended result."}]})
    write_json(folder / "native-tools.json", {})
    template = json.loads((create.ROOT / "examples/trellis.native.pv.config.json").read_text())
    template["worker"]["model"] = "runtime-worker"
    template["reviewer"]["model"] = "runtime-reviewer"
    for role in template["preparation"]:
        template["preparation"][role]["model"] = "prepare-" + role
    write_json(folder / "config-template.json", template)
    archive(folder / "target.tar.gz", {"crate/lib.rs": "pub fn f() {}\n"})
    archive(folder / "support.tar.gz", {"support/traits.rs": "pub trait T {}\n"})
    # Tool installations and paid agents are external boundaries. Source intake,
    # real initialization, settings, the generated goal view, and aliases are real.
    monkeypatch.setattr(create, "validate_inputs", lambda goals, tools: read_goal_collection(goals)[1])
    return folder


def fake_modeling(monkeypatch, job, *, halted=False, wrong_inventory=False):
    real_run = subprocess.run
    calls = []

    def run(argv, **kwargs):
        if len(argv) < 3 or argv[1] != str(create.ROOT / "scripts/pv_modeling.py"):
            return real_run(argv, **kwargs)
        calls.append(argv)
        root = Path(argv[3])
        if argv[2] == "init":
            def value(flag):
                return Path(argv[argv.index(flag) + 1])
            initialize(root, value("--source"), value("--goals"), {}, model="fallback", effort="high",
                       config_template=value("--config-template"))
        elif argv[2] == "run":
            assert "--no-viewer" in argv
            info = json.loads((job / "job.json").read_text())
            assert Path(info["repo"]).resolve() == root
            native = Path(argv[argv.index("--repo") + 1])
            runtime = Path(argv[argv.index("--runtime") + 1])
            state = json.loads((root / ".trellis/modeling/state.json").read_text())
            state.update(status="halted" if halted else "prepared", native_repo=str(native), native_runtime=str(runtime))
            write_json(root / ".trellis/modeling/state.json", state)
            if not halted:
                config = launch_template(json.loads((root / "config.json").read_text()))
                targets = ["acceptance"] if wrong_inventory else ["acceptance", "correctness"]
                config.update(repo_path=str(native), pv_tablet={"verification_targets": targets})
                write_json(native / "trellis.config.json", config)
                write_json(native / "pv_tablet.json", config["pv_tablet"])
                (native / "GOAL.md").write_bytes((root / "GOAL.md").read_bytes())
                write_json(runtime / "protocol_state.json", {"phase": "Modeling"})
        else:
            assert argv[2] == "stop"
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setattr(create.subprocess, "run", run)
    return calls


def test_native_handoff_keeps_source_goals_and_selected_roles(job, monkeypatch):
    calls = fake_modeling(monkeypatch, job)
    create.run_job(job)
    prep = job / "native/parser-preparation"
    native = job / "native/parser"
    alias = Path(json.loads((job / "job.json").read_text())["repo"])
    assert alias.resolve() == native
    assert not (prep / ".trellis-creating").exists()
    assert (prep / "goals.json").read_bytes() == (job / "GOAL.json").read_bytes()
    assert (prep / "upstream/lib.rs").read_text() == "pub fn f() {}\n"
    assert (prep / "upstream/traits.rs").read_text() == "pub trait T {}\n"
    assert not (prep / "upstream/Cargo.toml").exists()
    assert json.loads((job / "goal-inventory.json").read_text())["registered"] == ["acceptance", "correctness"]
    config = json.loads((native / "trellis.config.json").read_text())
    assert config["worker"]["model"] == "runtime-worker"
    assert config["reviewer"]["model"] == "runtime-reviewer"
    assert config["system_feedback_halt"] is True
    assert "preparation" not in config
    inputs = json.loads((prep / "config.json").read_text())
    loaded = load_config(prep / "trellis.config.json")
    for role, selected in [("worker", "worker"), ("reviewer", "reviewer"), ("audit", "audit"),
                           ("planner", "reviewer"), ("critic", "audit"), ("route_audit", "audit")]:
        assert preparation_provider(loaded, inputs, role).model == "prepare-" + selected
    assert [call[2] for call in calls] == ["init", "run"]
    with pytest.raises(ValueError, match="fresh"):
        create.run_job(job)


def test_all_sol_defaults_survive_preparation_and_native_handoff(job, monkeypatch):
    template = json.loads((create.ROOT / "examples/trellis.native.pv.config.json").read_text())
    write_json(job / "config-template.json", template)
    fake_modeling(monkeypatch, job)
    create.run_job(job)
    prep = job / "native/parser-preparation"
    inputs = json.loads((prep / "config.json").read_text())
    loaded = load_config(prep / "trellis.config.json")
    for role in ("worker", "reviewer", "audit", "planner", "critic", "route_audit"):
        selected = preparation_provider(loaded, inputs, role)
        assert (selected.model, selected.effort) == ("gpt-6.1-sol", "xhigh")
    native = json.loads((job / "native/parser/trellis.config.json").read_text())
    assert native["phase0"]["allow_same_model_lanes"] is True
    for role in ("worker", "easy_worker", "hard_worker", "reviewer", "stuck_math_audit"):
        assert (native[role]["model"], native[role]["effort"]) == ("gpt-6.1-sol", "xhigh")
    for lane in native["verification"].values():
        for selected in lane:
            assert (selected["model"], selected["effort"]) == ("gpt-6.1-sol", "xhigh")


@pytest.mark.parametrize('provider,model', [('deepseek', 'deepseek-v4-pro'), ('kimi', 'kimi-k3')])
def test_api_roles_survive_preparation_and_native_handoff(provider, model, job, monkeypatch):
    from trellis.provider_overrides import apply_binding
    template = json.loads((create.ROOT / 'examples/trellis.native.pv.config.json').read_text())
    def apply(value):
        if isinstance(value, dict):
            if 'provider' in value and 'model' in value:
                apply_binding(value, {'provider': provider, 'base_url': 'https://api.example.test/v1'}, template)
            for child in list(value.values()):
                apply(child)
        elif isinstance(value, list):
            for child in value:
                apply(child)
    apply(template)
    write_json(job / 'config-template.json', template)
    fake_modeling(monkeypatch, job)
    create.run_job(job)
    prep = job / 'native/parser-preparation'
    inputs = json.loads((prep / 'config.json').read_text())
    loaded = load_config(prep / 'trellis.config.json')
    for role in ('worker', 'reviewer', 'audit', 'planner', 'critic', 'route_audit'):
        selected = preparation_provider(loaded, inputs, role)
        assert (selected.provider, selected.model, selected.effort) == (provider, model, 'max')
    native = json.loads((job / 'native/parser/trellis.config.json').read_text())
    assert native['api_providers'][provider]['base_url'] == 'https://api.example.test/v1'
    for role in ('worker', 'easy_worker', 'hard_worker', 'reviewer', 'stuck_math_audit'):
        assert (native[role]['provider'], native[role]['model'], native[role]['effort']) == (provider, model, 'max')
    for lane in native['verification'].values():
        for selected in lane:
            assert (selected['provider'], selected['model'], selected['effort']) == (provider, model, 'max')


@pytest.mark.parametrize("halted,wrong_inventory", [(True, False), (False, True)])
def test_unsuccessful_handoff_keeps_preparation_visible(job, monkeypatch, halted, wrong_inventory):
    fake_modeling(monkeypatch, job, halted=halted, wrong_inventory=wrong_inventory)
    with pytest.raises(ValueError, match="did not launch|inventory differs"):
        create.run_job(job)
    alias = Path(json.loads((job / "job.json").read_text())["repo"])
    assert alias.resolve() == job / "native/parser-preparation"
    assert not (job / "goal-inventory.json").exists()


def test_source_overlap_cannot_reach_initialization(job, monkeypatch):
    calls = fake_modeling(monkeypatch, job)
    (job / "support.tar.gz").unlink()
    archive(job / "support.tar.gz", {"support/lib.rs": "different bytes\n"})
    with pytest.raises(SystemExit, match="overlap"):
        create.run_job(job)
    assert not calls


def test_stop_addresses_only_this_preparation(job, monkeypatch):
    prep = job / "native/parser-preparation"
    write_json(prep / ".trellis/modeling/state.json", {})
    calls = fake_modeling(monkeypatch, job)
    create.stop_job(job)
    assert len(calls) == 1
    assert calls[0][2:] == ["stop", str(prep)]


def test_inventory_accounts_for_declines_and_rejects_changed_meaning(tmp_path):
    prep, native = tmp_path / "prep", tmp_path / "native"
    write_json(prep / "goals.json", {"goals": [{"id": "a", "text": "A"}, {"id": "b", "text": "B"}]})
    write_json(prep / ".trellis/modeling/state.json", {"declined": {"b": "audited decline"}})
    write_json(native / "pv_tablet.json", {"verification_targets": ["a"]})
    write_json(native / "trellis.config.json", {"pv_tablet": {"verification_targets": ["a"]}})
    for p in (prep, native):
        (p / "GOAL.md").write_text("Original meaning\n")
    result = verify_inventory(prep, native)
    assert result == {"supplied": ["a", "b"], "registered": ["a"], "declined": {"b": "audited decline"}}
    (native / "GOAL.md").write_text("Changed meaning\n")
    with pytest.raises(ValueError, match="changed"):
        verify_inventory(prep, native)
    write_json(prep / ".trellis/modeling/state.json", {"declined": {"other": "invalid"}})
    with pytest.raises(ValueError, match="outside"):
        verify_inventory(prep, native)


def test_alias_handoff_does_not_replace_an_unrelated_project(tmp_path):
    original, unrelated, new = (tmp_path / name for name in ("original", "unrelated", "new"))
    for p in (original, unrelated, new):
        p.mkdir()
    alias = tmp_path / "parser"
    alias.symlink_to(unrelated)
    with pytest.raises(ValueError, match="changed"):
        create.publish_alias(alias, new, previous=original)
    assert alias.resolve() == unrelated


def interrupted_preparation(job, monkeypatch):
    fake_modeling(monkeypatch, job, halted=True)
    with pytest.raises(ValueError, match="did not launch"):
        create.run_job(job)
    root = job / "native/parser-preparation"
    state_path = root / ".trellis/modeling/state.json"
    state = json.loads(state_path.read_text())
    state.update(resume_status="prepared", audits={"setup": {"basis": "retained"}}, sequence=16)
    write_json(state_path, state)
    return root


def test_native_startup_requires_dispatch_and_reports_fatal_error(job):
    runtime = Path(json.loads((job / "job.json").read_text())["runtime_root"])
    assert not create.startup_ready(job)
    write_json(runtime / "protocol_state.json", {"phase": "Modeling", "cycle": 0})
    assert not create.startup_ready(job)
    write_json(runtime / "protocol_state.json", {"phase": "Modeling", "cycle": 1,
                                                "in_flight_request": {"kind": "Review"}})
    assert create.startup_ready(job)
    write_json(runtime / "runtime_error_halt.json", {"error": "model startup failed"})
    with pytest.raises(ValueError, match="model startup failed"):
        create.startup_ready(job)


def test_completed_revision_handoff_does_not_wait_for_another_dispatch(job):
    runtime = Path(json.loads((job / "job.json").read_text())["runtime_root"])
    write_json(runtime / "protocol_state.json", {"phase": "ProofFormalization", "cycle": 20,
        "in_flight_request": None, "pv_revision_handoff": {"schema_version": 1, "cycle": 20}})
    assert create.startup_ready(job)
    write_json(runtime / "runtime_error_halt.json", {"error": "real failure"})
    with pytest.raises(ValueError, match="real failure"):
        create.startup_ready(job)


def test_native_parallel_startup_recognizes_adopted_worker(job):
    runtime = Path(json.loads((job / "job.json").read_text())["runtime_root"])
    state = {"phase": "Modeling", "cycle": 2, "in_flight_request": None,
             "dispatch": {"tasks": {}}}
    write_json(runtime / "protocol_state.json", state)
    assert not create.startup_ready(job)
    state['dispatch']['tasks']['1'] = {'status': 'Reserved'}
    write_json(runtime / "protocol_state.json", state)
    assert create.startup_ready(job)


@pytest.mark.parametrize("outcome", ["slow_ready", "fatal", "exited"])
def test_native_creation_waits_for_live_startup_and_reports_real_failures(job, tmp_path, outcome):
    # Exercise the real shell status/heartbeat loop. Only the expensive native
    # launcher and tmux process lookup are replaced. Three startup probes cross
    # the old timeout while the supervisor is still alive.
    stubs = tmp_path / "stubs"
    stubs.mkdir()
    native = stubs / "native.py"
    native.write_text('''import os, sys
from pathlib import Path
job = Path(sys.argv[2])
if sys.argv[1] == "resume":
    (job / "probes").write_text("0")
else:
    count = int((job / "probes").read_text()) + 1
    (job / "probes").write_text(str(count))
    if os.environ["STARTUP_OUTCOME"] == "fatal":
        sys.exit(1)
    sys.exit(0 if count >= 3 else 2)
''')
    tmux = stubs / "tmux"
    tmux.write_text('''#!/usr/bin/env python3
import os
from pathlib import Path
count = int((Path(os.environ["STARTUP_JOB"]) / "probes").read_text())
if os.environ["STARTUP_OUTCOME"] != "exited" or count < 2:
    print("trellis-run-parser")
''')
    tmux.chmod(0o755)
    sleeper = stubs / "sleep"
    sleeper.write_text('#!/usr/bin/env python3\nimport time\ntime.sleep(0.05)\n')
    sleeper.chmod(0o755)
    env = {**os.environ, "PATH": str(stubs) + os.pathsep + os.environ["PATH"],
           "TRELLIS_PROJECTS_ROOT": str(tmp_path / "projects"),
           "TRELLIS_PV_CREATE_NATIVE_CMD": f"{sys.executable} {native}",
           "TRELLIS_CREATE_VERIFY_SECS": "2", "STARTUP_OUTCOME": outcome,
           "STARTUP_JOB": str(job)}
    result = subprocess.run(["bash", str(create.ROOT / "scripts/pv_create_run.sh"),
                             "resume-run", "parser"], env=env,
                            capture_output=True, text=True, timeout=15)
    status = json.loads((job / "status.json").read_text())
    if outcome == "slow_ready":
        assert result.returncode == 0, result.stderr
        assert status["state"] == "done"
        assert "still initializing" in result.stdout
        assert int((job / "probes").read_text()) == 3
    else:
        assert result.returncode != 0
        assert status["state"] == "launch_failed"
        assert ("exited" if outcome == "exited" else "failed") in status["error"]


def test_resume_keeps_preparation_and_settings_without_reinitializing(job, monkeypatch):
    root = interrupted_preparation(job, monkeypatch)
    (root / "work/SETUP.md").write_text("Audited setup\n")
    calls = fake_modeling(monkeypatch, job)
    create.run_job(job, resume=True)
    assert [call[2] for call in calls] == ["run"]
    state = json.loads((root / ".trellis/modeling/state.json").read_text())
    assert state["audits"] == {"setup": {"basis": "retained"}}
    assert state["sequence"] == 16
    assert (root / "work/SETUP.md").read_text() == "Audited setup\n"
    assert Path(json.loads((job / "job.json").read_text())["repo"]).resolve() == job / "native/parser"


@pytest.mark.parametrize("boundary", ["halt", "settings", "goals", "alias", "runtime", "driver"])
def test_resume_respects_stop_and_ownership_boundaries(job, monkeypatch, boundary):
    import fcntl
    root = interrupted_preparation(job, monkeypatch)
    lock = None
    if boundary == "halt":
        write_json(root.with_name(root.name + "-runtime") / "system_feedback_halt.json", {})
    elif boundary == "settings":
        write_json(job / "config-template.json", {})
    elif boundary == "goals":
        (job / "GOAL.json").write_text('{}\n')
    elif boundary == "alias":
        Path(json.loads((job / "job.json").read_text())["repo"]).unlink()
    elif boundary == "runtime":
        runtime = Path(json.loads((job / "job.json").read_text())["runtime_root"])
        write_json(runtime / "protocol_state.json", {})
    else:
        lock = (root / ".trellis/modeling/runner.lock").open("a")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    calls = fake_modeling(monkeypatch, job)
    try:
        with pytest.raises((ValueError, RuntimeError)):
            create.run_job(job, resume=True)
        assert not calls
    finally:
        if lock:
            lock.close()


def reopening_parent(job, monkeypatch):
    """An accepted native boundary; tool/agent execution remains external."""
    import shutil
    from trellis.modeling.driver import source_fingerprint
    from trellis.modeling.observations import fingerprint
    fake_modeling(monkeypatch, job)
    create.run_job(job)
    info = json.loads((job / 'job.json').read_text())
    prep, repo, runtime = create.active_paths(job, info)
    shutil.copytree(prep / 'work/crate', repo / 'crate')
    write_json(repo / 'modeling-input/manifest.json', {
        'provenance_schema': 'trellis-native-preparation/v1',
        'preparation_root': str(prep), 'source_basis': source_fingerprint(prep)})
    write_json(runtime / 'runtime_metadata.json', {'repo_path': str(repo)})
    write_json(runtime / 'protocol_state.json', {
        'phase': 'Modeling', 'stage': 'Reviewer', 'cycle': 4, 'request_seq': 27,
        'in_flight_request': None, 'configured_targets': ['acceptance', 'correctness'],
        'modeling_request': {'action': 'reopen', 'reason': 'Approved interface repair'},
        'modeling_route_decisions': [{'request_id': 27,
            'resolution': {'action': 'approve', 'scope': 'Both supplied goals',
                           'tasks': [{'id': 'repair', 'title': 'Repair the interface', 'body': 'Compare alternatives'}],
                           'reopen_setup': True, 'declined_targets': []},
            'assessment': 'The narrower interface is adequate'}]})
    (repo / 'PriorProof.lean').write_text('Prior work stays here.\n')
    monkeypatch.setattr(create, 'startup_ready', lambda _: True)
    return prep, repo, runtime


def test_native_reopening_uses_existing_driver_and_fresh_bootstrap(job, monkeypatch):
    from trellis.modeling.reopening import reference_paths
    prep, repo, runtime = reopening_parent(job, monkeypatch)
    old_state = (runtime / 'protocol_state.json').read_bytes()
    calls = fake_modeling(monkeypatch, job)
    assert create.continue_native(runtime)
    info = json.loads((job / 'job.json').read_text())
    child, child_repo, child_runtime = create.active_paths(job, info)
    state = json.loads((child / '.trellis/modeling/state.json').read_text())
    # The fake tool finishes immediately; source/goal setup and the resume path are real.
    assert state['audits'] == {} and state['stage'] == 'setup'
    assert state['next']['role'] == 'reviewer'
    assert state['route_decisions'][-1]['resolution']['tasks'][0]['id'] == 'repair'
    assert state.get('history') == []
    assert (child / 'goals.json').read_bytes() == (job / 'GOAL.json').read_bytes()
    assert (child / 'work/crate/lib.rs').read_bytes() == (prep / 'work/crate/lib.rs').read_bytes()
    assert (child / 'upstream/lib.rs').read_bytes() == (prep / 'upstream/lib.rs').read_bytes()
    assert reference_paths(child) == [str(prep), str(repo), str(runtime)]
    assert not (child_repo / 'PriorProof.lean').exists()
    assert (repo / 'PriorProof.lean').read_text() == 'Prior work stays here.\n'
    assert (runtime / 'protocol_state.json').read_bytes() == old_state
    assert Path(info['repo']).resolve() == child_repo
    assert info['native_generation'] == 1 and not info['reopening']['pending']
    assert child_runtime != runtime
    assert [call[2] for call in calls] == ['run']
    # A delayed callback cannot create another generation or restart the new one.
    assert create.continue_native(runtime)
    assert len(calls) == 1


@pytest.mark.parametrize('point', ['initialize', 'publish', 'launch'])
def test_reopening_retry_keeps_one_generation_and_preserves_work(job, monkeypatch, point):
    prep, repo, runtime = reopening_parent(job, monkeypatch)
    original_init = create.initialize_reopened
    original_publish = create.publish_alias
    original_launch = create.launch_preparation
    def interrupted_init(*args):
        if point == 'initialize':
            root = args[0]
            root.mkdir()
            (root / 'partial-evidence').write_text('preserve')
            raise RuntimeError('interrupted initialization')
        original_init(*args)
    def interrupted_publish(*args, **kwargs):
        if point == 'publish':
            raise RuntimeError('interrupted publication')
        return original_publish(*args, **kwargs)
    def interrupted_launch(*args, **kwargs):
        if point == 'launch':
            (args[2] / 'work/SETUP.md').write_text('Worker progress survives retry')
            raise RuntimeError('interrupted launch')
        return original_launch(*args, **kwargs)
    monkeypatch.setattr(create, 'initialize_reopened', interrupted_init)
    monkeypatch.setattr(create, 'publish_alias', interrupted_publish)
    monkeypatch.setattr(create, 'launch_preparation', interrupted_launch)
    with pytest.raises(RuntimeError, match='interrupted'):
        create.continue_native(runtime)
    pending = json.loads((job / 'job.json').read_text())
    assert pending['native_generation'] == 1 and pending['reopening']['pending']
    monkeypatch.setattr(create, 'initialize_reopened', original_init)
    monkeypatch.setattr(create, 'publish_alias', original_publish)
    monkeypatch.setattr(create, 'launch_preparation', original_launch)
    create.run_job(job, resume=True)
    done = json.loads((job / 'job.json').read_text())
    assert done['native_generation'] == 1 and not done['reopening']['pending']
    child, _, _ = create.active_paths(job, done)
    if point == 'initialize':
        assert next(child.parent.glob('*-interrupted-init-*/partial-evidence')).read_text() == 'preserve'
    if point == 'launch':
        assert (child / 'work/SETUP.md').read_text() == 'Worker progress survives retry'


@pytest.mark.parametrize('change', ['proposal', 'math', 'fail', 'pause', 'source', 'goals', 'alias'])
def test_reopening_does_not_bypass_boundaries(job, monkeypatch, change):
    prep, repo, runtime = reopening_parent(job, monkeypatch)
    if change in {'proposal', 'math', 'fail'}:
        state = json.loads((runtime / 'protocol_state.json').read_text())
        if change == 'proposal':
            state.update(stage='ModelingAudit', in_flight_request={'kind': 'Audit'})
        elif change == 'math':
            state['phase'] = 'TheoremStating'
        else:
            state['modeling_request']['action'] = 'fail'
        write_json(runtime / 'protocol_state.json', state)
    elif change == 'pause':
        write_json(runtime / 'pause_request.json', {'reason': 'operator pause'})
    elif change == 'source':
        (prep / 'work/crate/lib.rs').write_text('changed')
    elif change == 'goals':
        (repo / 'GOAL.md').write_text('changed')
    else:
        Path(json.loads((job / 'job.json').read_text())['repo']).unlink()
    if change in {'source', 'goals', 'alias'}:
        with pytest.raises(ValueError):
            create.continue_native(runtime)
    else:
        assert not create.continue_native(runtime)
    # No new prepared child, nor alterations to the old native work.
    info = json.loads((job / 'job.json').read_text())
    assert not info.get('native_generation')
    assert (repo / 'PriorProof.lean').is_file()


@pytest.mark.parametrize('alive', [False, True])
def test_reopening_finishes_sealed_bootstrap_without_reextracting(job, monkeypatch, alive):
    from trellis.modeling.native import export_directory
    import trellis.modeling.native as native_module
    prep, repo, runtime = reopening_parent(job, monkeypatch)
    original_launch = create.launch_preparation
    def sealed_then_interrupted(job_dir, info, root, native, alias, child_runtime):
        original_launch(job_dir, info, root, native, alias, child_runtime)
        write_json(export_directory(root, native) / 'prepared.json', {
            'repo': str(native), 'runtime': str(child_runtime), 'launch_environment': {}})
        # Interruption after launch but before publication/state bookkeeping.
        alias.unlink()
        alias.symlink_to(root)
        raise RuntimeError('publication interrupted')
    monkeypatch.setattr(create, 'launch_preparation', sealed_then_interrupted)
    with pytest.raises(RuntimeError, match='interrupted'):
        create.continue_native(runtime)
    pending = json.loads((job / 'job.json').read_text())
    child, child_repo, child_runtime = create.active_paths(job, pending)
    create.validate_resume(job)
    launches = []
    monkeypatch.setattr(create, 'runtime_running', lambda *_: alive)
    real_run = create.subprocess.run
    def run(argv, **kwargs):
        if str(argv[0]).endswith('/restart_configured_run.sh'):
            launches.append(argv)
            return subprocess.CompletedProcess(argv, 0)
        return real_run(argv, **kwargs)
    monkeypatch.setattr(create.subprocess, 'run', run)
    monkeypatch.setattr(create, 'launch_preparation', lambda *args: pytest.fail('sealed bootstrap extracted again'))
    create.run_job(job, resume=True)
    assert len(launches) == (0 if alive else 1)
    if launches:
        assert '--no-reset' in launches[0]
    assert Path(pending['repo']).resolve() == child_repo
    assert not (child / '.trellis-creating').exists()
    assert not json.loads((job / 'job.json').read_text())['reopening']['pending']


@pytest.mark.parametrize('boundary', ['halt', 'goals', 'settings'])
def test_reopening_refuses_before_retiring_parent_services(job, monkeypatch, boundary):
    _, _, runtime = reopening_parent(job, monkeypatch)
    if boundary == 'halt':
        write_json(runtime / 'checker_disagreement_halt.json', {'reason': 'unresolved'})
    elif boundary == 'goals':
        write_json(job / 'GOAL.json', {})
    else:
        write_json(job / 'config-template.json', {})
    monkeypatch.setattr(create, 'stop_retired_services', lambda *_: pytest.fail('retired before validation'))
    with pytest.raises((ValueError, RuntimeError)):
        create.continue_native(runtime, resume=True)
    assert not json.loads((job / 'job.json').read_text()).get('native_generation')


def test_reopening_generation_two_retains_reference_chain(job, monkeypatch):
    import shutil
    from trellis.modeling.driver import source_fingerprint
    from trellis.modeling.reopening import reference_paths
    prep, repo, runtime = reopening_parent(job, monkeypatch)
    assert create.continue_native(runtime)
    info = json.loads((job / 'job.json').read_text())
    child, child_repo, child_runtime = create.active_paths(job, info)
    shutil.copytree(child / 'work/crate', child_repo / 'crate')
    write_json(child_repo / 'modeling-input/manifest.json', {
        'provenance_schema': 'trellis-native-preparation/v1',
        'preparation_root': str(child), 'source_basis': source_fingerprint(child)})
    write_json(child_runtime / 'runtime_metadata.json', {'repo_path': str(child_repo)})
    state = json.loads((runtime / 'protocol_state.json').read_text())
    state['request_seq'] = 50
    state['modeling_route_decisions'][0]['request_id'] = 50
    write_json(child_runtime / 'protocol_state.json', state)
    assert create.continue_native(child_runtime)
    latest = json.loads((job / 'job.json').read_text())
    grandchild, _, _ = create.active_paths(job, latest)
    assert latest['native_generation'] == 2
    assert reference_paths(grandchild) == list(map(str, [prep, repo, runtime, child, child_repo, child_runtime]))
    assert len(json.loads((grandchild / '.trellis/modeling/state.json').read_text())['route_decisions']) == 2
    assert not create.reopening_status(runtime)['recoverable']
    (job / 'native-controller.pause').touch()
    create.continue_native(runtime, resume=True)
    assert (job / 'native-controller.pause').exists(), 'a stale Resume cannot clear the current stop'


def test_native_disposition_status_does_not_replay_terminal_runtime(job, monkeypatch):
    prep, repo, runtime = reopening_parent(job, monkeypatch)
    result = subprocess.run([str(create.ROOT / 'scripts/trellis_pause.sh'), 'status', str(runtime), str(repo)],
                            check=True, capture_output=True, text=True)
    status = json.loads(result.stdout)
    assert status['state'] == 'preparation_reopening'
    assert status['resumable']
    assert status['modeling_disposition']['reason'] == 'Approved interface repair'
    state = json.loads((runtime / 'protocol_state.json').read_text())
    state['modeling_request']['action'] = 'fail'
    write_json(runtime / 'protocol_state.json', state)
    result = subprocess.run([str(create.ROOT / 'scripts/trellis_pause.sh'), 'status', str(runtime), str(repo)],
                            check=True, capture_output=True, text=True)
    status = json.loads(result.stdout)
    assert status['state'] == 'modeling_failed' and not status['resumable']


def test_operator_stop_during_reopening_requires_explicit_resume(job, monkeypatch):
    _, _, runtime = reopening_parent(job, monkeypatch)
    publish = create.publish_alias
    def pause_on_publish(*args, **kwargs):
        publish(*args, **kwargs)
        create.stop_job(job)
    monkeypatch.setattr(create, 'publish_alias', pause_on_publish)
    with pytest.raises(RuntimeError, match='stopped by operator'):
        create.continue_native(runtime)
    assert not create.continue_native(runtime)
    monkeypatch.setattr(create, 'publish_alias', publish)
    create.run_job(job, resume=True)
    assert not (job / 'native-controller.pause').exists()
    assert not json.loads((job / 'job.json').read_text())['reopening']['pending']


def test_reopening_does_not_claim_existing_runtime(job, monkeypatch):
    _, _, runtime = reopening_parent(job, monkeypatch)
    collision = runtime.parent / 'parser-r0001-runtime'
    collision.mkdir()
    (collision / 'evidence').write_text('unrelated existing runtime')
    monkeypatch.setattr(create, 'stop_retired_services', lambda *_: pytest.fail('retired before path validation'))
    with pytest.raises(ValueError, match='already occupied'):
        create.continue_native(runtime)
    assert (collision / 'evidence').read_text() == 'unrelated existing runtime'
    assert not json.loads((job / 'job.json').read_text()).get('native_generation')


def test_reopening_retries_interruption_before_publishing_intent(job, monkeypatch):
    _, _, runtime = reopening_parent(job, monkeypatch)
    save = create.save_json
    def interrupted(path, value, **kwargs):
        if path == job / 'job.json' and value.get('native_generation') == 1:
            raise RuntimeError('intent publication interrupted')
        return save(path, value, **kwargs)
    monkeypatch.setattr(create, 'save_json', interrupted)
    with pytest.raises(RuntimeError, match='intent publication'):
        create.continue_native(runtime)
    assert not json.loads((job / 'job.json').read_text()).get('native_generation')
    assert (job / 'native/generations/0001/reopened-from.json.lock').is_file()
    monkeypatch.setattr(create, 'save_json', save)
    create.continue_native(runtime)
    info = json.loads((job / 'job.json').read_text())
    assert info['native_generation'] == 1 and not info['reopening']['pending']


def test_native_resume_uses_environment_preserving_creation_launcher(job, monkeypatch):
    _, _, runtime = reopening_parent(job, monkeypatch)
    calls = []
    real_run = subprocess.run
    def run(argv, **kwargs):
        if argv[0] != 'bash':
            return real_run(argv, **kwargs)
        calls.append((argv, kwargs))
        return subprocess.CompletedProcess(argv, 0)
    monkeypatch.setattr(create.subprocess, 'run', run)
    monkeypatch.setenv('TRELLIS_TRELLIS_KERNEL_CMD', '/selected/deployment/kernel')
    create.schedule_reopening(runtime)
    assert len(calls) == 1
    argv, options = calls[0]
    assert argv == ['bash', str(create.ROOT/'scripts/pv_create_run.sh'), 'reopen', 'parser']
    assert options['env']['TRELLIS_PROJECTS_ROOT'] == str(job.parent.parent.parent)
    assert options['env']['TRELLIS_TRELLIS_KERNEL_CMD'] == '/selected/deployment/kernel'


def test_reopening_tmux_handoff_keeps_current_kernel_environment(job, tmp_path):
    """Real tmux boundary: its pre-existing server has a stale kernel setting."""
    import time
    socket = 'trellis-reopening-' + str(os.getpid()) + '-' + tmp_path.name[-8:]
    tmux = ['tmux', '-L', socket]
    stub = tmp_path / 'native-launch.py'
    stub.write_text('''import json,os,subprocess,sys
from pathlib import Path
job=Path(sys.argv[2])
if sys.argv[1]=='reopen':
    (job/'observed-kernel').write_text(os.environ['TRELLIS_TRELLIS_KERNEL_CMD'])
    info=json.loads((job/'job.json').read_text())
    info['native_generation']=1
    info['runtime_root']=str(job/'child-runtime')
    (job/'job.json').write_text(json.dumps(info))
    subprocess.run(['tmux','-L',os.environ['TRELLIS_TMUX_SOCKET'],'new-session','-d',
                    '-s','trellis-run-parser-r0001','sleep 30'],check=True)
elif sys.argv[1]!='started':
    raise RuntimeError('Unexpected command: '+sys.argv[1])
''')
    stale = {**os.environ, 'TRELLIS_TRELLIS_KERNEL_CMD': '/stale/server/kernel'}
    env = {**os.environ, 'TRELLIS_TRELLIS_KERNEL_CMD': '/current/deployment/kernel',
           'TRELLIS_PROJECTS_ROOT': str(job.parent.parent.parent),
           'TRELLIS_TMUX_SOCKET': socket,
           'TRELLIS_PV_CREATE_NATIVE_CMD': f'{sys.executable} {stub}'}
    try:
        subprocess.run([*tmux, 'new-session', '-d', '-s', 'anchor', 'sleep 30'], env=stale, check=True)
        subprocess.run(['bash', str(create.ROOT/'scripts/pv_create_run.sh'), 'reopen', 'parser'],
                       env=env, check=True, capture_output=True, text=True, timeout=10)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            status = json.loads((job/'status.json').read_text()) if (job/'status.json').exists() else {}
            if status.get('state') in {'done', 'resolve_failed', 'launch_failed'}:
                break
            time.sleep(.05)
        assert status.get('state') == 'done', (status, (job/'create.log').read_text())
        assert (job/'observed-kernel').read_text() == '/current/deployment/kernel'
    finally:
        subprocess.run([*tmux, 'kill-server'], capture_output=True)

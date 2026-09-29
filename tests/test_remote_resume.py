from __future__ import annotations

import copy
import hashlib
import json
from pathlib import Path

import pytest

from trellis.remote_resume import (ResumeJob, atomic_json, checkpoint_at, claim, git,
    rebind_destination_paths, select_checkpoint, validate_remote, run)
from trellis.runtime.git_checkpoint_hook import _do_push


def repository(path):
    path.mkdir()
    git(path, "init", "-b", "main")
    git(path, "config", "user.name", "Resume Test")
    git(path, "config", "user.email", "resume@example.invalid")
    return path


def commit(repo, message):
    git(repo, "add", "-A")
    git(repo, "commit", "-m", message)
    return git(repo, "rev-parse", "HEAD").decode().strip()


def snapshot(repo, *, cycle=1, stage="Start", inflight=None, index=0):
    source = b"theorem trivial_fact : True := by trivial\n"
    (repo / "Tablet").mkdir(exist_ok=True)
    (repo / "Tablet/A.lean").write_bytes(source)
    state = {"cycle": cycle, "phase": "ProofFormalization", "stage": stage,
        "in_flight_request": inflight, "tablet_target": "lean", "corr_fingerprint_schema_version": 4,
        "sound_assessment_schema_version": 1, "trust_base": {"mode": "disabled"},
        "committed": {"present_nodes": ["A"]}, "live": {"present_nodes": ["A"]},
        "local_closure_records": {"A": {"active_decl_hash": hashlib.sha256(source).hexdigest()}}}
    checkpoint = {key: state[key] for key in ["cycle", "phase", "committed"]}
    history = repo / ".trellis-history"
    atomic_json(history / "supervisor_state.json", {"event_count": index + 1, "state": state,
        "checkpoint": checkpoint, "metadata": {"repo_path": "/old/repo"}})
    logs = history / "event-log"
    logs.mkdir(exist_ok=True)
    with (logs / f"cycle-{cycle:06d}.jsonl").open("ab") as stream:
        stream.write(json.dumps({"index": index, "cycle": cycle}).encode() + b"\n")
    return commit(repo, f"checkpoint cycle {cycle}")


def test_selects_latest_idle_on_ancestry_and_reports_skipped_work(tmp_path):
    repo = repository(tmp_path / "repo")
    idle = snapshot(repo)
    snapshot(repo, cycle=2, stage="Worker", inflight={"id": 8}, index=1)
    (repo / "notes").write_text("manual edit after snapshot\n")
    tip = commit(repo, "unpublished state")
    selected = select_checkpoint(repo, tip)
    assert selected["selected_commit"] == idle
    assert selected["fetched_tip"] == tip
    assert selected["skipped_commits"] == 2
    assert [row["reason"] for row in selected["skipped"]] == ["no new checkpoint snapshot", "in-flight request"]
    assert checkpoint_at(repo, idle)["document"]["state"]["in_flight_request"] is None


@pytest.mark.parametrize("stage", ["HumanGate", "Complete"])
def test_human_and_terminal_boundaries_remain_held(tmp_path, stage):
    repo = repository(tmp_path / "repo")
    tip = snapshot(repo, stage=stage)
    assert select_checkpoint(repo, tip)["stage"] == stage


def test_rejects_unpaired_sources_sparse_logs_and_unsupported_schema(tmp_path):
    repo = repository(tmp_path / "repo")
    snapshot(repo)
    document = json.loads((repo / ".trellis-history/supervisor_state.json").read_text())
    document["state"]["cycle"] = 2
    document["checkpoint"]["cycle"] = 2
    atomic_json(repo / ".trellis-history/supervisor_state.json", document)
    (repo / "Tablet/A.lean").write_text("theorem different : True := by trivial\n")
    bad = commit(repo, "unpaired snapshot")
    with pytest.raises(ValueError, match="source/checkpoint mismatch"):
        checkpoint_at(repo, bad)
    (repo / "Tablet/A.lean").write_bytes(b"theorem trivial_fact : True := by trivial\n")
    (repo / ".trellis-history/event-log/cycle-000001.jsonl").write_text('{"index":1,"cycle":1}\n')
    sparse = commit(repo, "sparse log")
    with pytest.raises(ValueError, match="not dense"):
        checkpoint_at(repo, sparse)
    document["state"]["sound_assessment_schema_version"] = 0
    atomic_json(repo / ".trellis-history/supervisor_state.json", document)
    old = commit(repo, "unsupported schema")
    with pytest.raises(ValueError, match="schema"):
        checkpoint_at(repo, old)


@pytest.mark.parametrize("convention", ["event_index", None, 1])
def test_rejects_unknown_checkpoint_count_convention(tmp_path, convention):
    repo = repository(tmp_path / "repo")
    snapshot(repo)
    path = repo / ".trellis-history/supervisor_state.json"
    document = json.loads(path.read_text())
    document["event_count_convention"] = convention
    atomic_json(path, document)
    selected = commit(repo, "unsupported count convention")
    with pytest.raises(ValueError, match="event-count convention"):
        checkpoint_at(repo, selected)


def test_rebinds_destination_paths_and_blocks_missing_mathematical_content(tmp_path):
    repo = tmp_path / "repo"
    (repo / "paper").mkdir(parents=True)
    (repo / "paper/paper.tex").write_text("mathematical reference")
    (repo / "trellis.policy.json").write_text("{}")
    config = {"repo_path": "/old/repo", "policy_path": "trellis.policy.json",
        "workflow": {"paper_tex_path": "/old/repo/paper/paper.tex"},
        "tmux": {"burst_home": "/old/provider-context"}, "chat": {"root_dir": "/old/repo/chats"}}
    metadata = {"repo_path": "/old/repo", "config_path": "/old/repo/trellis.config.json", "native_history_kinds": ["worker", "review"]}
    relocated, meta = rebind_destination_paths(config, metadata, repo, tmp_path / "runtime")
    assert relocated["workflow"]["paper_tex_path"] == str(repo / "paper/paper.tex")
    assert meta["config_path"] == str(repo / "trellis.config.json")
    assert meta["native_history_kinds"] == []
    assert config["repo_path"] == "/old/repo"
    (repo / "paper/paper.tex").unlink()
    with pytest.raises(ValueError, match="required paper is missing"):
        rebind_destination_paths(config, metadata, repo, tmp_path / "runtime")


@pytest.mark.parametrize("remote", ["https://token@host/repo", "https://user:pass@host/repo", "-evil", "ext::shell", "https://host/repo?token=x"])
def test_remote_credentials_and_command_transports_are_rejected(remote):
    with pytest.raises(ValueError):
        validate_remote(remote)


def test_retry_of_running_destination_only_verifies(monkeypatch):
    job = object.__new__(ResumeJob)
    job.slug = "resumed"
    job.progress = {}
    job.alive = lambda name: True
    seen = []
    job.verify_launch = lambda: seen.append("verified")
    job.clone_select = lambda: pytest.fail("running destination reconstructed")
    job.run()
    assert seen == ["verified"]


def mirror_fixture(tmp_path):
    remote = tmp_path / "remote.git"
    run(["git", "init", "--bare", remote])
    repo = repository(tmp_path / "writer")
    (repo / "value").write_text("one")
    initial = commit(repo, "first")
    git(repo, "remote", "add", "origin", str(remote))
    git(repo, "push", "origin", "main")
    return remote, repo, initial


def test_mirror_refuses_changed_remote_tip(tmp_path):
    remote, repo, initial = mirror_fixture(tmp_path)
    atomic_json(repo / ".trellis/resume-mirror.json", {"version": 1, "remote_url": str(remote),
        "branch": "main", "expected_tip": initial, "initial": True, "allow_initial_rewind": False})
    (repo / "value").write_text("local successor")
    # Stage only source: local operational guard must never enter Git.
    git(repo, "add", "value")
    git(repo, "commit", "-m", "successor")
    other = tmp_path / "other"
    run(["git", "clone", "-b", "main", remote, other])
    git(other, "config", "user.name", "Other")
    git(other, "config", "user.email", "other@example.invalid")
    (other / "value").write_text("other writer")
    changed = commit(other, "concurrent")
    git(other, "push", "origin", "main")
    _do_push(repo, "origin", log_path=repo / ".trellis/push.log")
    assert run(["git", "--git-dir", remote, "rev-parse", "refs/heads/main"]).decode().strip() == changed
    assert "resume_remote_lineage_changed" in (repo / ".trellis/push.log").read_text()


def test_earlier_checkpoint_requires_explicit_rewind_then_uses_exact_lease(tmp_path):
    remote, repo, earlier = mirror_fixture(tmp_path)
    (repo / "value").write_text("later checkpoint")
    tip = commit(repo, "later")
    git(repo, "push", "origin", "main")
    git(repo, "reset", "--hard", earlier)
    path = repo / ".trellis/resume-mirror.json"
    guard = {"version": 1, "remote_url": str(remote), "branch": "main", "expected_tip": tip,
        "initial": True, "allow_initial_rewind": False}
    atomic_json(path, guard)
    _do_push(repo, "origin", log_path=repo / ".trellis/push.log")
    assert run(["git", "--git-dir", remote, "rev-parse", "main"]).decode().strip() == tip
    guard["allow_initial_rewind"] = True
    atomic_json(path, guard)
    _do_push(repo, "origin", log_path=repo / ".trellis/push.log")
    assert run(["git", "--git-dir", remote, "rev-parse", "main"]).decode().strip() == earlier
    assert json.loads(path.read_text())["expected_tip"] == earlier


def test_stopped_launch_attempt_cannot_spawn_again():
    job = object.__new__(ResumeJob)
    job.slug = "stopped"
    job.progress = {"launch_intent": True}
    job.alive = lambda name: False
    job.finish_budget_pause = lambda: False
    job.clone_select = lambda: pytest.fail("attempted to reconstruct after launch")
    with pytest.raises(ValueError, match="already attempted"):
        job.run()


def test_build_json_failure_blocks_even_with_zero_process_exit(tmp_path, monkeypatch):
    from types import SimpleNamespace
    from trellis.remote_resume import build_view
    atomic_json(tmp_path / "lake-manifest.json", {"packages": []})
    monkeypatch.setattr("trellis.remote_resume.subprocess.run", lambda *a, **kw:
        SimpleNamespace(returncode=0, stdout=json.dumps({"returncode": 1, "stderr": "Lean failed"})))
    with pytest.raises(RuntimeError, match="Lean failed"):
        build_view(tmp_path, {})


def test_retry_after_pin_before_initial_checkout(tmp_path):
    repo = repository(tmp_path / "source")
    selected = snapshot(repo)
    dest = tmp_path / "dest"; dest.mkdir()
    git(dest, "init")
    git(dest, "fetch", str(repo), "main")
    job = object.__new__(ResumeJob)
    job.repo = dest
    job.runtime = tmp_path / "runtime"
    job.progress = {"selection": {"selected_commit": selected, "branch": "main"}}
    job.save = lambda: None
    job.clone_select()
    assert job.progress["checkout_complete"]
    assert git(dest, "rev-parse", "HEAD").decode().strip() == selected


def test_migration_builds_never_change_caller_parallelism_or_revive_source_tokens(tmp_path, monkeypatch):
    from trellis.remote_resume import ResumeJob
    profile = tmp_path / "profile.json"
    atomic_json(profile, {"version":1, "runtime_cli":"/bin/true", "lean_parallelism":1,
        "env":{"TRELLIS_SOUNDNESS_FINGERPRINT_MODE":"text", "TRELLIS_LOCAL_CLOSURE_AXCHECK_ENABLED":"1"},
        "dependency_prepare_argv":["true"]})
    claim(tmp_path / "job", tmp_path / "repo", tmp_path / "runtime", "continued", "/local/remote", profile, handoff=True, lean_parallelism=3)
    monkeypatch.setenv("TRELLIS_CHECKER_TOKEN", "old-source-token")
    monkeypatch.setenv("LEAN_NUM_THREADS", "9")
    job = ResumeJob(tmp_path / "job")
    assert "TRELLIS_CHECKER_TOKEN" not in job.env
    assert all(job.env[key] == "3" for key in ["TRELLIS_LEAN_PARALLELISM", "TRELLIS_BURST_LEAN_PARALLELISM", "LEAN_NUM_THREADS"])
    assert __import__('os').environ["LEAN_NUM_THREADS"] == "9"


@pytest.mark.parametrize("wrong_root", [False, True])
def test_real_process_launch_identity_and_retry(tmp_path, monkeypatch, wrong_root):
    import os
    import subprocess
    import sys
    socket_name = f"resume-verify-{os.getpid()}-{int(wrong_root)}"
    monkeypatch.setenv("TRELLIS_TMUX_SOCKET", socket_name)
    profile = tmp_path / "profile.json"
    atomic_json(profile, {"version":1, "runtime_cli":str(Path(sys.executable).resolve()), "lean_parallelism":1,
        "launch_verify_seconds":10,
        "env":{"TRELLIS_SOUNDNESS_FINGERPRINT_MODE":"text", "TRELLIS_LOCAL_CLOSURE_AXCHECK_ENABLED":"1"},
        "dependency_prepare_argv":["true"]})
    repo, runtime, directory = tmp_path / "repo", tmp_path / "runtime", tmp_path / "job"
    claim(directory, repo, runtime, "verification", "/local/remote", profile, handoff=True)
    repo.mkdir(); runtime.mkdir()
    atomic_json(repo / "trellis.config.json", {})
    (repo / ".trellis-creating").write_text("verification")
    job = ResumeJob(directory)
    job.progress.update(launch_intent=True, launch_event_count=0)
    log = repo / ".trellis-history/event-log/cycle-000001.jsonl"
    program = "from pathlib import Path; import time; p=Path(" + repr(str(log)) + "); p.parent.mkdir(parents=True); p.write_text('{}\\n'); time.sleep(30)"
    env = {**job.env, "TRELLIS_CHECKER_SOCKET":str(runtime / "sockets/checker.sock")}
    if wrong_root:
        env["TRELLIS_KERNEL_CACHE_ROOT"] = str(tmp_path / "other")
    try:
        job.start_session("trellis-run-verification", [sys.executable, "-c", program], env)
        if wrong_root:
            with pytest.raises(ValueError, match="wrong runtime"):
                job.verify_launch()
        else:
            job.verify_launch()
            assert job.progress["launched"]
            pid = job.progress["supervisor_pid"]
            job.clone_select = lambda: pytest.fail("already running destination was reconstructed")
            job.run()
            assert job.progress["supervisor_pid"] == pid
    finally:
        subprocess.run(["tmux", "-L", socket_name, "kill-server"], capture_output=True)


@pytest.mark.parametrize("delay,failure", [(0, None), (1.5, None), (0, "halt"), (0, "stale"), (0, "no_runtime")])
def test_initial_budget_pause_graduates_without_progress_or_duplicate_launch(tmp_path, monkeypatch, delay, failure):
    import os
    import subprocess
    import sys
    from trellis.remote_resume import ROOT
    socket = f"resume-budget-{os.getpid()}"
    monkeypatch.setenv("TRELLIS_TMUX_SOCKET", socket)
    profile = tmp_path / "profile.json"
    cli = str(Path(sys.executable).resolve())
    atomic_json(profile, {"version":1, "runtime_cli":cli, "lean_parallelism":3,
        "launch_verify_seconds":10,
        "env":{"TRELLIS_SOUNDNESS_FINGERPRINT_MODE":"text", "TRELLIS_LOCAL_CLOSURE_AXCHECK_ENABLED":"1"},
        "dependency_prepare_argv":["true"]})
    repo, runtime, directory = tmp_path / "repo", tmp_path / "runtime", tmp_path / "job"
    claim(directory, repo, runtime, "budget", "/local/remote", profile, handoff=True)
    repo.mkdir(); runtime.mkdir()
    atomic_json(repo / "trellis.config.json", {})
    atomic_json(repo / ".trellis/resume-mirror.json", {})
    marker = repo / ".trellis-creating"; marker.touch()
    job = ResumeJob(directory)
    job.progress["selection"] = {}
    spawn = job.start_session
    calls = []
    def start(name, argv, env):
        calls.append(name)
        spawn(name, [cli, ROOT / "tests/first_launch_fixture.py", runtime, repo, cli],
              {**env, "FIRST_PAUSE_DELAY":str(delay), "FIRST_PAUSE_FAILURE":failure or ""})
    job.start_session = start
    try:
        if failure:
            with pytest.raises(RuntimeError, match="halted|exited"):
                job.launch()
            assert marker.exists()
            with pytest.raises(ValueError, match="already attempted"):
                job.run()
        else:
            job.launch()
            assert json.loads((directory / "status.json").read_text())["state"] == "done"
            assert not marker.exists()
            assert job.progress["launch_outcome"] == "provider_budget_paused"
            assert not (repo / ".trellis-history/event-log").exists()
            state = (runtime / "protocol_state.json").read_bytes()
            pause = (runtime / "pause_request.json").read_bytes()
            job.clone_select = lambda: pytest.fail("paused runtime reconstructed")
            # Simulate interruption before the completion receipt/marker removal.
            job.progress.pop("launched"); job.progress.pop("launch_outcome")
            marker.touch(); job.save()
            job.run()
            job.run()
            assert (runtime / "protocol_state.json").read_bytes() == state
            assert (runtime / "pause_request.json").read_bytes() == pause
            assert not marker.exists()
        assert calls == ["trellis-run-budget"]
    finally:
        subprocess.run(["tmux", "-L", socket, "kill-server"], capture_output=True)

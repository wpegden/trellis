"""Real cold Lean/checker integration, opt-in and wholly disposable.

Build the CLI and `cargo test --test remote_resume_lean --no-run` into the
same TRELLIS_REMOTE_RESUME_TARGET first. No provider process is started.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

import pytest

from trellis.remote_resume import ResumeJob, atomic_json, build_view, claim, cli_call, git, maintenance_token, read_json, run
from trellis.checking import write_scripts
from trellis.supervisor_workspace import sync_supervisor_workspace
from trellis.atomic_actions.checker_client import client_ping

SOURCE = Path(__file__).resolve().parents[1]


@pytest.mark.skipif(os.environ.get("TRELLIS_RUN_REMOTE_RESUME_LEAN") != "1", reason="opt-in real cold Lean rebuild")
@pytest.mark.parametrize("historical", [False, True])
def test_cold_remote_resume_real_lean(historical):
    target = Path(os.environ.get("TRELLIS_REMOTE_RESUME_TARGET", SOURCE / ".build/resume"))
    cli = target / "debug/trellis_runtime_cli"
    helpers = [p for p in (target / "debug/deps").glob("remote_resume_lean-*") if p.is_file() and os.access(p, os.X_OK)]
    helper = max(helpers, key=lambda p: p.stat().st_mtime)
    scratch = Path(tempfile.mkdtemp(prefix="rr.", dir=Path.home() / ".cache"))
    socket_name = "resume-test-" + str(os.getpid())
    checker = None
    env = {**os.environ, "PYTHONPATH": str(SOURCE), "TRELLIS_TMUX_SOCKET": socket_name,
           "TRELLIS_SOUNDNESS_FINGERPRINT_MODE": "text", "TRELLIS_LOCAL_CLOSURE_AXCHECK_ENABLED": "1",
           "TRELLIS_LEAN_PARALLELISM": "1", "LEAN_NUM_THREADS": "1", "RUST_MIN_STACK": "67108864"}
    print(f"real resume evidence: {scratch}", flush=True)
    try:
        repo = scratch / "source"
        runtime = scratch / "r0"
        repo.mkdir(); runtime.mkdir()
        git(repo, "init", "-b", "main")
        git(repo, "config", "user.name", "Resume Fixture")
        git(repo, "config", "user.email", "fixture@example.invalid")
        (repo / "Tablet").mkdir()
        files = {
            ".gitignore": ".lake/\n.trellis/\n",
            "lean-toolchain": "leanprover/lean4:v4.33.0\n",
            "lakefile.lean": 'import Lake\nopen Lake DSL\npackage tablet\n@[default_target] lean_lib Tablet\nlean_exe cache where\n  root := `Cache\n',
            # There are no external dependencies to fetch. This project's
            # support-cache entry point is a real Lean executable with no work.
            "Cache.lean": "def main (_ : List String) : IO Unit := pure ()\n",
            "lake-manifest.json": '{"version":"1.2.0","packagesDir":".lake/packages","packages":[],"name":"tablet","lakeDir":".lake","fixedToolchain":false}\n',
            "Tablet.lean": "import Tablet.Fact\n",
            "Tablet/Preamble.lean": "-- shared preamble\n",
            "Tablet/Preamble.tex": "\\begin{definition}Natural numbers use their usual operations.\\end{definition}\n",
            "Tablet/Value.lean": "import Tablet.Preamble\n\ndef Value : Nat := 7\n",
            "Tablet/Value.tex": "\\begin{definition}\\label{def:Value}\nLet Value be seven.\n\\end{definition}\n",
            "Tablet/Fact.lean": "import Tablet.Value\n\ntheorem Fact : Value = 7 := by\n-- BODY\n  rfl\n",
            "Tablet/Fact.tex": "\\begin{theorem}\\label{thm:Fact}\nValue is seven.\n\\end{theorem}\n\\begin{proof}By definition.\\end{proof}\n",
            "paper.tex": "\\begin{theorem}Seven is seven.\\end{theorem}\n",
            "APPROVED_AXIOMS.json": "[]\n",
        }
        for name, content in files.items():
            (repo / name).write_text(content)
        config = read_json(SOURCE / "examples/trellis.config.json")
        config.update(repo_path=str(repo), policy_path="trellis.policy.json")
        config["workflow"].update(paper_tex_path="paper.tex", allowed_import_prefixes=["Init"], main_result_targets=[], main_result_labels=[])
        config["git"].update(branch="main", remote_url=None)
        atomic_json(repo / "trellis.config.json", config)
        shutil.copyfile(SOURCE / "examples/trellis.policy.json", repo / "trellis.policy.json")
        atomic_json(runtime / "runtime_metadata.json", {"repo_path": str(repo), "config_path": str(repo / "trellis.config.json")})
        git(repo, "add", "-A"); git(repo, "commit", "-m", "source fixture")
        write_scripts(repo, repo / ".trellis")
        sync_supervisor_workspace(repo)
        checklog = (scratch / "checker.log").open("w")
        checker = subprocess.Popen([str(SOURCE / "scripts/trellis_checker_server.sh"), str(runtime), "--parallelism", "1"], env=env, stdout=checklog, stderr=subprocess.STDOUT)
        socket = runtime / "sockets/checker.sock"
        for _ in range(60):
            if checker.poll() is not None:
                pytest.fail((scratch / "checker.log").read_text())
            try:
                client_ping(socket, timeout_secs=1)
                break
            except Exception:
                time.sleep(1)
        else:
            pytest.fail("isolated source checker did not start")
        with maintenance_token(runtime) as token:
            observed_env = {**env, "TRELLIS_CHECKER_SOCKET": str(socket), "TRELLIS_CHECKER_TOKEN": token,
                "TRELLIS_KERNEL_CACHE_ROOT": str(runtime), "TRELLIS_RESUME_TEST_REPO": str(repo), "TRELLIS_RESUME_TEST_ROOT": str(runtime)}
            build_view(repo, observed_env)
            run([helper, "--ignored", "--nocapture"], env=observed_env, capture=False)
        git(repo, "add", "-A"); git(repo, "commit", "-m", "idle checkpoint fixture")
        head = git(repo, "rev-parse", "HEAD").decode().strip()
        view = {"repo_path": str(repo), "runtime_root": str(runtime), "paper_path": str(repo / "paper.tex"), "checker_socket": str(socket), "source_commit": head}
        atomic_json(runtime / "trusted-rebind-views.json", {"version": 1, "views": {"live": view, "committed": view}})
        run(["python3", SOURCE / "scripts/migrate_local_closure_records", runtime, "--runtime-cli", cli, "--mode", "trusted-artifact-rebind", "--apply", "--parallelism", "1"], env=env, capture=False)
        git(repo, "add", "-A"); git(repo, "commit", "-m", "certified idle checkpoint fixture")
        before = read_json(runtime / "protocol_state.json")
        if historical:
            git(repo, "tag", "supervisor2/clean-000002")
            for name in ["Value.lean", "Fact.lean", "Value.tex", "Fact.tex"]:
                path = repo / "Tablet" / name
                path.write_text(path.read_text().replace("7", "8").replace("seven", "eight"))
            with maintenance_token(runtime) as token:
                changed_env = {**observed_env, "TRELLIS_CHECKER_TOKEN":token}
                build_view(repo, changed_env)
                run([helper, "--ignored", "--nocapture"], env=changed_env, capture=False)
            document = read_json(repo / ".trellis-history/supervisor_state.json")
            current = document["state"]
            for key in list(current):
                if key.startswith("last_clean_") and key[len("last_clean_"):] in before:
                    current[key] = before[key[len("last_clean_"):]]
            current.update(last_clean_commit=head, last_clean_verifier_mirror_ready=True,
                           last_clean_local_closure_mirror_ready=True, has_ever_been_clean=True, cycles_since_clean=2)
            document["state"] = current
            atomic_json(repo / ".trellis-history/supervisor_state.json", document)
            git(repo, "add", "-A"); git(repo, "commit", "-m", "later source checkpoint with historical clean tier")
            before = current
        # Produce the legacy boundary with the real runtime and Git hook after
        # observing genuine Lean source/evidence. Its final response is absent
        # from the Git prefix, exactly as in existing ordinary checkpoints.
        atomic_json(runtime / "protocol_state.json", before)
        run([helper, "--ignored", "--nocapture"], env={**env, "TRELLIS_RESUME_TEST_MODE":"legacy_checkpoint",
            "TRELLIS_RESUME_TEST_REPO":str(repo), "TRELLIS_RESUME_TEST_ROOT":str(runtime)}, capture=False)
        from trellis.history_artifacts import decode_shared_state
        selected_document = decode_shared_state(read_json(repo / ".trellis-history/supervisor_state.json"))
        before = selected_document["state"]
        assert "event_count_convention" not in selected_document
        selected_seed = scratch / "original-selected.json"
        shutil.copyfile(repo / ".trellis-history/supervisor_state.json", selected_seed)
        checker.terminate(); checker.wait(timeout=15); checker = None
        remote = scratch / "remote.git"
        run(["git", "clone", "--bare", repo, remote])
        profile = scratch / "profile.json"
        atomic_json(profile, {"version": 1, "runtime_cli": str(cli), "lean_parallelism": 1,
            "env": {"TRELLIS_SOUNDNESS_FINGERPRINT_MODE": "text", "TRELLIS_LOCAL_CLOSURE_AXCHECK_ENABLED": "1"},
            "dependency_prepare_argv": ["lake", "build"]})
        job_dir, dest, root = scratch / "job", scratch / "dest", scratch / "r1"
        claim(job_dir, dest, root, "coldfixture", str(remote), profile, branch="main", handoff=True, lean_parallelism=1)
        os.environ["TRELLIS_TMUX_SOCKET"] = socket_name
        job = ResumeJob(job_dir)
        job.run()
        assert read_json(job_dir / "status.json")["stage"] == "held"
        assert not job.alive("trellis-run-coldfixture")
        after = read_json(root / "protocol_state.json")
        assert after["cycle"] == before["cycle"]
        assert after["human_input_outstanding"] is True
        assert after["in_flight_request"] is None
        launch = read_json(root / "launch_env.json")
        expected_env = {"TRELLIS_TRELLIS_KERNEL_CMD":str(cli),
            "TRELLIS_KERNEL_CACHE_ROOT":str(root), "TRELLIS_CHECKER_SOCKET":str(root / "sockets/checker.sock"),
            "TRELLIS_LEAN_PARALLELISM":"1", "TRELLIS_BURST_LEAN_PARALLELISM":"1", "LEAN_NUM_THREADS":"1"}
        assert all(launch["env"][key] == value for key, value in expected_env.items())
        assert launch["trellis_sh"] == str(SOURCE / "scripts/trellis.sh")
        assert launch["trellis_head"] == git(SOURCE, "rev-parse", "HEAD").decode().strip()
        assert launch["tmux_socket"] == socket_name
        assert launch["runtime_cli_sha256"] == hashlib.sha256(cli.read_bytes()).hexdigest()
        # Exercise ordinary Resume's real launcher generation with inert tmux,
        # then verify its exports despite a conflicting ambient environment.
        controls = scratch / "controls"; controls.mkdir()
        for name, body in {"pgrep":"exit 1", "sleep":"exit 0",
                "tmux":'printf "%s\\n" "$*" >> "$RESUME_CONTROL_LOG"'}.items():
            path = controls / name; path.write_text("#!/bin/sh\n" + body + "\n"); path.chmod(0o700)
        control_env = {**os.environ, "PATH":f"{controls}:{os.environ['PATH']}",
                       "RESUME_CONTROL_LOG":str(scratch / "controls.log")}
        result = subprocess.run(["bash", SOURCE / "scripts/trellis_pause.sh", "resume", root, dest],
                                env=control_env, capture_output=True, text=True)
        assert result.returncode == 4, result.stdout + result.stderr  # inert tmux starts no provider
        launcher = (root / "resume_launch.sh").read_text()
        assert f"-L {socket_name} " in (scratch / "controls.log").read_text()
        exports = launcher.rsplit("exec ", 1)[0]
        probe = "python3 - <<'PY'\nimport json,os\nprint(json.dumps({k:os.environ[k] for k in " + repr(list(expected_env)) + "}))\nPY\n"
        measured = run(["bash", "-c", exports + probe], env={**os.environ, **{k:"stale" for k in expected_env}})
        assert json.loads(measured) == expected_env
        assert read_json(root / "protocol_state.json") == after
        for tier in ["local_closure_records", "committed_local_closure_records"]:
            assert set(after[tier]) == {"Preamble", "Value", "Fact"}
        original_log = (repo / ".trellis-history/event-log/cycle-000007.jsonl").read_bytes()
        new_log = (dest / ".trellis-history/event-log/cycle-000007.jsonl").read_bytes()
        assert new_log.startswith(original_log)
        replay = scratch / "replay.json"
        cli_call(str(cli), {"action":"replay_to_event_count", "root": str(root), "stop_after_event_count": len(new_log.splitlines()), "seed_checkpoint_path": str(root / "trusted-rebind-seed.json"), "dry_run_state_path": str(replay)}, job.env)
        assert read_json(replay) == after
        for seed in [selected_seed, runtime / "legacy-predecessor.json"]:
            cli_call(str(cli), {"action":"replay_to_event_count", "root":str(root),
                "stop_after_event_count":len(new_log.splitlines()), "seed_checkpoint_path":str(seed),
                "dry_run_state_path":str(replay)}, job.env)
            assert read_json(replay) == after
        # Retry does not append another maintenance transition or reconstruct.
        (root / "launch_env.json").unlink()  # interruption after reconstruction receipt
        job.run()
        assert all(read_json(root / "launch_env.json")["env"][key] == value for key, value in expected_env.items())
        assert (dest / ".trellis-history/event-log/cycle-000007.jsonl").read_bytes() == new_log
        # Erase the admitted binaries, restore the retained epoch, and run full
        # production artifact admission again, using the real activation helper.
        artifact = dest / ".trellis/supervisor/repo/.lake/build/lib/lean/Tablet/Fact.olean"
        digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
        artifact.unlink()
        run([helper, "--ignored", "--nocapture"], env={**job.env, "RUST_MIN_STACK":"67108864", "TRELLIS_RESUME_TEST_MODE":"epoch", "TRELLIS_RESUME_TEST_REPO":str(dest), "TRELLIS_RESUME_TEST_ROOT":str(root)}, capture=False)
        assert hashlib.sha256(artifact.read_bytes()).hexdigest() == digest
        job.readiness()
        # Destructive replay must preserve one coherent generation, including
        # the original selected seed which skips the boundary event.
        log_path = dest / ".trellis-history/event-log/cycle-000007.jsonl"
        count = len(new_log.splitlines())
        request = {"action":"replay_to_event_count", "root":str(root),
                   "stop_after_event_count":count, "seed_checkpoint_path":str(selected_seed)}
        def inverse():
            paths = [root / name for name in ["protocol_state.json", "checkpoint.json", "runtime_metadata.json"]]
            paths += [dest / "trellis.config.json", dest / ".trellis-history/supervisor_state.json"]
            paths += sorted((dest / ".trellis-history/event-log").glob("cycle-*.jsonl"))
            return {str(path):path.read_bytes() for path in paths}
        def refused(fragment):
            before_files = inverse()
            before_head = git(dest, "rev-parse", "HEAD")
            result = subprocess.run([str(cli)], input=json.dumps(request), capture_output=True, text=True, env=job.env)
            assert result.returncode != 0 and fragment in result.stdout + result.stderr
            assert inverse() == before_files
            assert git(dest, "rev-parse", "HEAD") == before_head
        epochs = root / "rollback-artifact-epochs"
        hidden_epochs = root / "hidden-epochs"
        epochs.rename(hidden_epochs)
        try:
            refused("target artifact epoch unavailable")
        finally:
            hidden_epochs.rename(epochs)
        prior_bytes = (root / "protocol_state.json").read_bytes()
        prior = read_json(root / "protocol_state.json")
        prior["local_closure_records"]["Fact"]["accepted_at_snapshot_id"] += "-missing-previous-epoch"
        atomic_json(root / "protocol_state.json", prior)
        try:
            refused("rollback artifact epoch unavailable")
        finally:
            (root / "protocol_state.json").write_bytes(prior_bytes)
        for shortened in [False, "malformed", "gap", True, False]:
            if shortened:
                tail = dest / ".trellis-history/event-log/cycle-000008.jsonl"
                extra = json.loads(new_log.splitlines()[-1])
                extra.update(index=count, cycle=8, event={"event":"start_cycle"})
                if shortened == "gap": extra["index"] += 100
                tail.write_text("malformed discarded future JSON\n" if shortened == "malformed" else json.dumps(extra) + "\n")
            cli_call(str(cli), request, job.env)
            assert log_path.read_bytes() == new_log
            assert sum(len(path.read_bytes().splitlines()) for path in log_path.parent.glob("cycle-*.jsonl")) == count
            assert read_json(root / "protocol_state.json") == after
            canonical = decode_shared_state(read_json(dest / ".trellis-history/supervisor_state.json"))
            assert canonical["event_count_convention"] == "record_count" and canonical["event_count"] == count
            assert canonical["state"] == after
            assert canonical["checkpoint"] == read_json(root / "checkpoint.json")
            assert canonical["metadata"] == read_json(root / "runtime_metadata.json")
            assert read_json(dest / "trellis.config.json")["repo_path"] == str(dest)
            cli_call(str(cli), {**request, "dry_run_state_path":str(replay)}, job.env)
            assert read_json(replay) == after
        # Force a real post-reset failure using an invalid historical config,
        # retaining a dirty log tail and dirty destination canonical/config.
        tag = cli_call(str(cli), {**request, "dry_run_state_path":str(replay)}, job.env)["dry_run_checkpoint_tag"]
        tag_commit = git(dest, "rev-parse", tag).decode().strip()
        bad_view = scratch / "invalid-config-checkpoint"
        git(dest, "worktree", "add", "--detach", str(bad_view), tag_commit)
        (bad_view / "trellis.config.json").write_text("invalid restored config")
        git(bad_view, "add", "trellis.config.json"); git(bad_view, "commit", "-m", "invalid replay fixture config")
        bad_commit = git(bad_view, "rev-parse", "HEAD").decode().strip()
        git(dest, "tag", "-f", tag, bad_commit)
        tail = dest / ".trellis-history/event-log/cycle-000008.jsonl"
        extra = json.loads(new_log.splitlines()[-1]); extra.update(index=count, cycle=8, event={"event":"start_cycle"})
        tail.write_text("  " + json.dumps(extra) + "\n")
        try:
            refused("artifact activation failed")
        finally:
            git(dest, "tag", "-f", tag, tag_commit)
            tail.unlink()
        run([helper, "--ignored", "--nocapture"], env={**job.env, "RUST_MIN_STACK":"67108864",
            "TRELLIS_RESUME_TEST_MODE":"load", "TRELLIS_RESUME_TEST_REPO":str(dest), "TRELLIS_RESUME_TEST_ROOT":str(root)}, capture=False)
        # Re-plan read-only admission views for the deliberately rewound HEAD;
        # ordinary Run uses its loader/epoch, not the old create-job receipt.
        views = read_json(root / "trusted-rebind-views.json")
        current_head = git(dest, "rev-parse", "HEAD").decode().strip()
        for view in views["views"].values():
            if Path(view["repo_path"]) == dest:
                view["source_commit"] = current_head
        atomic_json(root / "trusted-rebind-views.json", views)
        job.readiness()
        # A real subsequent ordinary checkpoint is paired and selectable again.
        run([helper, "--ignored", "--nocapture"], env={**job.env, "RUST_MIN_STACK":"67108864",
            "TRELLIS_RESUME_TEST_MODE":"legacy_checkpoint", "TRELLIS_RESUME_TEST_REPO":str(dest), "TRELLIS_RESUME_TEST_ROOT":str(root)}, capture=False)
        from trellis.remote_resume import checkpoint_at
        selected_again = checkpoint_at(dest, git(dest, "rev-parse", "HEAD").decode().strip())
        assert selected_again["document"]["event_count"] == count
        assert selected_again["document"]["state"]["human_input_outstanding"]
        if historical:
            assert set(after["last_clean_local_closure_records"]) == {"Preamble", "Value", "Fact"}
            assert after["last_clean_live"] != after["live"]
            with maintenance_token(root) as token:
                run([helper, "--ignored", "--nocapture"], env={**job.env, "RUST_MIN_STACK":"67108864", "TRELLIS_CHECKER_SOCKET":str(root / "sockets/checker.sock"), "TRELLIS_CHECKER_TOKEN":token, "TRELLIS_RESUME_TEST_MODE":"last_clean", "TRELLIS_RESUME_TEST_REPO":str(dest), "TRELLIS_RESUME_TEST_ROOT":str(root)}, capture=False)
            for node, record in after["last_clean_local_closure_records"].items():
                for part in record["node_certificate"]["artifact_bundle"]:
                    suffix = {"exported":"olean", "server":"olean.server", "private":"olean.private"}[part["level"]]
                    data = (dest / f".trellis/supervisor/repo/.lake/build/lib/lean/Tablet/{node}.{suffix}").read_bytes()
                    assert hashlib.sha256(data).hexdigest() == part["sha256"]
                    assert len(data) == part["size_bytes"]
        atomic_json(scratch / "evidence.json", {"ready": True, "coverage": {"live":3,"committed":3,"last_clean":3 if historical else 0}, "provider_requests":0,"replay_equal":True,"old_prefix_unchanged":True,"retry_idempotent":True,"epoch_restored":True,"historical_rollback":historical})
    finally:
        if checker is not None:
            checker.terminate(); checker.wait(timeout=15)
        subprocess.run(["tmux", "-L", socket_name, "kill-server"], capture_output=True)
        # Keep disposable evidence for review; no live run shares these paths.

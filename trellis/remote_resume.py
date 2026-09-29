"""Repo-only remote resume, run by the existing durable create-job worker.

All migration data is local. The tablet keeps its existing state/event surfaces;
no artifact transport branch, remote lease service, or provider recertification.
"""
from __future__ import annotations

import argparse
import copy
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import threading
import time
from urllib.parse import urlsplit

from trellis.history_artifacts import decode_shared_state
from trellis.launch_verification import arm_initial_launch, capture_launch_environment, initial_budget_pause, HALT_MARKERS

ROOT = Path(__file__).resolve().parents[1]
HISTORY = ".trellis-history/supervisor_state.json"
SLUG = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,63}\Z")


def atomic_json(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + f".tmp-{os.getpid()}")
    with temporary.open("w") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def read_json(path: Path):
    return json.loads(path.read_text())


def run(argv, *, cwd=None, env=None, capture=True):
    result = subprocess.run([str(a) for a in argv], cwd=cwd, env=env,
                            stdout=subprocess.PIPE if capture else None,
                            stderr=subprocess.PIPE if capture else None, check=False)
    if result.returncode:
        detail = result.stderr.decode(errors="replace")[-2000:] if result.stderr else "see create.log"
        raise RuntimeError(f"{Path(str(argv[0])).name} failed ({result.returncode}): {detail}")
    return result.stdout or b""


def git(repo: Path, *args: str) -> bytes:
    return run(["git", "-C", repo, *args], env={**os.environ, "GIT_TERMINAL_PROMPT": "0"})


def validate_remote(remote: str) -> str:
    if not remote or remote.startswith("-") or any(c.isspace() or ord(c) < 32 for c in remote):
        raise ValueError("remote must be a Git URL or an absolute local repository path")
    if remote.startswith("/"):
        return remote
    if re.fullmatch(r"[A-Za-z0-9_.-]+@[A-Za-z0-9_.-]+:[A-Za-z0-9_./-]+", remote):
        return remote
    url = urlsplit(remote)
    if url.scheme not in {"https", "ssh", "file"} or url.password or (url.scheme == "https" and url.username):
        raise ValueError("use HTTPS/SSH with a credential helper; credentials must not be embedded in the remote URL")
    if url.query or url.fragment or (url.scheme != "file" and not url.hostname):
        raise ValueError("remote URL must name a repository without query credentials or fragments")
    return remote


def scope_issue(state: dict) -> str | None:
    if state.get("tablet_target", "lean") != "lean":
        return "unsupported Isabelle/backend checkpoint; remote resume currently supports Lean math"
    if state.get("revision_context") or state.get("phase") in {"RevisionStating", "revision_stating"}:
        return "unsupported revision checkpoint"
    if (state.get("trust_base", {}).get("mode", "disabled") != "disabled" or state.get("node_role")
            or state.get("configured_challenge_targets") or state.get("pv_tablet_configured")
            or state.get("pv_live_polarity") or state.get("pv_authored_statements") or state.get("assumption_authoring")):
        return "unsupported PV/RequiredV1/challenge approval surfaces"
    if state.get("corr_fingerprint_schema_version") != 4 or state.get("sound_assessment_schema_version") != 1:
        return "unsupported correspondence/Sound checkpoint schema; use its named schema migration first"
    return None


def checkpoint_at(repo: Path, commit: str) -> dict:
    raw = git(repo, "show", f"{commit}:{HISTORY}")
    document = decode_shared_state(json.loads(raw))
    state = document.get("state")
    checkpoint = document.get("checkpoint")
    if not isinstance(state, dict) or not isinstance(checkpoint, dict):
        raise ValueError("checkpoint lacks state or summary")
    if "event_count_convention" in document and document["event_count_convention"] != "record_count":
        raise ValueError("unsupported checkpoint event-count convention")
    issue = scope_issue(state)
    if issue:
        raise ValueError(issue)
    if checkpoint.get("committed") != state.get("committed") or checkpoint.get("cycle") != state.get("cycle") or checkpoint.get("phase") != state.get("phase"):
        raise ValueError("checkpoint summary disagrees with protocol state")
    count = document.get("event_count")
    if not isinstance(count, int) or count <= 0:
        raise ValueError("checkpoint has no supported segmented event prefix")
    paths = git(repo, "ls-tree", "-r", "--name-only", commit, "--", ".trellis-history/event-log").decode().splitlines()
    events = hashlib.sha256()
    index = 0
    for path in sorted(paths):
        if not re.fullmatch(r".trellis-history/event-log/cycle-\d+\.jsonl", path):
            continue
        data = git(repo, "show", f"{commit}:{path}")
        if data and not data.endswith(b"\n"):
            raise ValueError("checkpoint event prefix ends in a partial record")
        events.update(data)
        for line in data.splitlines():
            event = json.loads(line)
            if event.get("index") != index:
                raise ValueError(f"checkpoint event prefix is not dense at index {index}")
            index += 1
    if index != count:
        raise ValueError(f"checkpoint requires {count} events but repository contains {index}")
    for tier, records_key in [("live", "local_closure_records"), ("committed", "committed_local_closure_records")]:
        for node, record in state.get(records_key, {}).items():
            if not re.fullmatch(r"[A-Za-z0-9_]+", node):
                raise ValueError("unsafe checkpoint owner name")
            data = git(repo, "show", f"{commit}:Tablet/{node}.lean")
            if hashlib.sha256(data).hexdigest() != record.get("active_decl_hash"):
                raise ValueError(f"{tier} source/checkpoint mismatch for {node}")
    return {"document": document, "state_sha256": hashlib.sha256(raw).hexdigest(),
            "event_prefix_sha256": events.hexdigest()}


def select_checkpoint(repo: Path, tip: str, requested: str | None = None) -> dict:
    lineage = git(repo, "rev-list", "--first-parent", tip).decode().splitlines()
    if requested:
        selected = git(repo, "rev-parse", "--verify", f"{requested}^{{commit}}").decode().strip()
        if selected not in lineage:
            raise ValueError("requested checkpoint is not on the fetched branch's first-parent ancestry")
        candidates = [selected]
    else:
        candidates = lineage
    skipped = []
    for commit in candidates:
        try:
            # An inherited snapshot in a later manual source commit is not a
            # publication boundary. Require the canonical snapshot to change.
            changed = git(repo, "diff-tree", "--root", "--no-commit-id", "--name-only", "-r", commit, "--", HISTORY).decode().splitlines()
            if HISTORY not in changed:
                skipped.append({"commit": commit, "reason": "no new checkpoint snapshot"})
                continue
            result = checkpoint_at(repo, commit)
            state = result["document"]["state"]
            if state.get("in_flight_request") is not None:
                skipped.append({"commit": commit, "reason": "in-flight request"})
                continue
            stage = state.get("stage")
            if stage not in {"Start", "HumanGate", "Complete", "start", "human_gate", "complete"}:
                skipped.append({"commit": commit, "reason": f"unsupported idle boundary {stage}"})
                continue
            return {"fetched_tip": tip, "selected_commit": commit, "cycle": state["cycle"],
                    "stage": stage, "phase": state["phase"], "skipped_commits": lineage.index(commit),
                    "skipped": skipped, "state_sha256": result["state_sha256"],
                    "event_prefix_sha256": result["event_prefix_sha256"]}
        except (ValueError, RuntimeError, KeyError) as error:
            skipped.append({"commit": commit, "reason": str(error)})
    detail = "; ".join(item["reason"] for item in skipped[:5])
    raise ValueError(f"no supported idle checkpoint on fetched ancestry: {detail}")


def rebind_destination_paths(config: dict, metadata: dict, repo: Path, runtime: Path) -> tuple[dict, dict]:
    config, metadata = copy.deepcopy(config), copy.deepcopy(metadata)
    old_repo = Path(metadata.get("repo_path") or config.get("repo_path") or "/__missing_source_root__")
    def relocate(value):
        if isinstance(value, dict):
            return {key: relocate(item) for key, item in value.items()}
        if isinstance(value, list):
            return [relocate(item) for item in value]
        if isinstance(value, str) and Path(value).is_absolute():
            try:
                return str(repo / Path(value).relative_to(old_repo))
            except ValueError:
                return value
        return value
    config, metadata = relocate(config), relocate(metadata)
    config["repo_path"] = str(repo)
    config["state_dir"] = ".trellis"
    config.setdefault("tmux", {})["burst_home"] = str(runtime / "burst-homes")
    config["tmux"]["session_name"] = f"trellis-{repo.name}"
    config.setdefault("chat", {})["root_dir"] = str(repo / ".trellis/chats")
    metadata["repo_path"] = str(repo)
    metadata["config_path"] = str(repo / "trellis.config.json")
    metadata["native_history_kinds"] = []
    workflow = config.get("workflow", {})
    content = [("paper", workflow.get("paper_tex_path")), ("policy", config.get("policy_path"))]
    if workflow.get("approved_axioms_path"):
        content.append(("axiom policy", workflow["approved_axioms_path"]))
    for reference in workflow.get("reference_papers", []):
        content.append(("reference paper", reference.get("tex_path")))
    for label, raw in content:
        if not raw:
            raise ValueError(f"missing required {label} path in tracked config")
        path = repo / raw
        if not path.is_file():
            raise ValueError(f"required {label} is missing on the destination: {path}; restore the mathematical content before retrying")
        if not path.resolve().is_relative_to(repo.resolve()):
            raise ValueError(f"required {label} is outside the destination repository: {path}; track and rebind this input explicitly")
    return config, metadata


def load_profile(path: Path) -> dict:
    profile = read_json(path)
    if profile.get("version") != 1:
        raise ValueError("host resume profile requires version 1")
    cli = Path(profile.get("runtime_cli", ""))
    if not cli.is_absolute() or not os.access(cli, os.X_OK):
        raise ValueError("host resume profile must select an absolute executable runtime_cli")
    env = profile.get("env", {})
    for required in ["TRELLIS_SOUNDNESS_FINGERPRINT_MODE", "TRELLIS_LOCAL_CLOSURE_AXCHECK_ENABLED"]:
        if not isinstance(env.get(required), str) or not env[required]:
            raise ValueError(f"compatible host resume profile must explicitly supply {required}")
    if any(not isinstance(k, str) or not isinstance(v, str) or any(word in k for word in ["TOKEN", "SECRET", "PASSWORD"]) for k, v in env.items()):
        raise ValueError("host profile environment must contain nonsecret string settings only")
    command = profile.get("dependency_prepare_argv")
    if not isinstance(command, list) or not command or any(not isinstance(item, str) or not item for item in command):
        raise ValueError("host profile must supply dependency_prepare_argv for its pinned Lean packages")
    return profile


def cli_call(cli: str, request: dict, env: dict) -> dict:
    result = subprocess.run([cli], input=json.dumps(request), text=True, stdout=subprocess.PIPE, env=env)
    try:
        output = json.loads(result.stdout)
    except ValueError as error:
        raise RuntimeError("runtime CLI returned invalid maintenance output") from error
    if result.returncode:
        raise RuntimeError(json.dumps(output))
    return output.get("output", output)


def claim(job_dir: Path, repo: Path, runtime: Path, slug: str, remote: str, profile_path: Path, branch=None, commit=None, handoff=False, lean_parallelism=None):
    if not profile_path or not slug or repo is None or runtime is None:
        raise ValueError("resume requires a new run name, destination paths, and a compatible host profile")
    if not SLUG.fullmatch(slug):
        raise ValueError("invalid new run name")
    validate_remote(remote)
    if not handoff:
        raise ValueError("confirm the previous writer is stopped before resuming this remote")
    profile = load_profile(profile_path)
    if lean_parallelism is not None and (isinstance(lean_parallelism, bool) or not isinstance(lean_parallelism, int) or lean_parallelism < 1):
        raise ValueError("lean_parallelism must be a positive integer")
    if branch and (branch.startswith("-") or run(["git", "check-ref-format", "--branch", branch]).decode().strip() != branch):
        raise ValueError("invalid branch")
    if commit and (commit.startswith("-") or not re.fullmatch(r"[A-Za-z0-9_./-]+", commit)):
        raise ValueError("invalid checkpoint selector")
    if repo.exists() or runtime.exists():
        raise ValueError("destination repository or runtime already exists; choose a new run name")
    job_dir.parent.mkdir(parents=True, exist_ok=True)
    job_dir.mkdir(mode=0o700)
    atomic_json(job_dir / "host-profile.json", profile)
    atomic_json(job_dir / "job.json", {"slug": slug, "kind": "resume", "create_flow": "resume", "remote_url": remote,
        "branch": branch, "commit": commit, "handoff_confirmed": True, "repo_path": str(repo), "runtime_root": str(runtime),
        "lean_parallelism": lean_parallelism, "runtime_cli_sha256": hashlib.sha256(Path(profile["runtime_cli"]).read_bytes()).hexdigest(), "created_ts": time.time()})


def source_digest(repo: Path) -> str:
    digest = hashlib.sha256()
    for path in git(repo, "ls-files", "-z").split(b"\0"):
        if not path or path.startswith(b".trellis-history/"):
            continue
        target = repo / os.fsdecode(path)
        if target.is_symlink():
            raise ValueError(f"tracked source symlink requires explicit support: {target}")
        digest.update(path + b"\0")
        digest.update(target.read_bytes())
    return digest.hexdigest()


def event_count(repo: Path) -> int:
    return sum(len(path.read_bytes().splitlines()) for path in (repo / ".trellis-history/event-log").glob("cycle-*.jsonl"))


def build_view(repo: Path, env: dict):
    workspace = repo / ".trellis/supervisor/repo"
    manifest = read_json(repo / "lake-manifest.json")
    commands = ["materialize-tablet-oleans"]
    if any(package.get("name") == "mathlib" for package in manifest.get("packages", [])):
        commands.insert(0, "prepare-compiled-support")
    for command in commands:
        result = subprocess.run([sys.executable, str(workspace / ".trellis/scripts/check.py"), command, str(workspace)],
                                env=env, stdout=subprocess.PIPE, text=True)
        payload = json.loads(result.stdout)
        for outcome in [payload, payload.get("kernel_replay", {"returncode": 0})]:
            if result.returncode or outcome.get("returncode") != 0 or outcome.get("timed_out") or outcome.get("spawn_error"):
                raise RuntimeError(f"{command} failed: {json.dumps(payload)}")
        print(f"remote resume: {command} completed", flush=True)


def process_descendants(pid: int):
    yield pid
    try:
        children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
    except OSError:
        return
    for child in children:
        yield from process_descendants(int(child))


@contextmanager
def maintenance_token(runtime: Path):
    from trellis.runtime.bridge import (_mint_burst_token, _register_burst_token,
        _burst_tokens_path, _load_burst_tokens_file, _atomic_write_burst_tokens)
    token = _mint_burst_token()
    _register_burst_token(runtime, token=token, burst_id="remote-resume-prewarm", kind="trusted_artifact_rebind", request_id=0, cycle=0)
    try:
        yield token
    finally:
        path = _burst_tokens_path(runtime)
        data = _load_burst_tokens_file(path)
        data["tokens"] = [value for value in data.get("tokens", []) if value != token]
        data["entries"] = [value for value in data.get("entries", []) if value.get("token") != token]
        _atomic_write_burst_tokens(path, data)


class ResumeJob:
    def __init__(self, job_dir: Path):
        self.directory = job_dir
        self.job = read_json(job_dir / "job.json")
        self.repo = Path(self.job["repo_path"])
        self.runtime = Path(self.job["runtime_root"])
        self.profile = load_profile(job_dir / "host-profile.json")
        expected_cli = self.job.get("runtime_cli_sha256")
        if expected_cli and hashlib.sha256(Path(self.profile["runtime_cli"]).read_bytes()).hexdigest() != expected_cli:
            raise ValueError("pinned runtime executable changed; restore the selected compatible binary")
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("TRELLIS_")}
        self.env.update(self.profile["env"])
        self.env.update({"PYTHONPATH": str(ROOT), "TRELLIS_RUNTIME_CLI": self.profile["runtime_cli"],
            "TRELLIS_TRELLIS_KERNEL_CMD": self.profile["runtime_cli"],
            "TRELLIS_KERNEL_CACHE_ROOT": str(self.runtime),
            "TRELLIS_TMUX_SOCKET": os.environ.get("TRELLIS_TMUX_SOCKET", "trellis"),
            "GIT_TERMINAL_PROMPT": "0"})
        parallelism = self.job.get("lean_parallelism") or self.profile.get("lean_parallelism")
        if not isinstance(parallelism, int) or isinstance(parallelism, bool) or parallelism < 1:
            raise ValueError("resume job or compatible host profile must supply lean_parallelism")
        for key in ["TRELLIS_LEAN_PARALLELISM", "TRELLIS_BURST_LEAN_PARALLELISM", "LEAN_NUM_THREADS"]:
            self.env[key] = str(parallelism)
        self.socket_name = self.env["TRELLIS_TMUX_SOCKET"]
        self.slug = self.job["slug"]
        self.status_lock = threading.Lock()
        self.progress = read_json(job_dir / "resume-progress.json") if (job_dir / "resume-progress.json").exists() else {}

    def save(self):
        atomic_json(self.directory / "resume-progress.json", self.progress)

    def status(self, state, stage, error=None):
        path = self.directory / "status.json"
        with self.status_lock:
            status = read_json(path) if path.exists() else {"started_ts": time.time()}
            status.update(slug=self.slug, state=state, stage=stage, phase="resume", updated_ts=time.time(), error=error)
            if "selection" in self.progress:
                status["selection"] = self.progress["selection"]
            atomic_json(path, status)
        print(f"remote resume: {stage}" + (f": {error}" if error else ""), flush=True)

    @contextmanager
    def heartbeat(self):
        stopped = threading.Event()
        def refresh():
            while not stopped.wait(10):
                with self.status_lock:
                    path = self.directory / "status.json"
                    if path.is_file():
                        status = read_json(path)
                        status["updated_ts"] = time.time()
                        atomic_json(path, status)
        thread = threading.Thread(target=refresh, daemon=True)
        thread.start()
        try:
            yield
        finally:
            stopped.set()
            thread.join()

    def tmux(self, *args, check=True):
        result = subprocess.run(["tmux", "-L", self.socket_name, *args], env=self.env, capture_output=True, text=True)
        if check and result.returncode:
            raise RuntimeError(f"tmux: {result.stderr.strip()}")
        return result

    def alive(self, name):
        return name in self.tmux("list-sessions", "-F", "#S", check=False).stdout.splitlines()

    def start_session(self, name, argv, env):
        # The launch command itself must not expose inherited credentials to
        # process listings or tmux pane command inspection. tmux's explicit
        # environment options also defeat a stale server environment.
        options = [item for key, value in env.items() for item in ("-e", f"{key}={value}")]
        self.tmux("new-session", "-d", "-s", name, *options, shlex.join(map(str, argv)))

    def remote_tip(self):
        branch = self.progress["selection"]["branch"]
        lines = run(["git", "ls-remote", "--heads", self.job["remote_url"], f"refs/heads/{branch}"], env=self.env).decode().splitlines()
        if len(lines) != 1:
            raise ValueError("selected remote branch disappeared or cannot be read")
        return lines[0].split()[0]

    def clone_select(self):
        if "selection" in self.progress:
            current = subprocess.run(["git", "-C", str(self.repo), "rev-parse", "--verify", "HEAD"], capture_output=True, text=True)
            if current.stdout.strip() != self.progress["selection"]["selected_commit"] and not self.progress.get("launched"):
                if self.adopt_checkpoint(current.stdout.strip()):
                    return
                elif not self.progress.get("checkout_complete"):
                    selected = self.progress["selection"]
                    git(self.repo, "checkout", "-B", selected["branch"], selected["selected_commit"])
                    self.progress["checkout_complete"] = True
                    self.save()
                else:
                    raise ValueError("destination source moved after checkpoint selection")
            return
        self.status("resume_cloning", "clone_select")
        # The claimed destination is initialized atomically. An interrupted
        # fetch keeps its object database and resumes into the same directory.
        if not self.repo.exists():
            self.repo.mkdir()
            git(self.repo, "init")
            (self.repo / ".trellis-creating").write_text(self.slug + "\n")
            git(self.repo, "remote", "add", "origin", self.job["remote_url"])
            with (self.repo / ".git/info/exclude").open("a") as stream:
                stream.write("\n/.trellis/\n/.trellis-creating\n")
        elif not (self.repo / ".trellis-creating").is_file():
            raise ValueError("refusing to overwrite a destination not owned by this job")
        branch = self.job.get("branch")
        if not branch:
            head = run(["git", "ls-remote", "--symref", self.job["remote_url"], "HEAD"], env=self.env).decode()
            match = re.search(r"^ref: refs/heads/(.+)\tHEAD$", head, re.M)
            if not match:
                raise ValueError("remote default branch unavailable; select a branch explicitly")
            branch = match[1]
        git(self.repo, "fetch", "--tags", "origin", f"refs/heads/{branch}:refs/remotes/origin/{branch}")
        tip = git(self.repo, "rev-parse", f"refs/remotes/origin/{branch}").decode().strip()
        selection = select_checkpoint(self.repo, tip, self.job.get("commit"))
        selection["branch"] = branch
        self.progress["selection"] = selection
        self.save()  # Pin before checkout/reconstruction; retry never refetches.
        git(self.repo, "checkout", "-b", branch, selection["selected_commit"])
        self.progress["checkout_complete"] = True
        self.save()

    def run(self):
        if self.alive(f"trellis-run-{self.slug}") or self.progress.get("launched"):
            self.verify_launch()
            return
        if self.progress.get("launch_intent"):
            if self.finish_budget_pause():
                return
            raise ValueError("a supervisor launch was already attempted and has stopped; inspect the retained runtime before starting it manually")
        self.clone_select()
        selected = self.progress["selection"]
        if self.remote_tip() != selected["fetched_tip"]:
            raise ValueError("remote branch changed since it was fetched; this job remains pinned and will not overwrite the new lineage")
        if selected["selected_commit"] != selected["fetched_tip"] and not self.job.get("rewind_confirmed"):
            self.status("resume_awaiting_rewind", "checkpoint_selected", "The selected idle checkpoint precedes the remote tip. Review the skipped history and explicitly approve the rewind before continuation.")
            return
        self.prepare()
        self.rebind()
        self.readiness()
        self.commit_checkpoint()
        # Holds have never invoked trellis.sh, but ordinary Resume needs the
        # same complete destination recipe that an active launch would capture.
        capture_launch_environment(self.runtime,
            {**self.env, "TRELLIS_CHECKER_SOCKET": str(self.runtime / "sockets/checker.sock")},
            ROOT, tmux_session=f"trellis-run-{self.slug}",
            trellis_head=git(ROOT, "rev-parse", "HEAD").decode().strip(), cwd=str(ROOT))
        state = read_json(self.runtime / "protocol_state.json")
        if state.get("human_input_outstanding") or selected["stage"] in {"HumanGate", "Complete", "human_gate", "complete"}:
            self.status("done", "complete" if selected["stage"].lower() == "complete" else "held")
            (self.repo / ".trellis-creating").unlink(missing_ok=True)
            return
        self.launch()

    def adopt_checkpoint(self, head):
        """Recover a crash after the local commit but before its job receipt."""
        if not head or not (self.runtime / "trusted-artifact-rebind-publication/complete.json").is_file():
            return False
        selected = self.progress["selection"]["selected_commit"]
        if git(self.repo, "show", "-s", "--format=%P%n%s", head).decode().splitlines() != [selected, "trellis: trusted remote resume"]:
            return False
        document = decode_shared_state(json.loads(git(self.repo, "show", f"{head}:{HISTORY}")))
        if document.get("state") != read_json(self.runtime / "protocol_state.json") or document.get("event_count") != event_count(self.repo):
            raise ValueError("local resume checkpoint differs from the retained runtime")
        self.progress["checkpoint_commit"] = head
        views_path = self.runtime / "trusted-rebind-views.json"
        views = read_json(views_path)
        for view in views["views"].values():
            if view["source_commit"] == selected and Path(view["repo_path"]) == self.repo:
                view["source_commit"] = head
        atomic_json(views_path, views)
        self.save()
        return True

    def commit_checkpoint(self):
        if self.progress.get("checkpoint_commit"):
            return
        from trellis.history_artifacts import encode_shared_state
        selection = self.progress["selection"]
        atomic_json(self.repo / ".trellis/resume-mirror.json", {"version": 1,
            "remote_url": self.job["remote_url"], "branch": selection["branch"],
            "expected_tip": selection["fetched_tip"], "initial": True,
            "allow_initial_rewind": bool(self.job.get("rewind_confirmed"))})
        config = read_json(self.repo / "trellis.config.json")
        config.setdefault("git", {}).update(remote_url=self.job["remote_url"], branch=selection["branch"])
        atomic_json(self.repo / "trellis.config.json", config)
        # Use the existing canonical sharing format before Git publication, so
        # large checkpoints do not create an oversized plain Git blob.
        document = decode_shared_state(read_json(self.repo / HISTORY))
        atomic_json(self.repo / HISTORY, encode_shared_state(document))
        git(self.repo, "config", "user.name", config["git"].get("author_name", ".trellis"))
        git(self.repo, "config", "user.email", config["git"].get("author_email", "trellis@localhost"))
        git(self.repo, "add", "--", "trellis.config.json", HISTORY, ".trellis-history/event-log")
        git(self.repo, "commit", "-m", "trellis: trusted remote resume")
        if not self.adopt_checkpoint(git(self.repo, "rev-parse", "HEAD").decode().strip()):
            raise ValueError("could not verify the local resume checkpoint")

    def checker(self, view, suffix=""):
        from trellis.atomic_actions.checker_client import CheckerRpcError, client_ping
        name = f"trellis-checker-{self.slug}{suffix}"
        runtime = Path(view["runtime_root"])
        socket = Path(view["checker_socket"])
        if len(os.fsencode(socket)) >= 108:
            raise ValueError(f"checker socket path is too long; choose a shorter destination root: {socket}")
        if not self.alive(name):
            self.start_session(name, [ROOT / "scripts/trellis_checker_server.sh", runtime,
                "--parallelism", self.env["TRELLIS_LEAN_PARALLELISM"]], self.env)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            try:
                ping = client_ping(socket, timeout_secs=2)
                if Path(ping["worker_repo"]).resolve() != Path(view["repo_path"]).resolve():
                    raise ValueError("checker socket belongs to another repository")
                pid = int(ping["server_pid"])
                if Path(f"/proc/{pid}/exe").resolve() != Path(sys.executable).resolve():
                    raise ValueError("checker executable differs from this installation's Python")
                return
            except (OSError, RuntimeError, KeyError, CheckerRpcError) as error:
                if not self.alive(name):
                    raise RuntimeError(f"destination checker exited: {error}") from error
                time.sleep(1)
        raise RuntimeError(f"destination checker did not become ready at {socket}")

    def prepare(self):
        from trellis.checking import write_scripts
        from trellis.supervisor_workspace import sync_supervisor_workspace
        if (self.runtime / "trusted-artifact-rebind-publication/complete.json").is_file():
            # Never invoke destructive reconstruction after committed evidence.
            self.progress["views"] = read_json(self.runtime / "trusted-rebind-views.json")["views"]
            for tier, view in self.progress["views"].items():
                if tier != "committed" or view != self.progress["views"]["live"]:
                    self.checker(view, "" if tier == "live" else f"-{tier}")
            return
        self.status("resume_reconstructing", "reconstruct")
        self.runtime.mkdir(parents=True, exist_ok=True)
        atomic_json(self.runtime / "remote-resume-launch.json", {"version": 1,
            "remote_url": self.job["remote_url"], "branch": self.progress["selection"]["branch"]})
        if not (self.runtime / "resume-reconstruction.json").exists():
            if self.alive(f"trellis-checker-{self.slug}"):
                raise ValueError("destination checker is active before reconstruction completed")
            # A crash after config rebinding but before the reconstruction
            # receipt can safely replay only that known path transformation.
            original = json.loads(git(self.repo, "show", "HEAD:trellis.config.json"))
            current = read_json(self.repo / "trellis.config.json")
            document = decode_shared_state(json.loads(git(self.repo, "show", f"HEAD:{HISTORY}")))
            rebound, _ = rebind_destination_paths(original, document["metadata"], self.repo, self.runtime)
            if current not in (original, rebound):
                raise ValueError("destination config changed outside the pinned resume job")
            (self.repo / "trellis.config.json").write_bytes(git(self.repo, "show", "HEAD:trellis.config.json"))
            run([ROOT / "scripts/prepare_migrated_resume.sh", self.repo, self.runtime,
                 "--reconstruct-only", "--remote-resume"], env=self.env, capture=False)
        plan = cli_call(self.profile["runtime_cli"], {"action": "plan_trusted_artifact_rebind", "root": str(self.runtime)}, self.env)
        views = self.progress.setdefault("views", {})
        for tier in ["live", "committed", "last_clean"]:
            if tier not in plan["views"]:
                continue
            specification = plan["views"][tier]
            commit = specification["source_commit"]
            if tier == "committed" and commit == plan["views"]["live"]["source_commit"]:
                views[tier] = views["live"]
                continue
            repo = self.repo if tier == "live" else self.directory / f"source-{tier}"
            runtime = self.runtime if tier == "live" else self.directory / f"runtime-{tier}"
            if not repo.exists():
                git(self.repo, "worktree", "add", "--detach", str(repo), commit)
            runtime.mkdir(parents=True, exist_ok=True)
            if tier != "live":
                config, metadata = rebind_destination_paths(read_json(repo / "trellis.config.json"),
                    {"repo_path": read_json(repo / "trellis.config.json").get("repo_path")}, repo, runtime)
                atomic_json(repo / "trellis.config.json", config)
                atomic_json(runtime / "runtime_metadata.json", metadata)
            config = read_json(repo / "trellis.config.json")
            view = {"repo_path": str(repo), "runtime_root": str(runtime),
                "paper_path": str(repo / config["workflow"]["paper_tex_path"]),
                "checker_socket": str(runtime / "sockets/checker.sock"), "source_commit": commit}
            views[tier] = view
            self.save()
            warm_key = f"warmed_{tier}"
            if not self.progress.get(warm_key):
                self.status("resume_building", f"prewarm_build:{tier}")
                frozen = source_digest(repo)
                run(self.profile["dependency_prepare_argv"], cwd=repo, env=self.env, capture=False)
                if source_digest(repo) != frozen:
                    raise ValueError("dependency preparation changed tracked source or policy; restore its pins before retrying")
                write_scripts(repo, repo / ".trellis")
                sync_supervisor_workspace(repo)
                self.checker(view, "" if tier == "live" else f"-{tier}")
                with maintenance_token(runtime) as token:
                    env = {**self.env, "TRELLIS_CHECKER_SOCKET": view["checker_socket"], "TRELLIS_CHECKER_TOKEN": token}
                    build_view(repo, env)
                self.progress[warm_key] = True
                self.save()
            else:
                self.checker(view, "" if tier == "live" else f"-{tier}")
        atomic_json(self.runtime / "trusted-rebind-views.json", {"version": 1, "views": views})
        self.save()

    def maintenance(self, option):
        argv = [sys.executable, ROOT / "scripts/migrate_local_closure_records", self.runtime,
            "--mode", "trusted-artifact-rebind", option, "--runtime-cli", self.profile["runtime_cli"],
            "--parallelism", self.env["TRELLIS_LEAN_PARALLELISM"]]
        run(argv, env=self.env, capture=False)

    def rebind(self):
        self.status("resume_rebinding", "trusted_rebind")
        self.maintenance("--apply")
        self.progress["rebound"] = True
        self.save()

    def readiness(self):
        self.status("resume_checking", "readiness")
        self.maintenance("--readiness")
        selected = self.progress["selection"]
        if self.remote_tip() != selected["fetched_tip"]:
            raise ValueError("remote branch changed during the rebuild; launch is blocked and the job remains pinned")
        self.progress["ready"] = True
        self.save()

    def launch(self):
        self.status("resume_launching", "launch_verify")
        name = f"trellis-run-{self.slug}"
        if not self.alive(name):
            selection = self.progress["selection"]
            config = read_json(self.repo / "trellis.config.json")
            config.setdefault("git", {})["remote_url"] = self.job["remote_url"]
            atomic_json(self.repo / "trellis.config.json", config)
            if not (self.repo / ".trellis/resume-mirror.json").is_file():
                raise ValueError("missing remote handoff guard; launch is blocked")
            self.progress["launch_event_count"] = event_count(self.repo)
            attempt = arm_initial_launch(self.runtime, self.repo, self.profile["runtime_cli"])
            self.progress["launch_intent"] = True
            self.save()  # If spawn succeeds but this process dies, retry sees it.
            env = {**self.env, "TRELLIS_CHECKER_SOCKET": str(self.runtime / "sockets/checker.sock"),
                   "TRELLIS_LAUNCH_ATTEMPT": attempt}
            self.start_session(name, [ROOT / "scripts/trellis.sh", "run", self.runtime], env)
        self.verify_launch()

    def finish_budget_pause(self):
        if not initial_budget_pause(self.runtime, self.repo, self.profile["runtime_cli"]):
            return False
        self.progress["launched"] = True
        self.progress["launch_outcome"] = "provider_budget_paused"
        self.save()
        (self.repo / ".trellis-creating").unlink(missing_ok=True)
        self.status("done", "paused")
        return True

    def verify_launch(self):
        name = f"trellis-run-{self.slug}"
        deadline = time.monotonic() + float(self.profile.get("launch_verify_seconds", 120))
        verified_pid = None
        while time.monotonic() < deadline:
            halts = [name for name in HALT_MARKERS if (self.runtime / name).exists()]
            if halts:
                raise RuntimeError(f"destination halted during launch: {', '.join(halts)}")
            if not self.alive(name):
                if self.finish_budget_pause():
                    return
                raise RuntimeError("destination supervisor exited; inspect its retained runtime diagnostics before retrying")
            pane = self.tmux("display-message", "-p", "-t", name, "#{pane_pid}").stdout.strip()
            for pid in process_descendants(int(pane)):
                try:
                    if Path(f"/proc/{pid}/exe").resolve() == Path(self.profile["runtime_cli"]).resolve():
                        environment = Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
                        if (f"TRELLIS_CHECKER_SOCKET={self.runtime}/sockets/checker.sock".encode() not in environment
                                or f"TRELLIS_KERNEL_CACHE_ROOT={self.runtime}".encode() not in environment):
                            raise ValueError("supervisor executable is using the wrong runtime or checker socket")
                        verified_pid = pid
                except FileNotFoundError:
                    continue
            halts = [name for name in HALT_MARKERS if (self.runtime / name).exists()]
            if halts:
                raise RuntimeError(f"destination halted during launch: {', '.join(halts)}")
            if verified_pid and event_count(self.repo) > self.progress.get("launch_event_count", 0):
                self.progress["launched"] = True
                self.progress["supervisor_pid"] = verified_pid
                self.save()
                config = read_json(self.repo / "trellis.config.json")
                if config.get("sidecar", {}).get("enabled") and not self.alive(f"trellis-sidecar-{self.slug}"):
                    self.start_session(f"trellis-sidecar-{self.slug}", [ROOT / "scripts/trellis_sidecar.sh", self.runtime, "--repo", self.repo],
                        {**self.env, "TRELLIS_CHECKER_SOCKET": str(self.runtime / "sockets/checker.sock")})
                (self.repo / ".trellis-creating").unlink(missing_ok=True)
                self.status("done", "done")
                return
            time.sleep(1)
        raise RuntimeError("supervisor started but executable identity and first progress are not yet both verified; retry will inspect the existing run")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["claim", "run", "confirm-rewind"])
    parser.add_argument("job_dir", type=Path)
    parser.add_argument("--slug")
    parser.add_argument("--repo", type=Path)
    parser.add_argument("--runtime", type=Path)
    parser.add_argument("--remote")
    parser.add_argument("--branch")
    parser.add_argument("--commit")
    parser.add_argument("--profile", type=Path)
    parser.add_argument("--handoff-confirmed", action="store_true")
    parser.add_argument("--expected-tip")
    parser.add_argument("--selected-commit")
    parser.add_argument("--lean-parallelism", type=int)
    args = parser.parse_args(argv)
    if args.action == "claim":
        claim(args.job_dir, args.repo, args.runtime, args.slug, args.remote, args.profile,
              args.branch, args.commit, args.handoff_confirmed, args.lean_parallelism)
        return
    with (args.job_dir / "resume-job.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        job = ResumeJob(args.job_dir)
        if args.action == "confirm-rewind":
            selection = job.progress["selection"]
            if args.expected_tip != selection["fetched_tip"] or args.selected_commit != selection["selected_commit"]:
                raise ValueError("rewind approval must name the exact displayed fetched tip and selected checkpoint")
            if job.remote_tip() != args.expected_tip:
                raise ValueError("remote tip changed; refusing rewind approval")
            job.job["rewind_confirmed"] = True
            atomic_json(args.job_dir / "job.json", job.job)
            return
        try:
            with job.heartbeat():
                job.run()
        except Exception as error:
            job.status("resume_failed", "failed", str(error))
            raise


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError) as error:
        print(f"remote resume: {error}", file=sys.stderr)
        sys.exit(1)

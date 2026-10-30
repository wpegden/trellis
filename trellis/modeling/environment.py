"""Operator-supplied workspace and tool identities for Modeling roles."""
from __future__ import annotations

from collections.abc import Mapping
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import time

from trellis.pv_execution import DEFAULT_TIMEOUT_SECONDS, TIMEOUT_ENV, execution_timeout, owned_process, positive_timeout

from trellis.git_ops import _git, init_repo
from trellis.json_io import save_json as write_json
from trellis.checker.admission import cooperative, cooperative_poll


def _digest(stream):
    digest = hashlib.sha256()
    for chunk in iter(lambda: stream.read(1 << 20), b''):
        cooperative_poll()
        digest.update(chunk)
    return digest.hexdigest()


def source_environment(tools: dict, output: Path, *, base_env: Mapping[str, str] | None = None) -> dict[str, str]:
    """Use configured source tools and Lean libraries with run-local storage."""
    env = dict(os.environ if base_env is None else base_env)
    for name in ("RUSTC", "RUSTDOC", "RUSTUP_TOOLCHAIN", "RUSTFLAGS", "RUSTDOCFLAGS",
                 "CARGO_ENCODED_RUSTFLAGS", "RUSTC_BOOTSTRAP", "RUSTC_WRAPPER",
                 "RUSTC_WORKSPACE_WRAPPER"):
        env.pop(name, None)
    env.update(tools.get("env", {}))
    bins = [str(Path(tools[name]).absolute().parent) for name in ("cargo", "charon", "aeneas", "lean") if name in tools]
    env["PATH"] = os.pathsep.join([*dict.fromkeys(bins), env.get("PATH", "/usr/bin:/bin")])
    env["LEAN_PATH"] = os.pathsep.join(tools.get("lean_paths", []))
    if tools.get("cargo"):
        toolchain = Path(tools["cargo"]).absolute().parent.parent
        if (toolchain / "lib/rustlib").is_dir():
            # Rustup proxies invoked by login shells must select this same toolchain.
            env["RUSTUP_TOOLCHAIN"] = str(toolchain)
        for name in ("rustc", "rustdoc"):
            path = Path(tools.get(name, Path(tools["cargo"]).with_name(name))).absolute()
            if path.is_file():
                env[name.upper()] = str(path)
    output = output.resolve()
    temporary = output / "tmp"
    temporary.mkdir(parents=True, exist_ok=True)
    env.update(CARGO_TARGET_DIR=str(output / "target"), CARGO_HOME=str(output / "cargo-home"),
               CHARON_TOOLCHAIN_IS_IN_PATH="1", TMPDIR=str(temporary), TMP=str(temporary), TEMP=str(temporary))
    return env


def source_burst_environment(repo: Path, output: Path) -> dict[str, str]:
    """Project source tooling for ordinary sandboxed agents; inert for math runs."""
    revision_tools = None
    revision_state = repo / "state.json"
    if revision_state.is_file() and (repo / "sources/original").is_dir():
        state = json.loads(revision_state.read_text())
        if state.get("version") == 1 and "original_collection" in state and "negated_goals" in state:
            revision_tools = state.get("tools", {})
    if revision_tools is None and not ((repo / "modeling-input/manifest.json").is_file()
            or (repo / ".trellis/modeling/state.json").is_file()):
        return {}
    from trellis.pv_toolchain import effective_tools

    env = source_environment(revision_tools if revision_tools is not None else effective_tools(repo), output, base_env={})
    shell = output / "shell-env.sh"
    # Bash reads BASH_ENV again for agent tool calls, after the provider
    # launcher has set PATH. Keep ordinary Lean commands on the managed
    # route when reapplying the source toolchain in those child shells.
    shims = shlex.quote(str(repo / ".trellis/scripts/check-bin"))
    shell.write_text(f"export PATH={shlex.quote(env['PATH'])}:\"$PATH\"\n"
                     f'if [ -n "${{TRELLIS_CHECK_JOBS_SOCKET:-}}" ] || [ "${{TRELLIS_COMPILER_CAPABILITY:-}}" = managed ] || [ "${{TRELLIS_COMPILER_CAPABILITY:-}}" = none ]; then export PATH={shims}:"$PATH"; fi\n')
    env["BASH_ENV"] = str(shell)
    # HOME and temporary storage are owned by the shared sandbox. PATH has a
    # separate carrier because provider startup scripts establish their own PATH.
    env["TRELLIS_SOURCE_PATH"] = env.pop("PATH")
    if TIMEOUT_ENV in os.environ:
        env[TIMEOUT_ENV] = os.environ[TIMEOUT_ENV]
    for name in ("HOME", "TMPDIR", "TMP", "TEMP"):
        env.pop(name, None)
    return env


def run_source_command(argv: list[str], *, cwd: Path, env: dict[str, str], stdout: Path,
                       stderr: Path | None = None, timeout: float = DEFAULT_TIMEOUT_SECONDS) -> dict:
    """Retain command evidence and terminate the entire job on timeout/cancel."""
    if not argv:
        raise ValueError("Supply a command")
    timeout = positive_timeout(timeout)
    stdout.parent.mkdir(parents=True, exist_ok=True)
    if stderr is not None:
        stderr.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    timed_out = False
    with stdout.open("w") as output:
        error_output = stderr.open("w") if stderr is not None else None
        try:
            try:
                with owned_process(argv, compute_timeout=timeout, cwd=cwd, env=env, stdout=output,
                                   stderr=error_output or subprocess.STDOUT) as process:
                    code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                code, timed_out = process.returncode, True
                (error_output or output).write(f"\nTool timed out after {timeout:g}s; its process group was terminated.\n")
            except OSError as error:
                (error_output or output).write(str(error) + "\n")
                code = -1
        finally:
            if error_output is not None:
                error_output.close()
    return {"argv": argv, "cwd": str(cwd), "exit_code": code, "timeout": timed_out, "timeout_seconds": timeout,
            "seconds": round(time.monotonic() - started, 3), "stdout": str(stdout),
            "stderr": str(stderr or stdout), "environment": {
                name: env[name] for name in ("PATH", "LEAN_PATH", "RUSTC", "RUSTDOC", "RUSTUP_TOOLCHAIN",
                                             "CARGO_HOME", "CARGO_TARGET_DIR", "TMPDIR") if name in env}}


def tool_readonly_paths(tools: dict) -> list[str]:
    variants = [tools, *({**tools, **profile} for profile in tools.get("profiles", {}).values())]
    return list(dict.fromkeys(path for variant in variants for path in [
        *variant.get("readonly", []), *variant.get("lean_paths", []),
        *(str(Path(variant[name]).resolve().parent.parent)
          for name in ("cargo", "rustc", "rustdoc", "charon", "aeneas", "lean") if name in variant),
    ]))


@cooperative
def environment_signature(tools: dict, *, cache_path: Path | None = None,
                          read_caches: list[Path] = ()) -> str:
    """Hash executables and Lean loadable artifacts, excluding build bookkeeping.

    Stat metadata only avoids rereading unchanged files.
    Cache paths must be protected from the agents whose work is checked.
    An unavailable cache is an optimization failure, not a validation failure.
    """
    digest = hashlib.sha256()
    previous = {}
    for cached in [*read_caches, *([cache_path] if cache_path else [])]:
        try:
            payload = json.loads(cached.read_text())
            if payload.get("schema") == "trellis-resource-digests/v1":
                previous.update(payload["files"])
        except (OSError, ValueError, KeyError, TypeError):
            pass
    current = {}
    paths = [Path(tools[name]).resolve() for name in ("cargo", "charon", "aeneas", "lean") if name in tools]
    cargo = tools.get("cargo")
    if cargo:
        for name in ("rustc", "rustdoc"):
            path = Path(tools.get(name, Path(cargo).with_name(name)))
            if path.is_file():
                paths.append(path)
    artifacts = (".olean", ".olean.private", ".olean.server", ".ilean", ".ir", ".so", ".dll", ".dylib")
    for root in tools.get("lean_paths", []):
        paths.extend(sorted(path for path in Path(root).rglob("*")
                            if path.name.endswith(artifacts) and path.is_file()))
    def stamp(stat):
        return [stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns]

    for path in dict.fromkeys(paths):
        cooperative_poll()
        name = str(path)
        metadata = stamp(path.stat())
        cached = previous.get(name, {})
        if (isinstance(cached, dict) and cached.get("stat") == metadata
                and isinstance(cached.get("sha256"), str) and len(cached["sha256"]) == 64):
            content = cached["sha256"]
        else:
            with path.open("rb") as stream:
                before = stamp(os.fstat(stream.fileno()))
                content = _digest(stream)
                after = stamp(os.fstat(stream.fileno()))
            if before != after or after != stamp(path.stat()):
                raise OSError(f"Resource changed while hashing: {path}")
            metadata = after
        current[name] = {"stat": metadata, "sha256": content}
        digest.update(f"{name}\0{content}\n".encode())
    if cache_path and current != previous:
        try:
            write_json(cache_path, {"schema": "trellis-resource-digests/v1", "files": current})
        except OSError:
            pass
    return digest.hexdigest()


@cooperative
def identify_tools(tools: dict, *, timeout: float | None = None) -> dict:
    """Record the selected executables and extractor capabilities."""
    executables = {name: tools[name] for name in ("cargo", "charon", "aeneas", "lean") if name in tools}
    if tools.get("cargo"):
        for name in ("rustc", "rustdoc"):
            path = Path(tools.get(name, Path(tools["cargo"]).with_name(name)))
            if path.is_file():
                executables[name] = str(path)
    identities = {}
    for name, configured in executables.items():
        path = Path(configured).resolve()
        with path.open("rb") as stream:
            digest = _digest(stream)
        identities[name] = {"path": str(path), "sha256": digest}
        if name == "aeneas":
            help_result = subprocess.run([str(path), "-help"], capture_output=True, text=True,
                                         timeout=execution_timeout(tools, "tool_probe", timeout, default=30),
                                         env={**os.environ, **tools.get("env", {})})
            identities[name]["emit_json"] = "-emit-json" in help_result.stdout + help_result.stderr
    return identities


def prepare_environment(root: Path, tools: dict) -> None:
    identities = identify_tools(tools)
    write_json(root / "tool-identities.json", identities)
    init_repo(root)
    (root / ".gitignore").write_text(
        "/*\n!/.gitignore\n!/upstream/\n!/work/\n!/GOAL.md\n!/config.json\n"
        "!/tool-identities.json\n!/goals.json\n!/trellis.config.json\n"
        "!/.trellis/\n/.trellis/*\n!/.trellis/modeling/\n/.trellis/modeling/*\n"
        "!/.trellis/modeling/state.json\n**/target/\n**/.lake/\n**/__pycache__/\n"
    )
    paper = json.loads((root / "config.json").read_text()).get("paper_input")
    referents = ["paper", "paper-targets.json"] if paper else ["GOAL.md", "goals.json"]
    if paper:
        with (root / ".gitignore").open("a") as stream:
            stream.write("!/paper/\n!/paper-targets.json\n")
    _git(root, "add", ".gitignore", "upstream", "work", *referents, "config.json", "tool-identities.json",
         "trellis.config.json", ".trellis/modeling/state.json")
    if paper:
        from trellis.paper_implementation import stage_paper_inputs
        stage_paper_inputs(root, paper)
    _git(root, "commit", "--quiet", "-m", "Record Modeling inputs and initial candidate")

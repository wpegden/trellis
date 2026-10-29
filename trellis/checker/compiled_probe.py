"""Protected, toolchain-bound compilation of trusted Lean collectors.

Only the collector (``import Lean``) is compiled here, never project sources
or a Lake configuration. Candidate imports still happen in the existing
confined probe invocation. The cache lives outside every compiler/worker
writable overlay, alongside the pinned source collector.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
from typing import Mapping, Sequence

from trellis.checker import telemetry
from trellis.checker.admission import admitted, check_cancelled, guarded_run, lean_threads


def _hash(path: Path) -> str:
    if path.is_symlink():
        raise RuntimeError("symlink in compiled closure collector")
    return hashlib.sha256(path.read_bytes()).hexdigest()


def lake_probe_command(lean: Path, program: Path, arguments: Sequence[str],
                       directory: Path | None) -> list[str]:
    command = [str(lean), "--run", str(program), *arguments]
    launch = 'lean="$1"; shift; exec "$lean" "-j${LEAN_NUM_THREADS:-1}" "$@"'
    if directory is None:
        return ["env", "/bin/sh", "-c", launch, "trellis-closure", *command]
    # Lake PREPENDS candidate artifact directories to LEAN_PATH. Set the
    # protected collector path first only AFTER Lake constructs that path,
    # or a candidate could shadow our hash-named collector module. All paths
    # and arguments are positional shell parameters, never shell source.
    return ["env", "/bin/sh", "-c",
            'export LEAN_PATH="$1${LEAN_PATH:+:$LEAN_PATH}"; shift; ' + launch,
            "trellis-closure", str(directory), *command]


class CompiledClosureProbe:
    collector = "closure"
    module_prefix = "TrellisClosure"
    runner_name = "RunClosure.lean"
    switch = "TRELLIS_CLOSURE_PRECOMPILE"

    def __init__(self, repo: Path) -> None:
        self.root = repo / f".trellis/{self.collector}-collectors/compiled-v1"
        self._lock = threading.Lock()

    @telemetry.measured("compiled_probe_prepare")
    def prepare(
        self, source: Path, identity: Mapping[str, str], lean: Path,
        *, timeout_secs: float,
    ) -> tuple[Path, Path | None]:
        # Operational rollback retains the original, source-pinned execution
        # path and every acceptance check. It never changes proof policy.
        check_cancelled()
        if os.environ.get(self.switch, "1") == "0":
            return source, None
        inputs = {name: identity[name] for name in (
            "checker_script_hash", "toolchain_hash", "lean_executable_hash"
        )}
        if _hash(source) != inputs["checker_script_hash"]:
            raise RuntimeError("pinned collector changed before compilation")
        if _hash(lean) != inputs["lean_executable_hash"]:
            raise RuntimeError("Lean executable changed before collector compilation")
        key = hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()
        module = f"{self.module_prefix}_{key}"
        runner = f"import {module}\n".encode()
        directory = self.root / key
        with self._lock:
            check_cancelled()
            self.root.mkdir(parents=True, exist_ok=True)
            if not directory.exists() and not directory.is_symlink():
                self._compile(directory, module, runner, source, lean, inputs, timeout_secs)
            self._validate(directory, module, runner, inputs)
        return directory / self.runner_name, directory

    @admitted
    def _compile(self, directory, module, runner, source, lean, inputs, timeout_secs):
        temporary = Path(tempfile.mkdtemp(prefix=".compile-", dir=self.root))
        try:
            collector = temporary / f"{module}.lean"
            collector.write_bytes(source.read_bytes())
            if _hash(collector) != inputs["checker_script_hash"]:
                raise RuntimeError("collector changed while preparing compilation")
            (temporary / self.runner_name).write_bytes(runner)
            # A direct invocation of the pinned compiler in this trusted-only
            # directory avoids running project Lake code or loading candidate
            # modules. Do not inherit search-path overrides from a run.
            env = {k: v for k, v in os.environ.items()
                   if not k.startswith("LEAN_") and k != "ELAN_TOOLCHAIN"}
            env["LEAN_NUM_THREADS"] = lean_threads()
            result = guarded_run(
                [str(lean), f"-j{lean_threads()}", "-o", str(temporary / f"{module}.olean"), str(collector)],
                cwd=temporary, env=env, capture_output=True, text=True,
                timeout=max(1.0, min(timeout_secs, 120.0)),
            )
            if result.returncode:
                raise RuntimeError(f"collector compilation failed: {result.stderr[-2000:]} {result.stdout[-2000:]}")
            if _hash(lean) != inputs["lean_executable_hash"]:
                raise RuntimeError("Lean executable changed during collector compilation")
            names = [f"{module}.lean", self.runner_name, f"{module}.olean"]
            names += [f"{module}.olean{suffix}" for suffix in (".server", ".private")
                      if (temporary / f"{module}.olean{suffix}").exists()]
            receipt = {"inputs": inputs, "files": {name: _hash(temporary / name) for name in names}}
            (temporary / "receipt.json").write_text(json.dumps(receipt, sort_keys=True) + "\n")
            self._validate(temporary, module, runner, inputs)
            for path in temporary.iterdir():
                with path.open("rb") as stream:
                    os.fchmod(stream.fileno(), 0o444)
                    os.fsync(stream.fileno())
            fd = os.open(temporary, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
            # Directory rename publishes the source, wrapper, every olean
            # part and receipt together. A concurrent producer must agree.
            try:
                temporary.rename(directory)
            except OSError:
                if not directory.is_dir():
                    raise
                self._validate(directory, module, runner, inputs)
            fd = os.open(self.root, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        finally:
            if temporary.exists():
                shutil.rmtree(temporary)

    @classmethod
    def _validate(cls, directory, module, runner, inputs):
        if directory.is_symlink() or not directory.is_dir():
            raise RuntimeError("invalid compiled collector directory")
        receipt_path = directory / "receipt.json"
        _hash(receipt_path)  # Reject symlinks before opening metadata.
        receipt = json.loads(receipt_path.read_text())
        if not isinstance(receipt, dict):
            raise RuntimeError("invalid compiled collector receipt")
        required = {f"{module}.lean", f"{module}.olean", cls.runner_name}
        allowed = required | {f"{module}.olean.server", f"{module}.olean.private"}
        files = receipt.get("files", {})
        if (receipt.get("inputs") != inputs or not isinstance(files, dict)
                or not required <= files.keys() <= allowed
                or {p.name for p in directory.iterdir()} != files.keys() | {"receipt.json"}):
            raise RuntimeError("compiled collector receipt mismatch")
        if f"{module}.olean.private" in files and f"{module}.olean.server" not in files:
            raise RuntimeError("orphan private collector artifact")
        for name, expected in files.items():
            if _hash(directory / name) != expected:
                raise RuntimeError("compiled collector artifact digest mismatch")
        if (files[f"{module}.lean"] != inputs["checker_script_hash"]
                or (directory / cls.runner_name).read_bytes() != runner
                or not (directory / f"{module}.olean").stat().st_size):
            raise RuntimeError("compiled collector source/wrapper mismatch")


class CompiledSemanticProbe(CompiledClosureProbe):
    collector = "semantic"
    module_prefix = "TrellisSemantic"
    runner_name = "RunSemantic.lean"
    switch = "TRELLIS_SEMANTIC_PRECOMPILE"

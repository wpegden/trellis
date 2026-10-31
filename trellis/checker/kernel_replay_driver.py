"""Pinned LeanChecker discovery with sequential module replay.

Only the exact reviewed upstream source is transformed. Other toolchains keep
using their stock executable, one target per invocation. Discovery and scheduling
change; each module still runs the unchanged upstream replayFromImports check.
"""
from __future__ import annotations

from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile

from .compiled_probe import CompiledClosureProbe

UPSTREAM_SHA256 = 'eb5dee411837629f09c5c18d63cc833d30335a46048bc586642742e90aa65d5f'
_DISCOVERY = '''  for target in targets do
    let mut found := false
    for path in (← SearchPath.findAllWithExt sp "olean") do
      if let some m := (← searchModuleNameOfFileName path sp) then
        if !fresh && target.isPrefixOf m || target == m then
          targetModules := targetModules.insert m
          found := true
    if not found then
      throw <| IO.userError s!"Could not find any oleans for: {target}"
'''
_SINGLE_PASS = '''  -- Resolve the same ordered discovery list once for this invocation.
  let mut discovered : Array Name := #[]
  for path in (← SearchPath.findAllWithExt sp "olean") do
    if let some m := (← searchModuleNameOfFileName path sp) then
      discovered := discovered.push m
  for target in targets do
    let mut found := false
    for m in discovered do
      if !fresh && target.isPrefixOf m || target == m then
        targetModules := targetModules.insert m
        found := true
    if not found then
      throw <| IO.userError s!"Could not find any oleans for: {target}"
'''

_PARALLEL_REPLAY = '''    let mut tasks := #[]
    for m in targetModules do
      tasks := tasks.push (m, ← IO.asTask (replayFromImports m))
    for (m, t) in tasks do
      if verbose then IO.println s!"replaying {m}"
      if let .error e := t.get then
        IO.eprintln s!"leanchecker found a problem in {m}"
        throw e
'''
_SEQUENTIAL_REPLAY = '''    for m in targetModules do
      if verbose then IO.println s!"replaying {m}"
      try
        replayFromImports m
      catch e =>
        IO.eprintln s!"leanchecker found a problem in {m}"
        throw e
'''


_INIT = '  initSearchPath (← findSysroot)\n'
_RESTORE_SEARCH_PATH = _INIT + """  -- Driver imports were loaded only from the protected cache and sysroot.
  -- Replay/discovery must retain the original Lake path and precedence.
  if let some original ← IO.getEnv "TRELLIS_KERNEL_REPLAY_SEARCH_PATH" then
    searchPathRef.set (System.SearchPath.parse original ++ (← getBuiltinSearchPath (← findSysroot)))
"""


def protected_command(lean: Path, program: Path, modules: list[str], directory: Path) -> list[str]:
    sysroot = lean.parent.parent
    return ['env', '/bin/sh', '-c',
            'export TRELLIS_KERNEL_REPLAY_SEARCH_PATH="${LEAN_PATH-}"; '
            'export LEAN_SYSROOT="$2"; '
            'export LEAN_PATH="$1:$2/lib/lean${LEAN_PATH:+:$LEAN_PATH}"; '
            'shift 2; lean="$1"; shift; exec "$lean" "-j${LEAN_NUM_THREADS:-1}" "$@"',
            'trellis-kernel-replay', str(directory), str(sysroot),
            str(lean), '--run', str(program), *modules]


def sha(path: Path) -> str:
    if path.is_symlink():
        raise RuntimeError('aliased replay driver input')
    return hashlib.sha256(path.read_bytes()).hexdigest()


def transformed_source(source: bytes) -> bytes | None:
    if hashlib.sha256(source).hexdigest() != UPSTREAM_SHA256:
        return None
    text = source.decode('utf-8')
    if text.count(_DISCOVERY) != 1 or text.count(_INIT) != 1 or text.count(_PARALLEL_REPLAY) != 1:
        raise RuntimeError('reviewed LeanChecker discovery block changed')
    return (text.replace(_DISCOVERY, _SINGLE_PASS, 1)
            .replace(_INIT, _RESTORE_SEARCH_PATH, 1)
            .replace(_PARALLEL_REPLAY, _SEQUENTIAL_REPLAY, 1).encode('utf-8'))


def execution_inputs(sysroot: Path) -> list[Path]:
    """Execution-local inputs, including the transformer and trusted compiler."""
    upstream = sysroot / 'src/lean/LeanChecker.lean'
    if not upstream.is_file() or sha(upstream) != UPSTREAM_SHA256:
        return []
    return [Path(__file__).resolve(), Path(__file__).with_name('compiled_probe.py').resolve(), upstream]


class CompiledReplayDriver(CompiledClosureProbe):
    collector = 'kernel-replay'
    module_prefix = 'TrellisKernelReplay'
    runner_name = 'RunKernelReplay.lean'
    switch = 'TRELLIS_KERNEL_REPLAY_PRECOMPILE'


@dataclass
class PreparedReplay:
    command: list[str]
    lake: Path
    evidence: dict[str, str]
    files: dict[Path, str]
    directory: Path | None = None
    directory_identity: tuple[int, int] | None = None

    def command_for(self, modules: list[str]) -> list[str]:
        """Use the same protected driver for an independently admitted batch."""
        end = self.command.index('--run') + 2  # Include the pinned runner.
        return [*self.command[:end], *modules]

    def validate(self) -> None:
        if self.directory is not None:
            metadata = self.directory.lstat()
            if (self.directory.is_symlink()
                    or (metadata.st_dev, metadata.st_ino) != self.directory_identity
                    or set(self.directory.iterdir()) != {p for p in self.files if p.parent == self.directory}):
                raise RuntimeError('kernel replay driver cache membership changed')
        if any(sha(path) != digest for path, digest in self.files.items()):
            raise RuntimeError('kernel replay driver changed during invocation')


def prepare(repo: Path, modules: list[str], *, timeout_secs: float) -> PreparedReplay | None:
    # A single namespace target can expand to many modules. It needs the
    # sequential driver too. Unsupported/disabled drivers use isolated stock
    # invocations at the caller, never an uncompiled modified-source path.
    if not modules or os.environ.get('TRELLIS_KERNEL_REPLAY_PRECOMPILE', '1') == '0':
        return None
    toolchain = repo / 'lean-toolchain'
    if not toolchain.is_file():
        return None
    from trellis.host_runtime import resolve_elan_executable, worker_elan_home
    from trellis.supervisor_workspace import authoritative_env_for_repo
    try:
        elan_home = worker_elan_home()
        env = {**os.environ, **authoritative_env_for_repo(repo), 'ELAN_HOME': str(elan_home)}
        elan = resolve_elan_executable(elan_home, env)
        paths = []
        for name in ('lean', 'lake', 'leanchecker'):
            result = subprocess.run([str(elan), 'which', name], cwd=repo, env=env,
                                    text=True, capture_output=True, timeout=15, check=True)
            selected = result.stdout.strip()
            path = Path(selected)
            if not selected or len(selected.splitlines()) != 1 or not path.is_absolute():
                raise RuntimeError('invalid replay executable resolution')
            paths.append(path.resolve(strict=True))
    except (OSError, subprocess.SubprocessError, RuntimeError):
        # Resolution is an optimization precondition. Let the unchanged
        # stock invocation report unavailable/unsupported toolchains.
        return None
    lean, lake, checker = paths
    sysroot = lean.parent.parent
    if any(path.parent != sysroot / 'bin' for path in paths):
        return None
    upstream = sysroot / 'src/lean/LeanChecker.lean'
    if not upstream.is_file():
        return None
    original = upstream.read_bytes()
    generated = transformed_source(original)
    if generated is None:
        return None
    if any(sha(path) != expected for path, expected in _IMPLEMENTATION_INPUTS.items()):
        raise RuntimeError('loaded replay driver implementation changed')
    bindings = [*execution_inputs(sysroot), toolchain, *paths]
    files = {path: sha(path) for path in bindings}
    if files.get(upstream) != UPSTREAM_SHA256:
        raise RuntimeError('LeanChecker changed before driver preparation')
    identity = dict(checker_script_hash=hashlib.sha256(generated).hexdigest(),
                    toolchain_hash=files[toolchain], lean_executable_hash=files[lean])
    compiler = CompiledReplayDriver(repo)
    compiler.root.mkdir(parents=True, exist_ok=True)
    # The transient input and compiled cache are host-owned, outside compiler
    # writable workspace overlays (the same custody as closure collectors). CompiledClosureProbe authenticates every
    # source/runner/olean byte on each use and publishes immutable files.
    with tempfile.TemporaryDirectory(prefix='.source-', dir=compiler.root) as temporary:
        source = Path(temporary) / 'Replay.lean'
        source.write_bytes(generated)
        program, directory = compiler.prepare(source, identity, lean, timeout_secs=timeout_secs)
        if directory is None:
            raise RuntimeError('replay driver compilation unexpectedly disabled')
    files.update({path: sha(path) for path in directory.iterdir()})
    module = f"{compiler.module_prefix}_{directory.name}"
    compiler._validate(directory, module, f"import {module}\n".encode(), identity)
    evidence = dict(mode='sequential_discovery_v2', upstream_sha256=UPSTREAM_SHA256,
                    driver_sha256=identity['checker_script_hash'],
                    generator_sha256=files[Path(__file__).resolve()])
    metadata = directory.lstat()
    prepared = PreparedReplay(protected_command(lean, program, modules, directory), lake, evidence, files,
                              directory, (metadata.st_dev, metadata.st_ino))
    prepared.validate()
    return prepared


def replay_batches(nodes, bundles, width):
    """Bounded, roughly balanced work; every requested module remains covered.

    Artifact size is only a scheduling hint. One batch per execution slot
    amortizes repeated imports without paying a process startup per module.
    Each driver still uses the sequential upstream replay loop.
    """
    if width <= 1 or len(nodes) <= 1:
        return [list(nodes)]
    weights = {node: max(1, sum(part.get('size_bytes', 0)
                               for part in bundles.get(node, []) or [])) for node in nodes}
    # Upstream accepts namespace prefixes, so a parent and its requested
    # descendants must stay together: its per-process discovery deduplicates
    # their overlapping targets. Splitting them would repeat child proofs.
    requested, groups = set(nodes), {}
    for node in nodes:
        parts = node.split('.')
        owner = next(('.'.join(parts[:i]) for i in range(1, len(parts))
                      if '.'.join(parts[:i]) in requested), node)
        groups.setdefault(owner, []).append(node)
    count = min(len(groups), width)
    batches, sizes = [[] for _ in range(count)], [0] * count
    for group in sorted(groups.values(), key=lambda group: -sum(weights[n] for n in group)):
        index = min(range(count), key=lambda i: sizes[i])
        batches[index].extend(group)
        sizes[index] += sum(weights[node] for node in group)
    return batches


# Bind the loaded Python implementation, not later bytes substituted after
# module import. The native acceptance identity also binds these sources.
_IMPLEMENTATION_INPUTS = {
    path: sha(path) for path in (
        Path(__file__).resolve(), Path(__file__).with_name('compiled_probe.py').resolve())
}

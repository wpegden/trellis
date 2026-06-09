"""Provider connectivity preflight for Trellis.

Runs a fast, end-to-end check against the providers a project is *actually*
configured to use, so auth / model / sandbox problems surface at first setup
instead of mid-run. Two layers:

1. **Sandbox + toolchain probe** (no API cost). Runs the real worker bwrap
   sandbox and asserts: bwrap works; ``lake``/``lean``/``python3`` resolve on
   the worker PATH; every configured provider CLI (``codex``/``claude``/
   ``gemini``) resolves there too; and the repo root is write-protected. This
   is the Gate H/I surface — the class of failure where ``setup_repo.sh``
   passes but the worker burst can't find its tools.

2. **Per-provider structured-output burst** (uses the provider API). For each
   distinct ``(provider, model, effort)`` the config uses, dispatch one minimal
   real burst that asks the agent to emit a tiny JSON file, then verify it
   round-trips. This confirms auth works, the model string is valid, and the
   headless agent actually produces structured output — exactly the things
   that otherwise fail on the first live cycle.

Run::

    python3 -m trellis.provider_check --config path/to/trellis.config.json

Add ``--sandbox-only`` to skip the API bursts (layer 1 only, free).
"""

from __future__ import annotations

import argparse
import json
import os
import secrets
import shlex
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Tuple

from trellis.adapters import ProviderConfig
from trellis.burst import run_worker_burst
from trellis.burst_home import seed_burst_home
from trellis.config import Config, load_config
from trellis.host_runtime import provider_cli_not_found_detail, worker_path_env
from trellis.sandbox import probe_worker_environment, wrap_command
from trellis.worker_scratch import worker_scratch_dir


# --------------------------------------------------------------------------
# Lane enumeration
# --------------------------------------------------------------------------

def _as_provider_config(obj) -> ProviderConfig:
    """Coerce a verification-agent config into a ProviderConfig."""
    if isinstance(obj, ProviderConfig):
        return obj
    return ProviderConfig(
        provider=str(getattr(obj, "provider", "") or ""),
        model=getattr(obj, "model", None),
        effort=getattr(obj, "effort", None),
        extra_args=list(getattr(obj, "extra_args", []) or []),
        fallback_models=list(getattr(obj, "fallback_models", []) or []),
    )


def lane_configs(cfg: Config) -> List[Tuple[str, ProviderConfig]]:
    """Every (lane-label, ProviderConfig) the run can dispatch."""
    lanes: List[Tuple[str, ProviderConfig]] = []

    def add(label: str, value) -> None:
        if value is None:
            return
        lanes.append((label, _as_provider_config(value)))

    add("worker", cfg.worker)
    add("easy_worker", cfg.easy_worker)
    add("hard_worker", cfg.hard_worker)
    add("blockered_worker", cfg.blockered_worker)
    add("easy_close_worker", cfg.easy_close_worker)
    add("reviewer", cfg.reviewer)

    ver = cfg.verification
    if ver is not None:
        add("verification", ProviderConfig(provider=ver.provider, model=ver.model,
                                           extra_args=list(ver.extra_args or [])))
        for i, agent in enumerate(ver.correspondence_agents or []):
            add(agent.label or f"correspondence[{i}]", agent)
        for i, agent in enumerate(ver.soundness_agents or []):
            add(agent.label or f"soundness[{i}]", agent)

    return lanes


def _signature(pc: ProviderConfig) -> Tuple[str, str, str, Tuple[str, ...]]:
    return (
        str(pc.provider or "").strip().lower(),
        str(pc.model or "").strip(),
        str(pc.effort or "").strip(),
        tuple(pc.extra_args or []),
    )


@dataclass
class LaneGroup:
    config: ProviderConfig
    labels: List[str]

    @property
    def title(self) -> str:
        parts = [self.config.provider or "?"]
        if self.config.model:
            parts.append(self.config.model)
        if self.config.effort:
            parts.append(f"effort={self.config.effort}")
        return "/".join(parts)


def distinct_groups(lanes: List[Tuple[str, ProviderConfig]]) -> List[LaneGroup]:
    """Collapse lanes that share a (provider, model, effort, extra_args)."""
    groups: dict = {}
    order: List[Tuple] = []
    for label, pc in lanes:
        sig = _signature(pc)
        if sig not in groups:
            groups[sig] = LaneGroup(config=pc, labels=[])
            order.append(sig)
        groups[sig].labels.append(label)
    return [groups[sig] for sig in order]


# --------------------------------------------------------------------------
# Layer 1: sandbox + toolchain probe
# --------------------------------------------------------------------------

@dataclass
class CheckResult:
    name: str
    ok: bool
    detail: str = ""
    seconds: float = 0.0


def run_sandbox_probe(cfg: Config, burst_home: Path,
                      provider_names: List[str]) -> CheckResult:
    start = time.monotonic()
    ok, detail = probe_worker_environment(
        sandbox=cfg.sandbox,
        repo_path=cfg.repo_path,
        burst_home=burst_home,
        provider_commands=provider_names,
        certify_checker_surface=False,
    )
    return CheckResult(
        name="sandbox + toolchain (bwrap, lake/lean, provider CLIs on PATH)",
        ok=ok,
        detail="" if ok else detail,
        seconds=time.monotonic() - start,
    )


_LEAN_PROBE_TRIVIAL = "example : True := trivial\n"
_LEAN_PROBE_MATHLIB = (
    "import Tablet.Preamble\n"
    "import Mathlib.Data.Nat.Basic\n\n"
    "example : Nat.succ 0 = 1 := rfl\n"
)


def run_lean_compile(cfg: Config, burst_home: Path, *, deep: bool,
                     timeout: float) -> CheckResult:
    """Compile a probe Lean file inside the real worker sandbox.

    ``deep=False`` compiles a no-import file (verifies lake/lean actually
    execute under elan in-sandbox). ``deep=True`` imports Tablet.Preamble +
    Mathlib (verifies mathlib is materialized and reachable).
    """
    repo = cfg.repo_path.resolve()
    scratch = worker_scratch_dir(repo)
    scratch.mkdir(parents=True, exist_ok=True)
    probe = scratch / "_provider_check_probe.lean"
    probe.write_text(_LEAN_PROBE_MATHLIB if deep else _LEAN_PROBE_TRIVIAL,
                     encoding="utf-8")
    rel = probe.relative_to(repo)
    label = "lake env lean (mathlib import)" if deep else "lake env lean (toolchain)"
    start = time.monotonic()
    try:
        inner = ["/bin/bash", "-c", f"lake env lean {shlex.quote(str(rel))}"]
        cmd = wrap_command(inner, sandbox=cfg.sandbox, work_dir=repo,
                           burst_home=burst_home, role="worker")
        env = dict(os.environ)
        env["PATH"] = worker_path_env(burst_home)
        proc = subprocess.run(cmd, capture_output=True, text=True,
                              env=env, timeout=timeout)
        ok = proc.returncode == 0
        detail = "" if ok else (proc.stderr or proc.stdout
                                or f"exit {proc.returncode}").strip()
    except subprocess.TimeoutExpired:
        ok, detail = False, f"timed out after {int(timeout)}s"
    except Exception as exc:  # pragma: no cover - defensive
        ok, detail = False, str(exc)
    finally:
        probe.unlink(missing_ok=True)
    return CheckResult(name=label, ok=ok, detail=detail,
                       seconds=time.monotonic() - start)


# --------------------------------------------------------------------------
# Layer 2: structured-output burst per provider group
# --------------------------------------------------------------------------

_PROMPT_TEMPLATE = (
    "This is an automated connectivity test for the Trellis harness. Do exactly "
    "one thing, then stop.\n\n"
    "Write a file at the relative path `{rel}` whose entire contents are this "
    "single line of JSON:\n\n"
    '{{"trellis_provider_check": "ok", "nonce": "{nonce}"}}\n\n'
    "Write only that file. Do not run other commands, edit other files, or ask "
    "questions. Once the file is written you are finished."
)


def run_agent_burst(group: LaneGroup, *, work_dir: Path, burst_home: Path,
                    sandbox, index: int, timeout: float) -> CheckResult:
    repo = work_dir.resolve()
    scratch = worker_scratch_dir(repo)
    scratch.mkdir(parents=True, exist_ok=True)
    nonce = secrets.token_hex(8)
    out_file = scratch / f"_provider_check_{index}.json"
    out_file.unlink(missing_ok=True)
    rel = out_file.relative_to(repo)
    prompt = _PROMPT_TEMPLATE.format(rel=str(rel), nonce=nonce)

    start = time.monotonic()
    result = run_worker_burst(
        group.config,
        prompt,
        session_name=f"trellis-provider-check-{index}",
        work_dir=repo,
        timeout_seconds=timeout,
        startup_timeout_seconds=timeout,
        max_rate_limit_retries=0,
        session_scope="provider_check",
        fresh=True,
        done_file=out_file,
        artifact_prefix=f"provider_check_{index}",
        sandbox=sandbox,
        burst_home=burst_home,
    )
    seconds = time.monotonic() - start

    # Prefer a precise "CLI not found on PATH" message when that's the cause.
    if not result.ok:
        detail = provider_cli_not_found_detail(
            group.config.provider,
            exit_code=result.exit_code,
            output=result.captured_output or "",
            burst_home=burst_home,
        )
        if not detail:
            detail = (result.error or (result.captured_output or "")[-400:]
                      or f"burst failed (exit {result.exit_code})").strip()
        out_file.unlink(missing_ok=True)
        return CheckResult(name=group.title, ok=False, detail=detail, seconds=seconds)

    # Burst returned ok — verify the structured output actually round-tripped.
    if not out_file.exists():
        return CheckResult(name=group.title, ok=False, seconds=seconds,
                           detail="burst completed but no structured-output file "
                                  f"was written at {rel}")
    try:
        payload = json.loads(out_file.read_text(encoding="utf-8"))
    except Exception as exc:
        return CheckResult(name=group.title, ok=False, seconds=seconds,
                           detail=f"structured-output file is not valid JSON: {exc}")
    finally:
        out_file.unlink(missing_ok=True)

    if not isinstance(payload, dict) or payload.get("nonce") != nonce:
        return CheckResult(name=group.title, ok=False, seconds=seconds,
                           detail="structured output did not echo the expected "
                                  f"nonce (got {payload!r})")
    return CheckResult(name=group.title, ok=True, seconds=seconds)


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------

def _resolve_config_path(args) -> Path:
    if args.config:
        return Path(args.config).expanduser().resolve()
    repo = Path(args.repo).expanduser().resolve() if args.repo else Path.cwd()
    candidate = repo / "trellis.config.json"
    if candidate.is_file():
        return candidate
    raise SystemExit(
        f"no --config given and no trellis.config.json under {repo}; "
        "pass --config <path>"
    )


def _print_header(text: str) -> None:
    print(f"\n=== {text} ===")


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python3 -m trellis.provider_check",
        description="Preflight the providers a Trellis project is configured to use.",
    )
    parser.add_argument("--config", help="path to trellis.config.json")
    parser.add_argument("--repo", help="project repo (looks for trellis.config.json inside)")
    parser.add_argument("--sandbox-only", action="store_true",
                        help="run only the no-API sandbox/toolchain probe")
    parser.add_argument("--no-lean-compile", action="store_true",
                        help="skip the in-sandbox `lake env lean` compile probe")
    parser.add_argument("--lean-mathlib", action="store_true",
                        help="deepen the lean probe to import Tablet.Preamble + Mathlib")
    parser.add_argument("--lanes", help="comma-separated lane labels to restrict to")
    parser.add_argument("--timeout", type=float, default=240.0,
                        help="per-burst timeout in seconds (default 240)")
    parser.add_argument("--keep", action="store_true",
                        help="keep the provider-check runtime dir for debugging")
    args = parser.parse_args(argv)

    # Stream progress line-by-line even when stdout is redirected to a file or
    # piped (e.g. through `tee`/`tail`), so a long-running lane isn't invisible
    # until the process exits.
    try:
        sys.stdout.reconfigure(line_buffering=True)
    except Exception:
        pass

    config_path = _resolve_config_path(args)
    cfg = load_config(config_path)
    print(f"config:    {config_path}")
    print(f"repo:      {cfg.repo_path}")
    print(f"sandbox:   enabled={cfg.sandbox.enabled} backend={cfg.sandbox.backend}")

    lanes = lane_configs(cfg)
    if args.lanes:
        wanted = {s.strip() for s in args.lanes.split(",") if s.strip()}
        lanes = [(label, pc) for label, pc in lanes if label in wanted]
        if not lanes:
            raise SystemExit(f"--lanes matched no configured lanes (have: "
                             f"{', '.join(l for l, _ in lane_configs(cfg))})")
    groups = distinct_groups(lanes)
    provider_names = sorted({g.config.provider for g in groups if g.config.provider})

    print(f"providers: {', '.join(provider_names) or '(none)'}")
    print(f"distinct provider configs: {len(groups)}")
    for g in groups:
        print(f"  - {g.title}  ({', '.join(g.labels)})")

    # The runtime root (and thus the burst home) must live OUTSIDE the repo:
    # the worker sandbox ro-binds the whole repo, which would shadow the
    # writable home bind and make HOME read-only in-sandbox. Mirror production,
    # where the runtime root is a sibling of the repo, by using a dedicated
    # cache dir keyed by repo name.
    repo_key = "".join(ch if ch.isalnum() or ch in "-._" else "_"
                       for ch in cfg.repo_path.name) or "repo"
    runtime_root = (Path.home() / ".cache" / "trellis" / "provider-check"
                    / repo_key).resolve()
    shutil.rmtree(runtime_root, ignore_errors=True)
    runtime_root.mkdir(parents=True, exist_ok=True)
    burst_home = seed_burst_home(runtime_root, "provider-check")

    results: List[CheckResult] = []
    try:
        _print_header("Layer 1: sandbox + toolchain (no API cost)")
        r = run_sandbox_probe(cfg, burst_home, provider_names)
        results.append(r)
        _emit(r)

        if not args.no_lean_compile:
            r = run_lean_compile(cfg, burst_home, deep=args.lean_mathlib,
                                 timeout=max(args.timeout, 300.0))
            results.append(r)
            _emit(r)

        if not args.sandbox_only:
            _print_header("Layer 2: structured-output burst per provider (uses API)")
            agent_work = runtime_root / "agent-work"
            agent_work.mkdir(parents=True, exist_ok=True)
            for i, g in enumerate(groups):
                print(f"\n-> {g.title} ...", flush=True)
                r = run_agent_burst(g, work_dir=agent_work, burst_home=burst_home,
                                    sandbox=cfg.sandbox, index=i, timeout=args.timeout)
                results.append(r)
                _emit(r)
        else:
            print("\n(--sandbox-only: skipping per-provider API bursts)")
    finally:
        if not args.keep:
            shutil.rmtree(runtime_root, ignore_errors=True)

    _print_header("Summary")
    failed = [r for r in results if not r.ok]
    for r in results:
        mark = "PASS" if r.ok else "FAIL"
        print(f"  [{mark}] {r.name}  ({r.seconds:.1f}s)")
        if not r.ok and r.detail:
            print(f"         {r.detail}")
    if failed:
        print(f"\n{len(failed)} of {len(results)} checks FAILED.")
        return 1
    print(f"\nAll {len(results)} checks passed.")
    return 0


def _emit(r: CheckResult) -> None:
    mark = "PASS" if r.ok else "FAIL"
    print(f"[{mark}] {r.name}  ({r.seconds:.1f}s)")
    if not r.ok and r.detail:
        print(f"       {r.detail}")


if __name__ == "__main__":
    sys.exit(main())

"""Grunt attempt runner CLI (queue redesign §2.3).

One PROCESS per attempt, spawned by the manager with
``start_new_session=True`` so SIGTERM/SIGKILL to the process group
takes the whole attempt down — including the warm lean server, which
the compile loop spawns directly in-group. The pipeline is the
extracted body of the old ``__main__._make_attempt_fn``: refresh own
grunt workspace → giant check → compile loop → driver → prevalidate →
publish-on-success, then write an outcome JSON for the manager and
exit. Killed mid-publish is safe: publish is an atomic rename out of
``inflight/`` and the manager sweeps that grunt's ``inflight/``
leftovers on cancel (F12).

Usage::

    python3 -m trellis.sidecar.attempt <runtime_root> \
        --grunt K --node N --entry-seq S --snapshot SHA \
        --node-file-sha H --statement-prefix-sha H2 \
        --attempt-id ID --result OUT.json [--config trellis.config.json]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
from typing import Any, Dict

import time

from trellis.sidecar import spool as spool_mod
from trellis.sidecar.config import SidecarConfig, read_api_key
from trellis.sidecar.codex_driver import run_attempt_codex
from trellis.sidecar.driver import (
    attempt_timings,
    build_attempt_record,
    build_system_prompt_v2,
    split_body_marker,
)


def _git_dirty_tablet_paths(repo):
    """Tablet paths with uncommitted changes, or None if git is unusable.

    Used as a restamp guard: a stamp asserts an olean was built from the
    sources on disk, which is only true if nobody edited those sources.
    """
    import subprocess

    try:
        proc = subprocess.run(
            ["git", "-C", str(repo), "status", "--porcelain", "--", "Tablet"],
            capture_output=True,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    return [line[3:].strip() for line in (proc.stdout or "").splitlines() if line.strip()]


def run_grunt_attempt(
    runtime_root: Path,
    *,
    grunt: int,
    node: str,
    entry_seq: int,
    snapshot_sha: str,
    node_file_sha: str,
    statement_prefix_sha: str,
    attempt_id: str,
    config: SidecarConfig,
) -> Dict[str, Any]:
    from trellis.sidecar.compile_loop import (
        SidecarCompileLoop,
        make_compile_callback,
    )
    from trellis.sidecar.prevalidate import prevalidate_success
    from trellis.sidecar.workspace import (
        prewarm_node_closure,
        purge_stale_oleans_for,
        restamp_rebuilt_oleans,
        refresh_to_snapshot,
        workspace_repo_path,
    )

    # The runner's stdout/stderr is the manager-provisioned
    # ``attempt-<id>.log`` (``__main__._make_spawn_fn``), so a plain
    # flushed print IS this attempt's log sink.
    def log(message: str) -> None:
        print(f"sidecar[{attempt_id}]: {message}", flush=True)

    phase_timings: Dict[str, float] = {}
    phase_started = time.monotonic()
    refresh = refresh_to_snapshot(runtime_root, snapshot_sha, grunt=grunt)
    phase_timings["refresh_secs"] = time.monotonic() - phase_started
    if not refresh.ok:
        return {
            "status": "workspace_idle",
            "attempt_id": attempt_id,
            "detail": refresh.reason,
        }
    repo = workspace_repo_path(runtime_root, grunt=grunt)
    purged = purge_stale_oleans_for(repo, node)
    # Rebuild what the purge removed BEFORE the agent's clock starts, then
    # stamp the sidecars so the next attempt does not purge the same set
    # again. Without the stamp the purge is a ratchet: it deletes the
    # `.olean.srcclosure` along with the olean, `olean_is_content_current`
    # fails closed on the missing sidecar, and the node is re-purged every
    # attempt forever. Measured on the live grunts, 85 of 118 purges were
    # this ratchet and 0 of `smallbico`'s 16 were genuinely stale.
    if purged:
        prewarm_ok, prewarm_secs, prewarm_tail = prewarm_node_closure(repo, node, config)
        log(
            f"prewarm: rebuilt {len(purged)} purged olean(s) in {prewarm_secs:.0f}s "
            f"off the agent wall (ok={prewarm_ok})"
        )
        if not prewarm_ok:
            log(f"prewarm did not finish clean (non-fatal): {prewarm_tail[:180]}")
        # Restamp only if the agent has not yet run and the tree is clean
        # beyond the target, so a stamp can only vouch for an olean this
        # attempt's own build produced from the sources on disk.
        dirty = [
            p
            for p in (_git_dirty_tablet_paths(repo) or [])
            if p != f"Tablet/{node}.lean"
        ]
        if dirty:
            log(f"restamp skipped: worktree dirty beyond the target ({dirty[:3]})")
        else:
            stamped = restamp_rebuilt_oleans(repo, node, purged)
            log(f"restamp: {stamped} srcclosure sidecar(s) written (breaks the re-purge ratchet)")

    loop = SidecarCompileLoop(repo, config)
    try:
        giant = loop.giant_reason(node)
        if giant is not None:
            return {
                "status": "skipped_giant",
                "attempt_id": attempt_id,
                "detail": giant,
            }
        node_content = (repo / "Tablet" / f"{node}.lean").read_text(encoding="utf-8")
        # Assignment-base sanity: the manager assigned this generation
        # against the export's hashes; a mismatch here means the
        # workspace snapshot diverged from the assignment (rewind race)
        # — fail fast instead of burning API tokens on a base the
        # kernel will reject as stale anyway.
        actual_sha = hashlib.sha256(node_content.encode("utf-8")).hexdigest()
        if node_file_sha and actual_sha != node_file_sha:
            return {
                "status": "failed",
                "attempt_id": attempt_id,
                "detail": "assignment base hash != workspace bytes at snapshot",
            }
        prefix, initial_body = split_body_marker(node_content)
        api_key = read_api_key(config)
        if api_key is None:
            return {
                "status": "error",
                "attempt_id": attempt_id,
                "detail": "api key unavailable",
            }
        # The codex CLI runs its own agent loop and its own `lake build`
        # in this workspace. Two consequences shape this path: nothing may
        # hold a warm lean server on the tree while it builds (`.lake`
        # contention), and the sidecar's info tools are unreachable to it
        # — so the prompt is built with every tool flag OFF, keeping the
        # mathematical content (NL proof, dependency statements, approved
        # axioms) while dropping the advertisement of tools this agent
        # cannot call.
        #
        # Only `attempt_wall_seconds` is enforceable here; the token,
        # iteration and compaction budgets belonged to the retired HTTP
        # loop and have no analogue in `codex exec`.
        log(
            f"codex-cli arm ({config.model_name}); wall cap "
            f"{config.attempt_wall_seconds:.0f}s is the binding budget"
        )
        result = run_attempt_codex(
            config=config,
            repo=repo,
            node=node,
            node_content=node_content,
            system_prompt=build_system_prompt_v2(
                repo,
                node,
                node_content,
                search_enabled=False,
                goals_enabled=False,
                mathlib_source_enabled=False,
                # This agent has a shell, not tool calls. Without this
                # the prompt still emits a numbered workflow built on
                # `read_file` / `search_tablet` / `lean_run_code` —
                # three mechanisms it does not have — which
                # `build_codex_task` then contradicts.
                tools_available=False,
            ),
            initial_body=initial_body,
            compile_body_factory=lambda: (
                loop.open_node(node, node_content),
                make_compile_callback(loop, node, node_content),
            )[1],
            codex_home=Path(config.codex_home) if config.codex_home else None,
            log=log,
        )
        # F5: the v3 long-regime telemetry rides on EVERY outcome, not
        # just the published success record — failures are precisely the
        # population you diagnose from (did it compact? was effort in
        # force? how many transport stalls?).
        outcome: Dict[str, Any] = {
            "status": result.status,
            "attempt_id": attempt_id,
            "detail": result.detail,
            "iterations": result.iterations,
            "prompt_tokens": result.prompt_tokens,
            "completion_tokens": result.completion_tokens,
            "wall_secs": result.wall_secs,
            "compactions": result.compactions,
            "compaction_round_tokens": list(result.compaction_round_tokens),
            "transport_retries": result.transport_retries,
            "reasoning_effort": result.reasoning_effort,
            # v4: retrieval-surface usage, per tool kind.
            "info_tool_calls": result.info_tool_calls,
            "info_tool_counts": dict(sorted(result.info_tool_counts.items())),
        }
        tool_usage = ", ".join(
            f"{name}={count}" for name, count in sorted(result.info_tool_counts.items())
        )
        # The WHY of a non-success rides the operator-visible line, not
        # just the ledger row: a bare "attempt error" left the cause
        # reconstructible only after the fact.
        failed = result.status != "success" and bool(result.detail)
        why = f" ({result.detail})" if failed else ""
        log(
            f"attempt {result.status}{why} after {result.iterations} iterations, "
            f"{result.wall_secs:.1f}s, {result.compactions} compactions, "
            f"{result.transport_retries} transport retries, effort="
            f"{result.reasoning_effort or 'off'}, info tools: "
            f"{tool_usage or 'none'}"
        )
        timings = attempt_timings(result, extra=phase_timings)
        if purged:
            # Freshness-regression signal (§3.1 of the bench report):
            # a healthy warm workspace purges ~1 node per attempt, not
            # the whole transitive closure.
            timings["purged_oleans"] = len(purged)
        if timings:
            outcome["timings"] = timings
        if result.status != "success":
            loop.restore(node, node_content)
            return outcome
        # Post-loop failures (prevalidate/record/publish crashes) are
        # contained into an `error` outcome so the manager still
        # ledgers the tokens already spent by the model loop.
        try:
            pre = prevalidate_success(
                repo=repo,
                node=node,
                config=config,
                pre_image=node_content,
                proof_body=result.proof_body,
            )
            # PREVALIDATE NO LONGER VETOES. It computes the record's
            # fingerprints and rides along as telemetry; the kernel's
            # gate sequence decides.
            #
            # It used to reject here, and that is where a whole class of
            # bug lived: prevalidate re-derives, in Python, three kernel
            # gates it has no access to (the node evaluation, the
            # local-closure probe, the approved-axiom set), and any place
            # that re-derivation came out STRICTER than the kernel, a
            # compiling proof was discarded with no artifact left
            # anywhere. Four instances found so far — a whole-log `sorry`
            # grep, a whole-log `sorryAx` grep, the transitive
            # `#print axioms` walk, and probe timeouts an order of
            # magnitude tighter than the kernel's, rejecting where the
            # kernel DEFERS. `perfect` lost `summary5` (a genuine closure
            # the retry loop had earned) and `linegraph2_5` this way.
            #
            # Restating the invariant did not hold the line, because
            # nothing enforced it. Removing the veto enforces it
            # structurally: the daemon can no longer be stricter than the
            # kernel, because it no longer decides. A divergence now
            # lands in `rejected/<gate>` with a reason the reviewer can
            # read, instead of vanishing.
            #
            # The published set is already high quality — a driver
            # `success` means the body compiled through `check_body` AND
            # the confirming `lake build` — so the cost is bounded: a bad
            # record occupies one `max_applies_per_boundary` slot for one
            # boundary. That is far cheaper than throwing away a 283s /
            # 1.37M-token closure.
            if not pre.ok:
                log(
                    "prevalidate advisory (publishing anyway; the kernel "
                    f"gates decide): {'; '.join(pre.reasons)[:300]}"
                )
            record = build_attempt_record(
                config=config,
                attempt_id=attempt_id,
                node=node,
                entry_seq=entry_seq,
                snapshot_sha=snapshot_sha,
                candidate={
                    "node_file_sha256": node_file_sha,
                    "statement_prefix_sha256": statement_prefix_sha,
                },
                workspace_fingerprints=pre.fingerprints,
                result=result,
                daemon_validation=pre.daemon_validation(),
                timings=phase_timings,
            )
            dirs = spool_mod.spool_dirs(runtime_root)
            spool_mod.ensure_spool_dirs(dirs)
            spool_mod.publish_attempt(dirs, record)
        except Exception as exc:  # noqa: BLE001 — containment boundary
            outcome["status"] = "error"
            outcome["detail"] = f"post-loop failure: {type(exc).__name__}"
            try:
                loop.restore(node, node_content)
            except OSError:
                pass  # workspace is a cache; the next refresh resets it
            return outcome
        loop.restore(node, node_content)
        return outcome
    finally:
        loop.shutdown()


def _write_result(path: Path, outcome: Dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(outcome, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, path)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="trellis.sidecar.attempt")
    parser.add_argument("runtime_root", type=Path)
    parser.add_argument("--grunt", type=int, required=True)
    parser.add_argument("--node", required=True)
    parser.add_argument("--entry-seq", type=int, required=True)
    parser.add_argument("--snapshot", required=True)
    parser.add_argument("--node-file-sha", default="")
    parser.add_argument("--statement-prefix-sha", default="")
    parser.add_argument("--attempt-id", required=True)
    parser.add_argument("--result", type=Path, required=True)
    parser.add_argument("--config", type=Path, default=None)
    args = parser.parse_args(argv)
    config = SidecarConfig.load(args.config) if args.config else SidecarConfig(enabled=True)
    try:
        outcome = run_grunt_attempt(
            args.runtime_root,
            grunt=args.grunt,
            node=args.node,
            entry_seq=args.entry_seq,
            snapshot_sha=args.snapshot,
            node_file_sha=args.node_file_sha,
            statement_prefix_sha=args.statement_prefix_sha,
            attempt_id=args.attempt_id,
            config=config,
        )
    except Exception as exc:  # noqa: BLE001 — the manager needs a result file
        outcome = {
            "status": "error",
            "attempt_id": args.attempt_id,
            "detail": f"attempt runner crashed: {type(exc).__name__}: {exc}",
        }
    _write_result(args.result, outcome)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

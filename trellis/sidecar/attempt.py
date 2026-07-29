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
from trellis.sidecar.driver import (
    ModelClient,
    attempt_timings,
    build_attempt_record,
    build_system_prompt_v2,
    info_tool_specs,
    make_info_tool_handlers,
    mathlib_source_root,
    run_attempt,
    split_body_marker,
)


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
        make_goal_tool_handler,
    )
    from trellis.sidecar.prevalidate import prevalidate_success
    from trellis.sidecar.workspace import (
        purge_stale_oleans_for,
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
        # F4: the client's fallback notices (e.g. "reasoning_effort
        # rejected") reach this attempt's log instead of a no-op sink —
        # a silent downgrade out of the trained regime is otherwise
        # invisible after the fact.
        client = ModelClient(config, api_key, log=log)
        phase_started = time.monotonic()
        loop.open_node(node, node_content)
        phase_timings["server_open_secs"] = time.monotonic() - phase_started
        # v2 info tools (grunt-bench): the NL proof stays on disk,
        # read on demand; loogle search only when the host configures
        # a loogle server (absent tool otherwise). v3 adds get_goals
        # (goal state over the same warm server; degrades gracefully).
        search_enabled = bool(config.loogle_enabled)
        goals_enabled = True
        # v4 retrieval widening: the tablet grep is always available;
        # the mathlib source grep exactly when this workspace carries a
        # mathlib checkout.
        mathlib_source_enabled = mathlib_source_root(repo) is not None
        info_tools = make_info_tool_handlers(
            repo,
            node,
            search_enabled=search_enabled,
            mathlib_source_enabled=mathlib_source_enabled,
        )
        info_tools["get_goals"] = make_goal_tool_handler(loop, node)

        def on_compaction(round_index: int, tokens: int, _summary: str) -> None:
            log(
                f"context compaction round {round_index} at "
                f"{tokens} cumulative tokens"
            )

        result = run_attempt(
            config=config,
            client=client,
            system_prompt=build_system_prompt_v2(
                repo,
                node,
                node_content,
                search_enabled=search_enabled,
                goals_enabled=goals_enabled,
                mathlib_source_enabled=mathlib_source_enabled,
            ),
            initial_body=initial_body,
            compile_body=make_compile_callback(loop, node, node_content),
            info_tools=info_tools,
            extra_tool_specs=info_tool_specs(
                search_enabled=search_enabled,
                goals_enabled=goals_enabled,
                mathlib_source_enabled=mathlib_source_enabled,
            ),
            on_compaction=on_compaction,
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
            if not pre.ok:
                loop.restore(node, node_content)
                outcome["status"] = "failed"
                outcome["detail"] = "; ".join(pre.reasons)[:500]
                return outcome
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

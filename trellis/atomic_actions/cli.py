"""CLI for atomic trellis checker actions."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Optional, Sequence

from .checker_client import (
    CheckerRpcError,
    _resolve_socket_path,
    client_local_closure_axioms,
)
from .observations import (
    LEAN_SUPPORT_TIMEOUT_SECS,
    build_tablet,
    compile_node,
    materialize_tablet_oleans,
    observe_lean_semantic_payloads,
    prepare_compiled_support,
    print_axioms,
)
from .tablet_support import sync_tablet_support


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="trellis-atomic-actions")
    subparsers = parser.add_subparsers(dest="command", required=True)

    compile_parser = subparsers.add_parser("lean-compile-node")
    compile_parser.add_argument("node_name")
    compile_parser.add_argument("repo_path", nargs="?", default=".")
    compile_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    build_parser = subparsers.add_parser("lean-build-tablet")
    build_parser.add_argument("repo_path", nargs="?", default=".")
    build_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    prepare_parser = subparsers.add_parser("prepare-compiled-support")
    prepare_parser.add_argument("repo_path", nargs="?", default=".")
    prepare_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    materialize_parser = subparsers.add_parser("materialize-tablet-oleans")
    materialize_parser.add_argument("repo_path", nargs="?", default=".")
    materialize_parser.add_argument("--node", action="append", default=[])
    materialize_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    axioms_parser = subparsers.add_parser("print-axioms")
    axioms_parser.add_argument("node_name")
    axioms_parser.add_argument("repo_path", nargs="?", default=".")
    axioms_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    # Patch A local-closure probe (LOCAL_CLOSURE_IMPL_PLAN.md §5.7).
    # Server-only op: there is no host-lake fallback. The subcommand
    # routes through ``client_local_closure_axioms`` when
    # ``TRELLIS_CHECKER_SOCKET`` is set; otherwise it errors loudly so
    # operator misconfiguration is surfaced rather than silently masked.
    local_closure_parser = subparsers.add_parser("local-closure-axioms")
    local_closure_parser.add_argument("node_name")
    local_closure_parser.add_argument("repo_path", nargs="?", default=".")
    local_closure_parser.add_argument(
        "--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS
    )
    # Plan §4.6.1 kill-switch: when set, the server appends
    # ``--no-axcheck`` to the Lean script CLI so the secondary
    # axiomization collector is skipped. The Rust kernel wrapper sets
    # this when the bridge config flag
    # ``local_closure_axcheck_enabled`` is false.
    local_closure_parser.add_argument(
        "--no-axcheck", action="store_true", default=False
    )

    payload_parser = subparsers.add_parser("lean-semantic-payloads")
    payload_parser.add_argument("repo_path", nargs="?", default=".")
    payload_parser.add_argument("--node", action="append", default=[])
    payload_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    support_parser = subparsers.add_parser("sync-tablet-support")
    support_parser.add_argument("repo_path", nargs="?", default=".")
    support_parser.add_argument("--render-json", required=True)

    args = parser.parse_args(list(argv if argv is not None else sys.argv[1:]))

    if args.command == "lean-compile-node":
        payload = compile_node(
            Path(args.repo_path).resolve(),
            args.node_name,
            timeout_secs=args.timeout_secs,
        )
    elif args.command == "lean-build-tablet":
        payload = build_tablet(
            Path(args.repo_path).resolve(),
            timeout_secs=args.timeout_secs,
        )
    elif args.command == "prepare-compiled-support":
        payload = prepare_compiled_support(
            Path(args.repo_path).resolve(),
            timeout_secs=args.timeout_secs,
        )
    elif args.command == "materialize-tablet-oleans":
        payload = materialize_tablet_oleans(
            Path(args.repo_path).resolve(),
            args.node,
            timeout_secs=args.timeout_secs,
        )
    elif args.command == "print-axioms":
        payload = print_axioms(
            Path(args.repo_path).resolve(),
            args.node_name,
            timeout_secs=args.timeout_secs,
        )
    elif args.command == "local-closure-axioms":
        # Server-only op (LOCAL_CLOSURE_IMPL_PLAN.md §5.7). No
        # host-lake fallback: the trust model (plan §2.3) requires the
        # server to derive ``repo_path`` from its socket runtime root,
        # so a worker-side direct invocation has no meaning.
        socket_path = _resolve_socket_path()
        if socket_path is None:
            print(
                json.dumps(
                    {
                        "error": (
                            "local-closure-axioms is a server-only op; "
                            "set TRELLIS_CHECKER_SOCKET to route through "
                            "the supervisor-side checker server "
                            "(no host-lake fallback per plan §5.7)"
                        ),
                    }
                )
            )
            return 2
        try:
            response = client_local_closure_axioms(
                socket_path,
                args.node_name,
                timeout_secs=args.timeout_secs,
                no_axcheck=getattr(args, "no_axcheck", False),
            )
        except CheckerRpcError as exc:
            print(json.dumps({"error": f"{exc.kind}: {exc.message}"}))
            return 2
        payload = dict(response)
        payload.pop("request_id", None)
    elif args.command == "lean-semantic-payloads":
        payload = observe_lean_semantic_payloads(
            Path(args.repo_path).resolve(),
            args.node,
            timeout_secs=args.timeout_secs,
        )
    elif args.command == "sync-tablet-support":
        # `--render-json -` sentinel: read the payload from stdin. The kernel
        # always uses this path because the rendered INDEX/README JSON for a
        # ~420-node tablet otherwise overflows ARG_MAX at spawn time.
        if args.render_json == "-":
            render_text = sys.stdin.read()
        else:
            render_text = args.render_json
        payload = sync_tablet_support(
            Path(args.repo_path).resolve(),
            json.loads(render_text),
        )
    else:
        parser.error(f"unknown command: {args.command}")
        return 2

    print(json.dumps(payload))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

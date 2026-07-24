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
    client_isabelle_build_session,
    client_isabelle_check_node,
    client_isabelle_sync_session,
    client_isabelle_thm_deps,
    client_isabelle_thm_oracles,
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
    # FIX 2 coverage: when set, the server appends ``--scan-only`` to the
    # Lean script CLI so it runs ONLY the owner-file authored-name scan
    # (no closure walk, no axcheck). The Rust kernel sets this to run the
    # universal, all-node-kind authoring gate on every changed/new node
    # file — including Definition-kind nodes the full closure probe skips.
    local_closure_parser.add_argument(
        "--scan-only", action="store_true", default=False
    )

    payload_parser = subparsers.add_parser("lean-semantic-payloads")
    payload_parser.add_argument("repo_path", nargs="?", default=".")
    payload_parser.add_argument("--node", action="append", default=[])
    payload_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    support_parser = subparsers.add_parser("sync-tablet-support")
    support_parser.add_argument("repo_path", nargs="?", default=".")
    support_parser.add_argument("--render-json", required=True)

    # Isabelle/HOL checker subcommands (B2a). Additive — the eight Lean
    # subcommands above are byte-untouched. Each is server-only (no host
    # fallback): the AF_UNIX checker server holds the Isabelle TCP+password
    # internally and derives the repo from its socket runtime root. The
    # kernel's PROVISIONAL ``ISABELLE_OP_*`` strings (backend.rs) are these
    # subcommand names; B2b finalizes the per-node routing.
    isa_check_parser = subparsers.add_parser("isabelle-check-node")
    isa_check_parser.add_argument("node_name")
    isa_check_parser.add_argument("repo_path", nargs="?", default=".")
    isa_check_parser.add_argument(
        "--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS
    )

    isa_oracles_parser = subparsers.add_parser("isabelle-thm-oracles")
    isa_oracles_parser.add_argument("node_name")
    isa_oracles_parser.add_argument("repo_path", nargs="?", default=".")
    isa_oracles_parser.add_argument(
        "--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS
    )

    isa_deps_parser = subparsers.add_parser("isabelle-thm-deps")
    isa_deps_parser.add_argument("node_name")
    isa_deps_parser.add_argument("repo_path", nargs="?", default=".")
    isa_deps_parser.add_argument(
        "--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS
    )
    # Accept (and ignore) ``--scan-only``: the kernel's provisional Isabelle
    # arm reuses the local-closure arg-builder, which may append it. The
    # Isabelle cert is whole-theorem, so scan-only is a no-op flag here;
    # accepting it keeps the kernel arg-vec valid without a parse error.
    isa_deps_parser.add_argument("--scan-only", action="store_true", default=False)
    isa_deps_parser.add_argument("--no-axcheck", action="store_true", default=False)

    isa_build_parser = subparsers.add_parser("isabelle-build-session")
    isa_build_parser.add_argument("node_name", nargs="?", default=None)
    isa_build_parser.add_argument("repo_path", nargs="?", default=".")
    isa_build_parser.add_argument(
        "--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS
    )

    isa_sync_parser = subparsers.add_parser("isabelle-sync-session")
    isa_sync_parser.add_argument("node_name", nargs="?", default=None)
    isa_sync_parser.add_argument("repo_path", nargs="?", default=".")
    isa_sync_parser.add_argument(
        "--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS
    )

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
                scan_only=getattr(args, "scan_only", False),
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
    elif args.command in {
        "isabelle-check-node",
        "isabelle-thm-oracles",
        "isabelle-thm-deps",
        "isabelle-build-session",
        "isabelle-sync-session",
    }:
        # Server-only ops (mirrors local-closure-axioms): the trust model
        # requires the server to derive the repo from its socket runtime
        # root and to hold the Isabelle TCP+password internally, so a
        # worker-side direct invocation has no meaning. Error loudly when
        # the socket is unset rather than silently masking misconfig.
        socket_path = _resolve_socket_path()
        if socket_path is None:
            print(
                json.dumps(
                    {
                        "error": (
                            f"{args.command} is a server-only op; set "
                            "TRELLIS_CHECKER_SOCKET to route through the "
                            "supervisor-side checker server (no host fallback)"
                        ),
                    }
                )
            )
            return 2
        try:
            if args.command == "isabelle-check-node":
                response = client_isabelle_check_node(
                    socket_path, args.node_name, timeout_secs=args.timeout_secs
                )
            elif args.command == "isabelle-thm-oracles":
                response = client_isabelle_thm_oracles(
                    socket_path, args.node_name, timeout_secs=args.timeout_secs
                )
            elif args.command == "isabelle-thm-deps":
                response = client_isabelle_thm_deps(
                    socket_path, args.node_name, timeout_secs=args.timeout_secs
                )
            elif args.command == "isabelle-build-session":
                response = client_isabelle_build_session(
                    socket_path, timeout_secs=args.timeout_secs
                )
            else:  # isabelle-sync-session
                response = client_isabelle_sync_session(
                    socket_path, timeout_secs=args.timeout_secs
                )
        except CheckerRpcError as exc:
            print(json.dumps({"error": f"{exc.kind}: {exc.message}"}))
            return 2
        payload = dict(response)
        payload.pop("request_id", None)
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

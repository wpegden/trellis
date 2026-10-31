"""CLI for atomic trellis checker actions."""

from __future__ import annotations

import argparse
import json
import sys
import time
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
    client_local_closure_axioms_batch,
    client_statement_transform,
)
from .observations import (
    LEAN_SUPPORT_TIMEOUT_SECS,
    _progress_emit,
    build_tablet,
    capture_replay_environment,
    compile_node,
    materialize_tablet_oleans,
    observe_lean_semantic_payloads,
    prepare_compiled_support,
    print_axioms,
)
from .tablet_support import sync_tablet_support


def main(argv: Optional[Sequence[str]] = None) -> int:
    try:
        return _main(argv)
    except CheckerRpcError as error:
        print(json.dumps({"error": str(error), "rpc_error": {
            "kind": error.kind, "message": error.message,
        }}))
        return 2


class _TranscriptArgumentParser(argparse.ArgumentParser):
    def error(self, message: str) -> None:
        if len(message) > 700:
            message = message[:700] + " ... [truncated; pass one file or directory path]"
        super().error(message)


def _main(argv: Optional[Sequence[str]] = None) -> int:
    argv = list(argv if argv is not None else sys.argv[1:])
    parser_type = _TranscriptArgumentParser if argv[:1] == ["inspect-transcript"] else argparse.ArgumentParser
    parser = parser_type(prog="trellis-atomic-actions")
    subparsers = parser.add_subparsers(dest="command", required=True)

    transcript = subparsers.add_parser("inspect-transcript", help="List a transcript directory or read bounded JSONL chat entries")
    transcript.add_argument("path", type=Path)
    transcript.add_argument("--after-line", type=int, default=0, help="Resume after this file line or directory-entry cursor")
    transcript.add_argument("--kind", choices=("all", "message", "command", "failure"), default="all")
    transcript.add_argument("--contains", default="")
    transcript.add_argument("--limit", type=int, default=10)
    transcript.add_argument("--max-chars", type=int, default=12000)

    model_support_parser = subparsers.add_parser("model-support-check")
    model_support_parser.add_argument("repo_path", nargs="?", default=".")

    native_basis_parser = subparsers.add_parser("native-basis-check")
    native_basis_parser.add_argument("repo_path", nargs="?", default=".")

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

    environment_parser = subparsers.add_parser("capture-replay-environment")
    environment_parser.add_argument("repo_path")
    environment_parser.add_argument("nodes_json")
    environment_parser.add_argument("--timeout-secs", type=float, default=180.0)
    environment_parser.add_argument("--materialized-only", action="store_true")

    materialize_parser = subparsers.add_parser("materialize-tablet-oleans")
    materialize_parser.add_argument("repo_path", nargs="?", default=".")
    materialize_parser.add_argument("--node", action="append", default=[])
    materialize_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    axioms_parser = subparsers.add_parser("print-axioms")
    axioms_parser.add_argument("node_name")
    axioms_parser.add_argument("repo_path", nargs="?", default=".")
    axioms_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)
    axioms_parser.add_argument("--principal")

    # Patch A local-closure probe (LOCAL_CLOSURE_IMPL_PLAN.md §5.7).
    # Server-only op: there is no host-lake fallback. The subcommand
    # routes through ``client_local_closure_axioms`` when
    # ``TRELLIS_CHECKER_SOCKET`` is set; otherwise it errors loudly so
    # operator misconfiguration is surfaced rather than silently masked.
    local_closure_parser = subparsers.add_parser("local-closure-axioms")
    closure_batch = subparsers.add_parser('local-closure-axioms-batch')
    closure_batch.add_argument('registration_json')
    closure_batch.add_argument('--timeout-secs', type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)
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
    # Source-based worker-authoring policy operation. It emits no certificate
    # evidence and is deliberately separate from the full closure probe.
    local_closure_parser.add_argument(
        "--scan-only", action="store_true", default=False
    )
    local_closure_parser.add_argument(
        "--module-owner", action="store_true", default=False
    )
    local_closure_parser.add_argument("--principal", default=None)
    local_closure_parser.add_argument("--premise-owner", action="append", default=[])

    transform_parser = subparsers.add_parser("lean-statement-transform")
    transform_parser.add_argument("--input-json", required=True)
    transform_parser.add_argument("--timeout-secs", type=float, default=LEAN_SUPPORT_TIMEOUT_SECS)

    payload_parser = subparsers.add_parser("lean-semantic-payloads")
    payload_parser.add_argument("repo_path", nargs="?", default=".")
    payload_parser.add_argument("--node", action="append", default=[])
    payload_parser.add_argument("--principal", action="append", default=[])
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

    args = parser.parse_args(argv)
    if args.command == "inspect-transcript":
        from trellis.chat_history import inspect_transcript
        try:
            result = inspect_transcript(args.path, after_line=args.after_line, kind=args.kind,
                                        contains=args.contains, limit=args.limit, max_chars=args.max_chars)
        except OSError as error:
            print(json.dumps({"error": str(error)[:1000]}))
            return 1
        print(json.dumps(result, ensure_ascii=False))
        return 0
    if args.command == "native-basis-check":
        from trellis.modeling.native import check_provenance
        result = check_provenance(Path(args.repo_path))
        errors = [name for name, passed in result["checks"].items() if not passed]
        if result["error"]:
            errors.append(result["error"])
        print(json.dumps({"ok": result["passed"], "errors": errors, "report": result["report"], "host_error": result.get("host_error", False)}))
        return 0 if result["passed"] else 1

    if args.command == "model-support-check":
        from trellis.pv_model_support import sync_model_support
        try:
            result = sync_model_support(Path(args.repo_path))
            result = {"ok": result["matches"], "errors": [] if result["matches"] else [
                "Generated model composition differs from the registered support in "
                + ", ".join(result["mismatched_files"])
                + "; run compose-native after editing support nodes or bindings"
            ]}
        except (OSError, ValueError, KeyError) as error:
            result = {"ok": False, "errors": [str(error)]}
        print(json.dumps(result))
        return 0 if result["ok"] else 1

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
    elif args.command == "capture-replay-environment":
        try:
            nodes = json.loads(args.nodes_json)
        except ValueError as error:
            payload = dict(environment=None, cold_reason=str(error)[:240])
        else:
            payload = capture_replay_environment(
                Path(args.repo_path).resolve(), nodes, timeout_secs=args.timeout_secs,
                materialized_only=args.materialized_only,
            )
    elif args.command == "materialize-tablet-oleans":
        # Mirror the ``local-closure-axioms`` contract: a client-side RPC
        # failure is reported as a JSON ``error``. Letting CheckerRpcError
        # escape printed nothing on stdout, so the kernel saw only
        # "returned invalid JSON: EOF while parsing a value" with the real
        # message buried in a stderr traceback.
        try:
            payload = materialize_tablet_oleans(
                Path(args.repo_path).resolve(),
                args.node,
                timeout_secs=args.timeout_secs,
            )
        except CheckerRpcError as exc:
            print(json.dumps({"error": f"{exc.kind}: {exc.message}"}))
            return 2
        if payload.get("returncode") != 0 or payload.get("timed_out") or payload.get("spawn_error"):
            print("materialize-tablet-oleans: failed; inspect the JSON result for diagnostics.", file=sys.stderr)
    elif args.command == "print-axioms":
        payload = print_axioms(
            Path(args.repo_path).resolve(),
            args.node_name,
            timeout_secs=args.timeout_secs,
            principal_name=args.principal,
        )
    elif args.command == 'local-closure-axioms-batch':
        socket_path = _resolve_socket_path()
        if socket_path is None:
            print(json.dumps({'error': 'local-closure-axioms-batch requires TRELLIS_CHECKER_SOCKET'}))
            return 2
        payload = dict(client_local_closure_axioms_batch(socket_path,
            json.loads(args.registration_json), timeout_secs=args.timeout_secs))
        payload.pop('request_id', None)
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
                module_owner=getattr(args, "module_owner", False),
                principal_name=getattr(args, "principal", None),
                premise_owners=args.premise_owner,
            )
        except CheckerRpcError as exc:
            print(json.dumps({"error": f"{exc.kind}: {exc.message}"}))
            return 2
        payload = dict(response)
        payload.pop("request_id", None)
    elif args.command == "lean-statement-transform":
        socket_path = _resolve_socket_path()
        if socket_path is None:
            print(json.dumps({"error": "lean-statement-transform requires TRELLIS_CHECKER_SOCKET; no host fallback"}))
            return 2
        try:
            payload = dict(client_statement_transform(socket_path, json.loads(args.input_json), timeout_secs=args.timeout_secs))
            payload.pop("request_id", None)
        except (ValueError, CheckerRpcError) as exc:
            print(json.dumps({"error": str(exc)}))
            return 2
    elif args.command == "lean-semantic-payloads":
        try:
            principal_names = {}
            for binding in args.principal:
                node_name, separator, principal = str(binding).partition("=")
                if not separator or not node_name or not principal:
                    raise ValueError(
                        "--principal must have the form <node>=<exact Lean name>"
                    )
                principal_names[node_name] = principal
            payload = observe_lean_semantic_payloads(
                Path(args.repo_path).resolve(),
                args.node,
                timeout_secs=args.timeout_secs,
                principal_names=principal_names,
            )
        except (CheckerRpcError, ValueError) as exc:
            if isinstance(exc, CheckerRpcError):
                message = f"{exc.kind}: {exc.message}"
            else:
                message = str(exc)
            print(json.dumps({"error": message}))
            return 2
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
        # Acceptance sub-progress heartbeat (Fix: the Isabelle acceptance gate
        # used to be silent for the whole multi-hour per-node sweep). THIS
        # process — the check.py child the kernel spawns per node — is the one
        # that inherits `TRELLIS_ACCEPTANCE_PROGRESS_LOG` from
        # `_run_kernel_cli_once`, so lines emitted here stream to the worker
        # agent's stderr in real time. The checker server does the actual work
        # on the far side of the AF_UNIX socket (its own process does not
        # carry the per-call env var), so the client-side start/done pair is
        # the worker-visible liveness signal for each long round-trip.
        target = str(getattr(args, "node_name", None) or "(session)")
        op_label = f"{args.command} {target}"
        started = time.time()
        _progress_emit(
            f"[acceptance]   {op_label}: dispatching to checker server "
            f"(an Isabelle cert probe can run ~10-15 min; a cold-session "
            f"fallback longer — silence here is the server working, not a hang)"
        )
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
            _progress_emit(
                f"[acceptance]   {op_label}: failed ({exc.kind}) "
                f"in {time.time() - started:.1f}s"
            )
            print(json.dumps({"error": f"{exc.kind}: {exc.message}"}))
            return 2
        payload = dict(response)
        payload.pop("request_id", None)
        if "status" in payload:
            outcome_note = f"status={payload.get('status')}"
        else:
            outcome_note = f"returncode={payload.get('returncode')}"
        _progress_emit(
            f"[acceptance]   {op_label}: done {outcome_note} "
            f"in {time.time() - started:.1f}s"
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

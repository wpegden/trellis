"""Sidecar compile-iterate loop (E§2) — warm ``lean --server`` engine.

Reuses ``trellis/incremental_check.py``'s ``_LeanServer`` + helpers
(imported, never copied — the active-node-prewarm precedent), with
sidecar-specific policy:

* injected options ``set_option maxHeartbeats 400000`` (E-D5:
  deliberately NOT the broker's ``maxHeartbeats 0`` — a background
  prover must not produce proofs that only close with unbounded
  heartbeats, nor burn 30 min on a runaway ``decide``) plus the
  broker's ``maxRecDepth 10000``;
* sidecar timeouts: quiet 90 s / progress 300 s (not the broker's
  1800 s);
* failure-open ``lake build`` fallback when the LSP verdict is
  ambiguous or the server crashed;
* the CONFIRMING ``lake build Tablet.<Node>`` is the sole exit
  authority (advisory-LSP house rule: green in the server is
  necessary, never sufficient) — exit 0 AND no ``sorry`` warning;
* giant-node skip (``is_giant_node``) — such candidates are marked
  ``skipped_giant`` by the daemon, never opened.

The server is launched at lowest priority through
``workspace.build_lean_server_command`` (the same nice 19 / ionice
idle / bwrap role / ``LEAN_NUM_THREADS`` wrapper as lake commands;
threads = config, 2 by default — ``Elab.async`` needs a worker
thread). Sandbox role configured + bwrap missing = fail-closed
refusal (F5), never a silent bare launch.
"""

from __future__ import annotations

import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Tuple

from trellis import incremental_check as _ic
from trellis.sidecar.config import SidecarConfig
from trellis.sidecar.driver import CompileVerdict, split_body_marker
from trellis.sidecar.workspace import build_lake_command, build_lean_server_command

SIDECAR_QUIET_TIMEOUT_SECS = 90.0
SIDECAR_PROGRESS_TIMEOUT_SECS = 300.0
SIDECAR_LAKE_BUILD_TIMEOUT_SECS = 600.0
SIDECAR_GOALS_TIMEOUT_SECS = 30.0
SIDECAR_INJECTED_OPTIONS: Tuple[str, ...] = (
    "set_option maxHeartbeats 400000",
    "set_option maxRecDepth 10000",
)


def _inject_sidecar_options(text: str) -> Tuple[str, int, int]:
    """Insert the sidecar option lines after the import block (the
    ``_inject_options`` shape with the sidecar heartbeat ceiling)."""
    insert_line = _ic._import_block_end_line(text)
    lines = text.splitlines(keepends=True)
    injected = [option + "\n" for option in SIDECAR_INJECTED_OPTIONS]
    new_lines = lines[:insert_line] + injected + lines[insert_line:]
    return "".join(new_lines), insert_line, len(injected)


@dataclass
class ConfirmResult:
    ok: bool
    log: str


class SidecarCompileLoop:
    """One warm server over the sidecar workspace; one node at a time.

    Iteration protocol (E§2.2): ``didOpen`` the target once (cold cost),
    then each candidate body is a ``didChange`` replacing the body
    region; the import environment stays loaded. Ambiguous/crashed
    verdicts fall back (failure-open) to a per-iteration
    ``lake build Tablet.<Node>``.
    """

    def __init__(
        self,
        repo: Path,
        config: SidecarConfig,
        *,
        server_cmd: Optional[List[str]] = None,
        server_env: Optional[dict] = None,
    ) -> None:
        self.repo = Path(repo)
        self.config = config
        self._server_cmd = server_cmd
        self._server_env = server_env
        self._server: Optional[_ic._LeanServer] = None
        self._open_node: Optional[str] = None
        self._open_text: str = ""
        self._inject_geometry: Optional[Tuple[int, int]] = None
        self._doc_version = 0
        # v3 goal-state tool: the body most recently loaded into the
        # server document (didOpen initial body or last check_body
        # candidate) plus its prefix — get_goals positions are relative
        # to THIS body.
        self._last_prefix: str = ""
        self._last_body: str = ""
        # Sidecar timeout policy on the shared helper module (the
        # prewarm-server precedent for tuning the module knobs).
        _ic._QUIET_TIMEOUT_SECS = SIDECAR_QUIET_TIMEOUT_SECS
        _ic._PROGRESS_TIMEOUT_SECS = SIDECAR_PROGRESS_TIMEOUT_SECS

    # -- giant skip -------------------------------------------------------

    def giant_reason(self, node: str) -> Optional[str]:
        lean_path = _ic._tablet_lean_path(self.repo, node)
        giant, reason = _ic.is_giant_node(
            lean_path, max_lines=self.config.giant_node_max_lines
        )
        return reason if giant else None

    # -- server lifecycle --------------------------------------------------

    def _ensure_server(self) -> Optional[_ic._LeanServer]:
        if self._server is not None and self._server.alive():
            return self._server
        self._server = None
        server_cmd, server_env = self._server_cmd, self._server_env
        if server_cmd is None:
            # F2: the warm server launches through the SAME
            # bwrap+nice+ionice+LEAN_NUM_THREADS wrapper as lake
            # commands — never bare. A SandboxUnavailableError here
            # propagates (fail-closed; the daemon refuses at startup
            # anyway).
            server_cmd, server_env = build_lean_server_command(
                self.repo,
                lean_threads=self.config.lean_threads,
                sandbox_role=self.config.sandbox_role,
                allow_unsandboxed=self.config.allow_unsandboxed,
            )
        try:
            server = _ic._LeanServer(
                self.repo,
                server_cmd=server_cmd,
                server_env=server_env,
            )
        except OSError:
            return None
        if not server.initialize():
            server.shutdown()
            return None
        self._server = server
        return server

    def shutdown(self) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server = None
        self._open_node = None

    # -- document management ------------------------------------------------

    def open_node(self, node: str, content: str) -> bool:
        """didOpen the target (cold elaboration). False => callers use
        the lake fallback for every iteration (failure-open)."""
        server = self._ensure_server()
        if server is None:
            return False
        injected, insert_line, num_injected = _inject_sidecar_options(content)
        self._inject_geometry = (insert_line, num_injected)
        self._doc_version = 1
        uri = _ic._uri_for(self.repo, node)
        server.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": uri,
                    "languageId": "lean4",
                    "version": self._doc_version,
                    "text": injected,
                }
            },
        )
        status, _, _ = _ic._wait_terminal_progress(server, uri)
        if status == "crashed":
            self.shutdown()
            return False
        self._open_node = node
        self._open_text = injected
        try:
            self._last_prefix, self._last_body = split_body_marker(content)
        except ValueError:
            self._last_prefix, self._last_body = content, ""
        return status == "complete"

    def check_body(self, node: str, prefix: str, body: str) -> CompileVerdict:
        """Advisory iteration check: splice in memory, didChange, wait,
        classify. Failure-open to ``lake build`` on ambiguity/crash.
        The on-disk file is NOT written here — the daemon writes the
        workspace file only for the confirming build."""
        server = self._server if self._open_node == node else None
        if server is None or not server.alive():
            return self._lake_fallback(node, prefix, body)
        new_full, insert_line, num_injected = _inject_sidecar_options(prefix + body)
        self._inject_geometry = (insert_line, num_injected)
        self._doc_version += 1
        uri = _ic._uri_for(self.repo, node)
        change = _ic._range_content_change(self._open_text, new_full)
        server.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": uri, "version": self._doc_version},
                "contentChanges": [change],
            },
        )
        self._open_text = new_full
        self._last_prefix, self._last_body = prefix, body
        status, _, diags_by_uri = _ic._wait_terminal_progress(server, uri)
        if status == "crashed":
            self.shutdown()
            return self._lake_fallback(node, prefix, body)
        if status != "complete":
            return self._lake_fallback(node, prefix, body)
        failed, error_lines, sorry_lines = _ic._classify_diagnostics(
            self.repo,
            node,
            diags_by_uri,
            inject_geometry=self._inject_geometry,
        )
        if failed:
            return CompileVerdict(ok=False, log="\n".join(error_lines[:20])[:4096])
        if sorry_lines:
            return CompileVerdict(
                ok=False,
                log="declaration uses 'sorry':\n" + "\n".join(sorry_lines[:20]),
            )
        return CompileVerdict(ok=True, log="")

    # -- v3 goal-state access (advisory, read-only) --------------------------

    def plain_goals(
        self,
        node: str,
        body_line: Optional[int] = None,
        column: Optional[int] = None,
    ) -> str:
        """``$/lean/plainGoal`` at a position of the LAST body loaded
        into the warm server (didOpen initial body or the most recent
        ``check_body`` candidate). ``body_line`` is 1-based within that
        body; None => first line containing ``sorry``, else the last
        non-empty line. Purely advisory: any failure degrades to a
        told-the-model message, never an exception."""
        server = self._server if self._open_node == node else None
        if server is None or not server.alive():
            return (
                "get_goals: goal state unavailable (no warm server "
                "document); rely on compiler feedback"
            )
        body_lines = self._last_body.splitlines() or [""]
        if body_line is None:
            body_line = next(
                (i + 1 for i, l in enumerate(body_lines) if "sorry" in l), 0
            )
            if body_line == 0:
                body_line = max(
                    (i + 1 for i, l in enumerate(body_lines) if l.strip()),
                    default=1,
                )
        body_line = max(1, min(int(body_line), len(body_lines)))
        line_text = body_lines[body_line - 1]
        if column is None:
            col = len(line_text)
        else:
            col = max(0, min(int(column), len(line_text)))
        prefix_lines = self._last_prefix.count("\n")
        insert_line, num_injected = self._inject_geometry or (0, 0)
        shift = num_injected if insert_line <= prefix_lines else 0
        doc_line = prefix_lines + shift + (body_line - 1)
        uri = _ic._uri_for(self.repo, node)
        rid = server.request(
            "$/lean/plainGoal",
            {
                "textDocument": {"uri": uri},
                "position": {"line": doc_line, "character": col},
            },
        )
        deadline = time.time() + SIDECAR_GOALS_TIMEOUT_SECS
        where = f"body line {body_line}, col {col}"
        while time.time() < deadline:
            msg = server.drain(timeout=0.5)
            if msg is None:
                if not server.alive():
                    return "get_goals: server crashed; rely on compiler feedback"
                continue
            if msg.get("id") != rid:
                continue  # stray notification/response — not ours
            if msg.get("error"):
                detail = msg["error"].get("message", "?")
                return f"get_goals: server error at {where}: {detail}"
            res = msg.get("result")
            if not res:
                return (
                    f"get_goals: no goal state at {where} (position may be "
                    "outside a tactic block — try a different line)"
                )
            rendered = res.get("rendered") if isinstance(res, dict) else None
            if rendered:
                return f"goal state at {where}:\n{rendered}"
            goals = res.get("goals") if isinstance(res, dict) else None
            if goals:
                return f"goal state at {where}:\n" + "\n---\n".join(
                    str(g) for g in goals
                )
            return (
                f"get_goals: empty goal state at {where} (no goals remain "
                "at this position)"
            )
        return "get_goals: timed out waiting for the goal state"

    # -- lake fallback + confirming build -----------------------------------

    def _write_node_file(self, node: str, content: str) -> None:
        path = _ic._tablet_lean_path(self.repo, node)
        path.write_text(content, encoding="utf-8")

    def _lake_build(self, node: str) -> ConfirmResult:
        cmd = build_lake_command(
            self.repo,
            ["lake", "build", f"Tablet.{node}"],
            lean_threads=self.config.lean_threads,
            sandbox_role=self.config.sandbox_role,
            allow_unsandboxed=self.config.allow_unsandboxed,
        )
        try:
            proc = subprocess.run(
                cmd,
                cwd=str(self.repo),
                capture_output=True,
                text=True,
                timeout=SIDECAR_LAKE_BUILD_TIMEOUT_SECS,
            )
        except subprocess.TimeoutExpired:
            return ConfirmResult(ok=False, log="lake build timeout")
        log = (proc.stdout + "\n" + proc.stderr)[-4096:]
        if proc.returncode != 0:
            return ConfirmResult(ok=False, log=log)
        # A ``sorry`` is a WARNING — ``lake build`` exits 0 on it — so
        # returncode alone NEVER proves cleanliness (that is the false-green
        # trap). Match the elaborator warning regardless of quote style via the
        # shared robust regex (v4.30.0-rc1 prints backticks: ``declaration uses
        # `sorry```; older Lean uses straight/double quotes or none), plus the
        # axiom-level ``sorryAx`` a ``#print axioms`` probe would surface.
        combined = proc.stdout + "\n" + proc.stderr
        if _sorry_warning_names_node(combined, node):
            return ConfirmResult(ok=False, log="declaration uses `sorry`\n" + log)
        if "sorryAx" in combined:
            return ConfirmResult(ok=False, log="sorryAx axiom\n" + log)
        return ConfirmResult(ok=True, log=log)

    def _lake_fallback(self, node: str, prefix: str, body: str) -> CompileVerdict:
        self._write_node_file(node, prefix + body)
        result = self._lake_build(node)
        return CompileVerdict(ok=result.ok, log=result.log)

    def confirm(self, node: str, prefix: str, body: str) -> ConfirmResult:
        """THE exit authority: write the candidate to the workspace file
        and run a real ``lake build Tablet.<Node>`` (exit 0, no sorry
        warning). LSP green is advisory by house rule."""
        self._write_node_file(node, prefix + body)
        return self._lake_build(node)

    def restore(self, node: str, content: str) -> None:
        """Restore the workspace node file (attempt abandoned)."""
        self._write_node_file(node, content)


def make_goal_tool_handler(loop: SidecarCompileLoop, node: str):
    """v3 ``get_goals`` info-tool handler over the warm server. Info
    tools cost a turn, never a compile iteration (driver contract)."""

    def get_goals(args) -> str:
        def _opt_int(key: str):
            val = args.get(key)
            if val is None or val == "":
                return None
            try:
                return int(val)
            except (TypeError, ValueError):
                return None

        return loop.plain_goals(
            node, body_line=_opt_int("line"), column=_opt_int("column")
        )

    return get_goals


def _sorry_warning_names_node(text: str, node: str) -> bool:
    """Does the build output carry a `sorry` warning FOR THIS NODE?

    `lake build Tablet.<Node>` builds the node's whole import closure and
    replays cached dependencies, re-emitting their warnings. Searching the
    combined output for the warning therefore rejected a node because some
    OTHER file was open — and with 113 of 614 nodes open in a live tablet,
    that noise is in nearly every build. Observed: a clean, compiling proof
    of `MinimumImperfectBergeNoTwoJoin` was discarded on warnings from four
    other nodes, three of which it does not even import.

    The gate a grunt must clear is the one a worker clears: LOCAL closure —
    this node's own declarations carry no `sorry`. Whether the proof
    *depends* on an open sibling is a different and more precise question,
    and it is already answered downstream by a real axiom probe
    (`prevalidate.run_axioms_probe` runs `#print axioms <Node>`
    fail-closed, and the kernel's model is axiom-closure: `sorryAx` in the
    closure rejects). A text grep over unrelated files cannot tell import
    from use; the probe can.

    Fail-closed on a warning that names no file at all, so an unattributed
    warning is still treated as this node's.
    """
    target = f"Tablet/{node}.lean"
    for line in text.splitlines():
        if _ic._SORRY_WARNING_RE.search(line) is None:
            continue
        if target in line:
            return True
        if "Tablet/" not in line and ".lean" not in line:
            return True
    return False


def make_compile_callback(
    loop: SidecarCompileLoop, node: str, node_content: str
):
    """The driver's ``compile_body`` callback: advisory server check;
    a green advisory result is then CONFIRMED by lake before the
    driver may declare success."""
    prefix, _ = split_body_marker(node_content)

    def compile_body(body: str) -> CompileVerdict:
        verdict = loop.check_body(node, prefix, body)
        if not verdict.ok:
            return verdict
        confirm = loop.confirm(node, prefix, body)
        if not confirm.ok:
            return CompileVerdict(ok=False, log="confirm failed:\n" + confirm.log)
        return CompileVerdict(ok=True, log=confirm.log)

    return compile_body

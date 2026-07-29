"""The supervisor-side active-node prewarm server.

One persistent, supervisor-owned, NON-AUTHORITATIVE ``lake env lean --server``
keeping EXACTLY ONE node warm: the supervisor's current active node. See the
package docstring for the full rationale (single-node + deps-as-oleans is the
shape that completes the giant node where the cone-as-live-docs preview service
wedged).

Everything LSP/reliability is REUSED from ``trellis.incremental_check`` (the
hardened in-burst broker): the ``_LeanServer`` (reliability env + unlimited
stack), ``_inject_options`` + diagnostic-offset correction,
``_wait_terminal_progress`` (the fileProgress-terminal wait WITH the
``saw_processing`` latch that is the false-early-complete fix),
``_classify_diagnostics`` (errors fail; ``sorry`` is INFO, mirroring
``lake build``; foreign-URI). This
module adds only single-node warm lifecycle, one-node content-keyed coherence,
and a unix-socket front end.

Lifecycle: ``start`` -> ``set_active_node`` (didOpen + drive to terminal, the
prewarm) -> serve checks of the active node -> crash-restart on a dead server
-> ``shutdown`` reaping the child. The warm state CARRIES ACROSS bursts while
the active node is unchanged; it re-seeds (didClose + didOpen) only when the
supervisor reports the active node's accepted CONTENT changed (a content hash,
never mtime-guessing).
"""

from __future__ import annotations

import hashlib
import logging
import re
import socket
import struct
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Mapping, Optional, Tuple

from trellis.active_node_prewarm.config import ActiveNodePrewarmConfig

# REUSE the hardened in-burst broker primitives. Importing the module (not
# copying) guarantees the supervisor-side server and the worker-side broker
# stay byte-for-byte identical in their false-green protections.
from trellis import incremental_check as _ic

_LOGGER = logging.getLogger("trellis.active_node_prewarm")

# Defense-in-depth for the stale-mirror case: an import that resolves to a node
# the workspace mirror lacks surfaces as an "unknown import"/"unknown module"
# elaboration error rather than a genuine in-node mistake. If a missing import
# slips past the pre-check (e.g. an alias or path shape the scan did not catch),
# classify ONLY that import-resolution failure as `fallback` (the in-burst
# broker sees the new helper), never reclassifying a real in-node error.
_BAD_IMPORT_DIAG_RE = re.compile(
    r"unknown (?:import|module|package)|"
    r"(?:could not|cannot|failed to) (?:find|resolve|load) .*\b(?:import|module)\b|"
    r"bad import",
    re.IGNORECASE,
)


def _error_lines_are_only_bad_imports(error_lines: List[str]) -> bool:
    """True iff EVERY rendered error line is an import-resolution failure (and
    there is at least one). Used so a missing-import elaboration result is
    reclassified to `fallback`, while any genuine in-node error keeps `fail`."""
    if not error_lines:
        return False
    return all(_BAD_IMPORT_DIAG_RE.search(line) is not None for line in error_lines)


# --------------------------------------------------------------------------
# Wire protocol (length-prefixed JSON, mirrors the broker's framing)
# --------------------------------------------------------------------------

def _send_frame(sock: socket.socket, obj: Mapping[str, Any]) -> None:
    _ic._send_frame(sock, obj)


def _recv_frame(sock: socket.socket) -> Optional[Mapping[str, Any]]:
    return _ic._recv_frame(sock)


def _content_hash(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _bwrap_lean_server_cmd(
    workspace: Path, *, bwrap_role: str, burst_home: Optional[Path]
) -> Tuple[List[str], Dict[str, str]]:
    """Build a bwrap-confined ``lake env lean --server`` command + its launch
    env, mirroring ``observations._run_lake_command``'s ``lake_compiler``
    wrapping. (Gap-1 wipe-safety.)

    The ``lake_compiler`` role ro-binds the workspace's
    ``.lake/packages/<pkg>`` SOURCE checkouts (only ``<pkg>/.lake/build``,
    ``<pkg>/.lake/config`` and ``<pkg>/widget`` are writable), so any ``lake
    env`` re-clone / manifest reconciliation that tries to rewrite a package
    checkout hits EROFS and CANNOT wipe ``.lake/packages`` — the server is
    wipe-safe even if lake decides to reconcile.

    This lives in the prewarm package (which may import the ``trellis`` source
    tree); ``incremental_check`` itself stays self-contained for the worker
    sandbox.
    """
    import os

    from trellis.config import SandboxConfig
    from trellis.sandbox import bwrap_available, wrap_command
    from trellis.host_runtime import worker_elan_home

    if not bwrap_available():
        raise RuntimeError(
            "bwrap is required for the bwrap-confined lean --server but is not "
            "installed"
        )

    home = Path(burst_home).resolve() if burst_home is not None else None
    cmd = wrap_command(
        ["lake", "env", "lean", "--server"],
        sandbox=SandboxConfig(enabled=True, backend="bwrap"),
        work_dir=workspace,
        burst_home=home,
        role=bwrap_role,
    )
    # Inside the bwrap HOME is rebound, so make ELAN_HOME explicit and pin the
    # elaborator thread pool (mirrors observations._run_lake_command).
    elan_home = str(worker_elan_home())
    lean_threads = os.environ.get("TRELLIS_LEAN_PARALLELISM", "6").strip() or "6"
    cmd = cmd[:1] + [
        "--setenv", "ELAN_HOME", elan_home,
        "--setenv", "LEAN_NUM_THREADS", lean_threads,
    ] + cmd[1:]

    env = dict(os.environ)
    # bwrap resolves the OUTER `lake` via PATH (wrap_command adds no PATH
    # setenv); prepend the resolved elan bin so it resolves even if the
    # launching shell had no elan on PATH.
    elan_bin = str(worker_elan_home() / "bin")
    existing_path = env.get("PATH", "")
    if elan_bin not in existing_path.split(os.pathsep):
        env["PATH"] = (
            f"{elan_bin}{os.pathsep}{existing_path}" if existing_path else elan_bin
        )
    return cmd, env


class ActiveNodePrewarmServer:
    """Supervisor-owned warm server for a single active node.

    Drive it in-process (``set_active_node`` / ``check``) or over a unix socket
    (``serve_forever``). The supervisor calls ``set_active_node`` when it
    selects or holds the active node; the worker connects over the socket for
    its first ``incremental-check``.
    """

    def __init__(
        self,
        *,
        workspace: Path,
        config: ActiveNodePrewarmConfig,
        socket_path: Optional[Path] = None,
    ) -> None:
        self.workspace = Path(workspace).resolve()
        self.config = config
        self.socket_path = Path(socket_path).resolve() if socket_path else None

        # Gap-1 wipe-safety: confine the server's `lean --server` under bwrap so
        # the run repo's `.lake/packages/<pkg>` checkouts are read-only and a
        # `lake env` re-clone cannot wipe them. An empty `sandbox_role` (offline
        # harness on a throwaway copy ONLY) leaves the server un-sandboxed.
        self._bwrap_role: Optional[str] = config.sandbox_role.strip() or None
        self._bwrap_home: Optional[Path] = (
            Path(config.sandbox_home).resolve() if config.sandbox_home else None
        )

        # Apply config timeouts onto the reused broker module's globals that
        # ``_wait_terminal_progress`` reads. Single server per process, so this
        # is unambiguous; documented as the configurability seam.
        _ic._PROGRESS_TIMEOUT_SECS = float(config.diagnostics_wait_secs)
        _ic._QUIET_TIMEOUT_SECS = float(config.quiet_timeout_secs)
        _ic._INIT_TIMEOUT_SECS = float(config.startup_timeout_secs)

        self._lock = threading.RLock()
        self._srv: Optional[_ic._LeanServer] = None
        # The single warm node: its name, the injected buffer text last sent,
        # the option-injection geometry, the content hash of the ACCEPTED
        # source it was warmed against (coherence key), and a cached terminal
        # verdict for that exact text.
        self._active_node: Optional[str] = None
        self._active_uri: Optional[str] = None
        self._open_text: Optional[str] = None
        self._inject: Optional[Tuple[int, int]] = None
        self._warm_hash: Optional[str] = None
        self._warm_verdict: Optional[Mapping[str, Any]] = None
        # Last overlay check (Gap-2): the content hash of the worker's in-flight
        # text last elaborated as a didChange overlay, and its verdict. A repeat
        # check of the same edit re-serves this without re-driving the server.
        self._last_check_hash: Optional[str] = None
        self._last_check_verdict: Optional[Mapping[str, Any]] = None

        self._listen_sock: Optional[socket.socket] = None
        self._stop = threading.Event()

    # ---------------------------------------------------------------- lifecycle
    def start(self) -> None:
        """Spawn the warm ``lean --server`` (cold until the first didOpen)."""
        with self._lock:
            self._ensure_server_locked()

    def _ensure_server_locked(self) -> bool:
        """Ensure a live initialized server. Returns True on success. Crash
        recovery: a dead server is torn down and respawned (warm state lost,
        re-seeded by the next ``set_active_node``)."""
        if self._srv is not None and not self._srv.alive():
            self._teardown_server_locked()
        if self._srv is None:
            if self._bwrap_role is not None:
                cmd, env = _bwrap_lean_server_cmd(
                    self.workspace,
                    bwrap_role=self._bwrap_role,
                    burst_home=self._bwrap_home,
                )
                srv = _ic._LeanServer(
                    self.workspace, server_cmd=cmd, server_env=env
                )
            else:
                srv = _ic._LeanServer(self.workspace)
            if not srv.initialize():
                tail = srv.stderr_tail().strip()[:200]
                self._srv = srv
                self._teardown_server_locked()
                _LOGGER.warning("lean --server failed to initialize: %s", tail)
                return False
            self._srv = srv
            # A fresh server has no open documents; drop any warm bookkeeping.
            self._open_text = None
            self._inject = None
            self._warm_hash = None
            self._warm_verdict = None
            self._last_check_hash = None
            self._last_check_verdict = None
            _LOGGER.info("active-node prewarm server up (pid=%s)", srv.pid)
        return True

    def _teardown_server_locked(self) -> None:
        if self._srv is not None:
            try:
                self._srv.shutdown()
            except Exception:
                pass
        self._srv = None
        self._open_text = None
        self._inject = None
        self._warm_hash = None
        self._warm_verdict = None
        self._last_check_hash = None
        self._last_check_verdict = None

    def shutdown(self) -> None:
        """Clean shutdown: stop serving and reap the ``lean --server`` child so
        no orphan survives."""
        self._stop.set()
        if self._listen_sock is not None:
            try:
                self._listen_sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                self._listen_sock.close()
            except OSError:
                pass
            self._listen_sock = None
        with self._lock:
            self._teardown_server_locked()
            self._active_node = None
            self._active_uri = None
        if self.socket_path is not None:
            try:
                if self.socket_path.exists():
                    self.socket_path.unlink()
            except OSError:
                pass

    # ----------------------------------------------------- single-node warming
    def set_active_node(self, node_name: str) -> Dict[str, Any]:
        """Declare/hold the active node and PREWARM it.

        Called by the supervisor when it selects or holds an active node. If the
        node and its accepted CONTENT are unchanged since last warmed, this is a
        no-op (the warm state carries across bursts). If the node changed, or
        the SAME node's accepted content changed (one-node coherence), the prior
        warm document is closed and the new content is opened + driven to
        terminal fileProgress so it is warm before the worker's first check.

        Failure-open: any problem is logged and reported in the result; it never
        raises (a prewarm must never break burst dispatch).

        Returns a status dict: ``{"warmed": bool, "reseeded": bool,
        "reason": str, "verdict": "ok"|"fail"|None}``.
        """
        try:
            node = _ic._parse_target(node_name)
        except ValueError as exc:
            return {"warmed": False, "reseeded": False, "reason": f"bad node name: {exc}"}

        lean_path = _ic._tablet_lean_path(self.workspace, node)
        if not lean_path.is_file():
            return {
                "warmed": False,
                "reseeded": False,
                "reason": f"Tablet/{node}.lean does not exist",
            }

        giant, giant_reason = self._is_giant(lean_path)
        if giant:
            # A giant monolith cannot be warmed reliably; skip it (the worker
            # client falls straight to lake build for this node). Logged, not an
            # error — warming would only waste time.
            _LOGGER.info(
                "active-node prewarm: SKIP %s, too large to warm (%s)",
                node, giant_reason,
            )
            return {
                "warmed": False,
                "reseeded": False,
                "reason": f"node too large to warm ({giant_reason}); skipped",
            }

        with self._lock:
            try:
                return self._set_active_locked(node, lean_path)
            except Exception as exc:  # failure-open
                self._teardown_server_locked()
                return {
                    "warmed": False,
                    "reseeded": False,
                    "reason": f"prewarm error: {type(exc).__name__}: {exc}",
                }

    def _set_active_locked(self, node: str, lean_path: Path) -> Dict[str, Any]:
        raw_text = lean_path.read_text(encoding="utf-8")
        new_hash = _content_hash(raw_text)

        same_node = self._active_node == node
        # One-node coherence: re-seed iff the node identity changed OR the SAME
        # node's accepted content changed. Driven by the supervisor's
        # authoritative content (its hash), never by mtime.
        unchanged = (
            same_node
            and self._warm_hash == new_hash
            and self._srv is not None
            and self._srv.alive()
            and self._open_text is not None
        )
        if unchanged:
            return {
                "warmed": True,
                "reseeded": False,
                "reason": "active node unchanged; warm state carried over",
                "verdict": self._verdict_label(),
            }

        if not self._ensure_server_locked():
            return {
                "warmed": False,
                "reseeded": False,
                "reason": "lean --server unavailable",
            }
        assert self._srv is not None

        # Re-seed: close any prior open document, then open the new content.
        # In single-node mode there is at most ONE open document ever, so this
        # never touches the cone (deps stay oleans) and can never evict an
        # in-flight dependency.
        new_uri = _ic._uri_for(self.workspace, node)
        if self._active_uri is not None and self._open_text is not None:
            self._did_close_locked(self._active_uri)

        reseeded = self._active_node is not None or same_node
        self._active_node = node
        self._active_uri = new_uri
        self._open_text = None
        self._inject = None
        self._warm_hash = None
        self._warm_verdict = None
        self._last_check_hash = None
        self._last_check_verdict = None

        verdict = self._open_and_drive_locked(node, raw_text, new_hash)
        return {
            "warmed": verdict is not None,
            "reseeded": bool(reseeded),
            "reason": "prewarmed active node" if verdict is not None else "prewarm ambiguous",
            "verdict": self._verdict_label(),
        }

    def _open_and_drive_locked(
        self, node: str, raw_text: str, content_hash: str
    ) -> Optional[Mapping[str, Any]]:
        """didOpen the active node and drive it to terminal fileProgress so the
        cold elaboration is paid HERE (the prewarm). Records the warm verdict
        for the worker's first check. Returns the verdict, or None if the
        elaboration was ambiguous/crashed (warm state cleared)."""
        assert self._srv is not None
        srv = self._srv
        uri = _ic._uri_for(self.workspace, node)
        text, insert_line, num_injected = _ic._inject_options(raw_text)

        srv.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": uri,
                    "languageId": "lean",
                    "version": 1,
                    "text": text,
                }
            },
        )

        status, _target_diags, diags_by_uri = _ic._wait_terminal_progress(srv, uri)
        if status == "crashed":
            self._teardown_server_locked()
            return None
        if status == "ambiguous":
            # No usable terminal signal; drop warm state so a later
            # set_active_node / check re-seeds cleanly.
            self._did_close_locked(uri)
            self._open_text = None
            self._inject = None
            self._warm_hash = None
            self._warm_verdict = None
            self._last_check_hash = None
            self._last_check_verdict = None
            return None

        failed, error_lines, sorry_lines = _ic._classify_diagnostics(
            self.workspace, node, diags_by_uri,
            inject_geometry=(insert_line, num_injected),
        )
        verdict: Mapping[str, Any] = _ic._build_verdict(
            failed, error_lines, sorry_lines
        )
        self._open_text = text
        self._inject = (insert_line, num_injected)
        self._warm_hash = content_hash
        self._warm_verdict = verdict
        return verdict

    def _overlay_and_drive_locked(
        self, node: str, raw_text: str, content_hash: str
    ) -> Optional[Mapping[str, Any]]:
        """Elaborate the worker's IN-FLIGHT text as a didChange OVERLAY against
        the warm baseline (Gap-2). No disk is read or written: the overlay is the
        worker's own text, applied as an LSP buffer change to the already-open
        document so ``lean --server`` reuses its elaboration snapshots up to the
        edit point. Returns the verdict for the EDITED text, or None on
        crash/ambiguous (warm state cleared, client falls back).

        The warm baseline (``_warm_hash``/``_warm_verdict``) is left untouched;
        only the open buffer (``_open_text``) and the last-overlay cache move to
        the edited text. A subsequent check of the same edit re-serves the
        cached overlay verdict; a check that matches the baseline re-serves the
        warm verdict."""
        assert self._srv is not None
        srv = self._srv
        uri = _ic._uri_for(self.workspace, node)
        text, insert_line, num_injected = _ic._inject_options(raw_text)

        if (
            self._open_text is not None
            and self._inject == (insert_line, num_injected)
        ):
            if self._open_text == text:
                # Identical buffer already open: a no-op didChange yields no new
                # terminal fileProgress, so reuse whatever verdict we have for
                # this exact text (baseline or last overlay).
                if self._last_check_hash == content_hash and self._last_check_verdict is not None:
                    return self._last_check_verdict
                if self._warm_hash == content_hash and self._warm_verdict is not None:
                    return self._warm_verdict
            # Warm reuse: send a MINIMAL-range didChange (common prefix/suffix
            # stripped) so the server re-elaborates only the changed suffix.
            change = _ic._range_content_change(self._open_text, text)
            srv.notify(
                "textDocument/didChange",
                {
                    "textDocument": {"uri": uri, "version": int(time.time() * 1000)},
                    "contentChanges": [change],
                },
            )
        else:
            # No coherent open buffer (or the import block / injection geometry
            # shifted): close any prior open and re-open the overlay clean.
            if self._open_text is not None:
                self._did_close_locked(uri)
            srv.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "lean",
                        "version": 1,
                        "text": text,
                    }
                },
            )

        status, _target_diags, diags_by_uri = _ic._wait_terminal_progress(srv, uri)
        if status == "crashed":
            self._teardown_server_locked()
            return None
        if status == "ambiguous":
            # No usable terminal signal; drop the open buffer so the next call
            # re-seeds cleanly. Keep the warm baseline cache (it is content-keyed
            # and still valid for a baseline check).
            self._did_close_locked(uri)
            self._open_text = None
            self._inject = None
            self._last_check_hash = None
            self._last_check_verdict = None
            return None

        failed, error_lines, sorry_lines = _ic._classify_diagnostics(
            self.workspace, node, diags_by_uri,
            inject_geometry=(insert_line, num_injected),
        )
        verdict: Mapping[str, Any] = _ic._build_verdict(
            failed, error_lines, sorry_lines
        )
        self._open_text = text
        self._inject = (insert_line, num_injected)
        self._last_check_hash = content_hash
        self._last_check_verdict = verdict
        return verdict

    def _did_close_locked(self, uri: str) -> None:
        if self._srv is None:
            return
        try:
            self._srv.notify(
                "textDocument/didClose", {"textDocument": {"uri": uri}}
            )
        except Exception:
            pass

    def _is_giant(self, lean_path: Path) -> Tuple[bool, str]:
        """Giant-node skip, using the configured thresholds. Reuses the
        standalone heuristic in ``incremental_check`` so the supervisor server
        and the worker client classify identically."""
        return _ic.is_giant_node(
            lean_path,
            max_lines=self.config.giant_node_max_lines,
            heartbeat_ceiling=self.config.giant_node_heartbeat_ceiling,
        )

    def _is_giant_text(self, text: str) -> Tuple[bool, str]:
        """Giant-node skip on in-memory overlay TEXT (Gap-2): classify the
        worker's in-flight edit by its own size, not the on-disk copy, so a node
        the worker grew past the threshold still falls back. Mirrors
        ``is_giant_node`` exactly (line-count + preamble maxHeartbeats)."""
        max_lines = self.config.giant_node_max_lines
        ceiling = self.config.giant_node_heartbeat_ceiling
        n_lines = text.count("\n") + 1
        if max_lines > 0 and n_lines > max_lines:
            return True, f"{n_lines} lines exceeds giant threshold {max_lines}"
        for m in _ic._MAXHEARTBEATS_RE.finditer(text):
            try:
                val = int(m.group(1).replace("_", ""))
            except ValueError:
                continue
            if val >= ceiling >= 1:
                return (
                    True,
                    f"preamble maxHeartbeats {val} at/above ceiling {ceiling}",
                )
        return False, ""

    def _verdict_label(self) -> Optional[str]:
        if self._warm_verdict is None:
            return None
        return str(self._warm_verdict.get("verdict"))

    # --------------------------------------------------------------- worker check
    def check(
        self, node_name: str, *, overlay_text: Optional[str] = None
    ) -> Mapping[str, Any]:
        """Advisory check of the active node against the warm server.

        ``overlay_text`` is the worker's IN-FLIGHT ``Tablet/<node>.lean`` text,
        forwarded by the client (Gap-2 edit-visibility). The worker edits inside
        its OWN bwrap, so the server's workspace copy is the last-accepted text,
        not the worker's edit; the client therefore ships its local text and the
        server applies it as an LSP ``didChange`` overlay (no disk write) against
        the warm baseline, returning diagnostics for the EDITED text. When
        ``overlay_text`` is None (a legacy frame), the server falls back to its
        own workspace copy.

        Returns a broker-shaped result: ``{"verdict": "ok"|"fail"|"fallback",
        "lines": [...], "reason": "...", "warm": bool}``. A ``fallback`` verdict
        tells the worker client to drop to the in-burst broker / lake build.

        This server warms ONLY the active node, so a request for a DIFFERENT
        node returns ``fallback`` (the worker then uses the in-burst broker).
        The fast first-call property holds for the active node: the cold
        elaboration was already paid at ``set_active_node`` time, so a check of
        the unchanged active node re-serves the cached terminal verdict and an
        edited active node re-elaborates only the changed suffix off the warm
        baseline.
        """
        try:
            node = _ic._parse_target(node_name)
        except ValueError as exc:
            return {"verdict": "fallback", "reason": f"bad node name: {exc}"}

        # The text to check is the worker's overlay when supplied; otherwise the
        # server's own workspace copy.
        if overlay_text is not None:
            raw_text = overlay_text
        else:
            lean_path = _ic._tablet_lean_path(self.workspace, node)
            if not lean_path.is_file():
                return {
                    "verdict": "fallback",
                    "reason": f"Tablet/{node}.lean does not exist",
                }
            try:
                raw_text = lean_path.read_text(encoding="utf-8")
            except OSError as exc:
                return {
                    "verdict": "fallback",
                    "reason": f"cannot read Tablet/{node}.lean: {exc}",
                }

        # Giant-node skip uses the EDITED text's own size (the overlay), so a
        # worker who blew a node up past the threshold still falls back.
        giant, giant_reason = self._is_giant_text(raw_text)
        if giant:
            # Never warm a giant node here; tell the client to fall back (it
            # will hit the same giant-skip and go to lake build).
            return {
                "verdict": "fallback",
                "reason": f"node too large to warm ({giant_reason})",
            }

        # Stale-mirror short-circuit: if the active node (with the overlay
        # applied) imports a Tablet node that this server's workspace mirror does
        # NOT have, the mirror is stale relative to the worker's edit (the worker
        # created a new helper this burst; the mirror only syncs on
        # acceptance/checkpoint). Elaboration here would fail on a bad import, so
        # short-circuit to `fallback` — the in-burst broker runs inside the
        # worker bwrap and DOES see the new helper, so it serves the check
        # correctly. Cheap: no elaboration wasted.
        missing = _ic._missing_tablet_imports(self.workspace, node, raw_text)
        if missing:
            return {
                "verdict": "fallback",
                "reason": (
                    "workspace mirror missing import "
                    + ", ".join(missing)
                    + "; deferring to in-burst broker"
                ),
            }

        with self._lock:
            if self._active_node != node:
                # Not the warmed node: not our job -> client uses the in-burst
                # broker. (Never elaborate a non-active node here; that would
                # break the single-node guarantee.)
                return {
                    "verdict": "fallback",
                    "reason": f"{node} is not the warmed active node",
                }
            try:
                return self._check_active_locked(node, raw_text)
            except Exception as exc:  # failure-open
                self._teardown_server_locked()
                return {
                    "verdict": "fallback",
                    "reason": f"prewarm server error: {type(exc).__name__}: {exc}",
                }

    def _check_active_locked(self, node: str, raw_text: str) -> Mapping[str, Any]:
        new_hash = _content_hash(raw_text)

        warm = (
            self._srv is not None
            and self._srv.alive()
            and self._open_text is not None
            and self._warm_hash == new_hash
            and self._warm_verdict is not None
        )
        if warm:
            # Fast path: the checked text matches the warmed content; re-serve
            # the cached terminal verdict (the elaboration was paid at prewarm
            # time). This is the headline "fast first call".
            result = dict(self._warm_verdict)  # type: ignore[arg-type]
            result["warm"] = True
            return self._reclassify_bad_import_fallback(result)

        # The checked text differs from the warm snapshot (the worker's in-flight
        # edit). Drive a didChange OVERLAY against the warm baseline so the edit
        # is elaborated WITHOUT touching the server's disk, then return the fresh
        # verdict. (No on-disk file is read or written; the overlay is the
        # worker's text.)
        if not self._ensure_server_locked():
            return {"verdict": "fallback", "reason": "lean --server unavailable"}
        verdict = self._overlay_and_drive_locked(node, raw_text, new_hash)
        if verdict is None:
            return {
                "verdict": "fallback",
                "reason": "no terminal fileProgress within wait window",
            }
        result = dict(verdict)
        result["warm"] = False
        return self._reclassify_bad_import_fallback(result)

    def _reclassify_bad_import_fallback(
        self, result: Dict[str, Any]
    ) -> Dict[str, Any]:
        """Defense-in-depth: a `fail` whose error diagnostics are ENTIRELY
        import-resolution failures (a node the workspace mirror lacks) is
        downgraded to `fallback`, so the worker reaches the in-burst broker that
        sees the new helper. A `fail` with any genuine in-node error keeps
        `fail` with its diagnostics."""
        if result.get("verdict") != "fail":
            return result
        lines = list(result.get("lines", []) or [])
        if _error_lines_are_only_bad_imports(lines):
            return {
                "verdict": "fallback",
                "reason": (
                    "elaboration failed on unresolved import "
                    "(workspace mirror stale); deferring to in-burst broker"
                ),
                "warm": result.get("warm", False),
            }
        return result

    # ----------------------------------------------------------- socket serve
    def serve_forever(self) -> None:
        """Serve the unix socket. The worker connects from inside its bwrap (the
        socket dir is ro-bound, mirroring the checker/Loogle precedent)."""
        if self.socket_path is None:
            raise ValueError("no socket_path configured")
        self.socket_path.parent.mkdir(parents=True, exist_ok=True)
        if self.socket_path.exists():
            try:
                self.socket_path.unlink()
            except OSError:
                pass
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.bind(str(self.socket_path))
        except OSError:
            _LOGGER.error("could not bind %s", self.socket_path)
            return
        sock.listen(8)
        sock.settimeout(0.5)
        self._listen_sock = sock
        _LOGGER.info("active-node prewarm server listening at %s", self.socket_path)
        try:
            while not self._stop.is_set():
                try:
                    conn, _ = sock.accept()
                except socket.timeout:
                    continue
                except OSError:
                    if self._stop.is_set():
                        return
                    continue
                threading.Thread(
                    target=self._serve_conn, args=(conn,), daemon=True
                ).start()
        finally:
            try:
                sock.close()
            except OSError:
                pass

    def _serve_conn(self, conn: socket.socket) -> None:
        try:
            conn.settimeout(self.config.diagnostics_wait_secs + 120.0)
            req = _recv_frame(conn)
            if req is None:
                return
            method = req.get("method")
            if method == "ping":
                with self._lock:
                    active = self._active_node
                _send_frame(conn, {"verdict": "pong", "active_node": active})
                return
            if method == "set_active":
                status = self.set_active_node(str(req.get("node", "")))
                _send_frame(conn, status)
                return
            if method == "shutdown":
                _send_frame(conn, {"verdict": "ok"})
                self._stop.set()
                return
            if method == "check":
                overlay = req.get("text")
                result = self.check(
                    str(req.get("node", "")),
                    overlay_text=overlay if isinstance(overlay, str) else None,
                )
                _send_frame(conn, result)
                return
            _send_frame(conn, {"verdict": "fallback", "reason": "unknown method"})
        except Exception:
            try:
                _send_frame(conn, {"verdict": "fallback", "reason": "server conn error"})
            except Exception:
                pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

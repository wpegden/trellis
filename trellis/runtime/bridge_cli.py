"""One-shot JSON CLI for the trellis Python bridge."""

from __future__ import annotations

import contextlib
import json
import sys
import traceback
from typing import Any, Dict

from .bridge import BridgeError, handle_bridge_request
from .bridge_protocol import BridgeCliRequest


def _read_request() -> Dict[str, Any]:
    try:
        return json.load(sys.stdin)
    except Exception as exc:  # pragma: no cover - thin CLI guard
        raise RuntimeError(f"invalid bridge request JSON: {exc}") from exc


def main() -> int:
    try:
        payload = _read_request()
        request = BridgeCliRequest.from_dict(payload)
        # Keep stdout reserved for the final JSON response so backend progress
        # chatter cannot corrupt the kernel-facing bridge contract.
        with contextlib.redirect_stdout(sys.stderr):
            response = handle_bridge_request(request)
        json.dump(response, sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 0
    except BridgeError as exc:  # pragma: no cover - thin CLI guard
        json.dump(
            {"ok": False, "error": str(exc), "traceback": traceback.format_exc()},
            sys.stdout,
            indent=2,
        )
        sys.stdout.write("\n")
        return 1
    except Exception as exc:  # pragma: no cover - thin CLI guard
        # Preserve the Python traceback so a non-BridgeError fault doesn't lose
        # the stack — without it, an unexpected exception (e.g. an
        # `'int' object has no attribute 'get'` type error) reaches the kernel
        # adapter as only `str(exc)`, making the crash undebuggable. The kernel
        # side does not parse this field; it's surfaced verbatim on the next
        # occurrence via the adapter-error message + any operator log inspection.
        json.dump(
            {"ok": False, "error": str(exc), "traceback": traceback.format_exc()},
            sys.stdout,
            indent=2,
        )
        sys.stdout.write("\n")
        return 2


if __name__ == "__main__":  # pragma: no cover - CLI entry point
    raise SystemExit(main())

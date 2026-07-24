"""Agent-facing deterministic checker wrapper."""

from __future__ import annotations

from typing import Optional, Sequence

from trellis.checking import main as checking_main


def main(argv: Optional[Sequence[str]] = None) -> int:
    return checking_main(argv)


if __name__ == "__main__":
    raise SystemExit(main())

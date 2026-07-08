from __future__ import annotations

import subprocess
from pathlib import Path


def run_setup_prewarm(
    *,
    repo_path: Path,
    burst_group: str,
    burst_home: Path,
    elan_home: Path,
    burst_path: str,
    mathlib_build_tar: Path | None = None,
) -> None:
    """Prewarm Lean dependencies/builds.

    Post-bwrap-only: prewarm runs directly as the supervisor user inside bwrap with
    HOME pointed at burst_home.
    """

    mathlib_build_tar_cmd = ""
    if mathlib_build_tar is not None:
        tar_path = str(mathlib_build_tar)
        mathlib_build_tar_cmd = f"""
MATHLIB_BUILD_TAR={tar_path!r}
MATHLIB_BUILD_DIR=".lake/packages/mathlib/.lake/build"
MATHLIB_BUILD_MARKER="$MATHLIB_BUILD_DIR/lib/lean/Mathlib.olean"
if [[ -f "$MATHLIB_BUILD_TAR" && ! -f "$MATHLIB_BUILD_MARKER" ]]; then
  echo "  Seeding mathlib build from $MATHLIB_BUILD_TAR"
  mkdir -p "$MATHLIB_BUILD_DIR"
  tar --extract --file "$MATHLIB_BUILD_TAR" --directory "$MATHLIB_BUILD_DIR" \
    --no-same-owner --no-same-permissions
fi
"""

    prewarm_script = f"""
set -euo pipefail
umask 0002
cd {str(repo_path)!r}
lake update
{mathlib_build_tar_cmd}
if [[ -f ".lake/packages/mathlib/.lake/build/lib/lean/Mathlib.olean" ]]; then
  echo "  Skipping 'lake exe cache get'; local mathlib build seed is present"
else
  lake exe cache get
fi
lake build Tablet.Preamble
lake build Tablet
lake env lean .trellis/scratch/example.lean
"""
    subprocess.run(
        [
            "env",
            f"HOME={str(burst_home)}",
            f"ELAN_HOME={str(elan_home)}",
            f"PATH={burst_path}",
            "bash",
            "-c",
            prewarm_script,
        ],
        check=True,
        text=True,
    )

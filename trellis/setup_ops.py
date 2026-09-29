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
        # Seed atomically. Extracting straight into the live build dir is a
        # latent corruption bug: tar writes in archive order, so an interrupt
        # (SIGKILL, ENOSPC, reboot) can leave `lib/lean/Mathlib.olean` written
        # — possibly itself truncated mid-write — while most of the tree is
        # missing. That file is the marker every later step trusts: the
        # `lake exe cache get` skip below reads it, and lake's traces then
        # vouch for whatever oleans did land. The result is a "prewarmed" repo
        # whose mathlib is partial or corrupt, discovered much later as
        # mid-run import failures.
        #
        # Instead: extract into a sibling `build.partial.$$`, write the
        # completion sentinel inside it, and `mv` the finished tree into
        # place. The rename is the single atomic commit point, so the tree and
        # the sentinel that vouches for it appear together or not at all, and
        # a crashed extract leaves only a `build.partial.*` directory that the
        # next run discards. Absence of the sentinel — not presence of
        # Mathlib.olean — is what makes a seed run.
        #
        # The partial directory is named by `mktemp -d`, not by `$$`: pids are
        # per-namespace and recycle, and two extracts sharing one directory
        # would let the first finisher rename a tree the second is still
        # writing — with the sentinel vouching for it. Sweeping OTHER runs'
        # partials is safe only because setup_repo.sh, the sole caller, holds
        # an exclusive per-repo lock for the whole run, so no concurrent
        # extract into this repo can exist.
        #
        # The sentinel records WHICH tarball it came from — path, size and
        # mtime, the same cheap fingerprint setup_repo.sh pins in its stage
        # ledger (a content hash of a multi-gigabyte archive would cost minutes
        # per run). A tarball swapped at the same path therefore re-seeds
        # instead of being silently ignored because "something is already
        # seeded".
        try:
            tar_stat = mathlib_build_tar.stat()
            tar_fingerprint = f"{tar_path}:{tar_stat.st_size}:{int(tar_stat.st_mtime)}"
        except OSError:
            tar_fingerprint = ""
        mathlib_build_tar_cmd = f"""
MATHLIB_BUILD_TAR={tar_path!r}
MATHLIB_SEED_FINGERPRINT={tar_fingerprint!r}
MATHLIB_BUILD_DIR=".lake/packages/mathlib/.lake/build"
MATHLIB_SEED_SENTINEL="$MATHLIB_BUILD_DIR/.trellis-mathlib-seed-complete"
MATHLIB_NEEDS_SEED=0
if [[ -f "$MATHLIB_BUILD_TAR" && ! -f "$MATHLIB_SEED_SENTINEL" ]]; then
  MATHLIB_NEEDS_SEED=1
elif [[ -f "$MATHLIB_BUILD_TAR" && "$(cat "$MATHLIB_SEED_SENTINEL")" != "$MATHLIB_SEED_FINGERPRINT" ]]; then
  echo "  mathlib seed tarball changed since the last seed; re-seeding"
  MATHLIB_NEEDS_SEED=1
fi
if [[ "$MATHLIB_NEEDS_SEED" == "1" ]]; then
  echo "  Seeding mathlib build from $MATHLIB_BUILD_TAR"
  rm -rf "$MATHLIB_BUILD_DIR".partial.*
  mkdir -p "$(dirname "$MATHLIB_BUILD_DIR")"
  MATHLIB_BUILD_PARTIAL="$(mktemp -d "$MATHLIB_BUILD_DIR.partial.XXXXXX")"
  tar --extract --file "$MATHLIB_BUILD_TAR" --directory "$MATHLIB_BUILD_PARTIAL" \
    --no-same-owner --no-same-permissions
  printf '%s' "$MATHLIB_SEED_FINGERPRINT" > "$MATHLIB_BUILD_PARTIAL/.trellis-mathlib-seed-complete"
  rm -rf "$MATHLIB_BUILD_DIR"
  mv "$MATHLIB_BUILD_PARTIAL" "$MATHLIB_BUILD_DIR"
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
# `lake exe cache get` fetches the Mathlib olean cache: Mathlib plus every
# dependency Mathlib's *library* imports (batteries, aesop, Qq, importGraph,
# proofwidgets, plausible, LeanSearchClient). Tooling-only deps that Mathlib
# does NOT import — notably `Cli` — are absent from that cache, and
# `lake build Tablet` never pulls them, so they end up with source but zero
# oleans. Derived workspaces (supervisor, checker, grunts) are materialized
# from this .lake and inherit the gap; supervisor_workspace.py can then only
# warn ("source but no prebuilt oleans"), and any node whose imports reach
# such a package fails prepare_compiled_support. Build the known gap here so
# the run repo's .lake is complete for every derived workspace.
if [[ -d ".lake/packages/Cli" ]] && \
   [[ -z "$(find .lake/packages/Cli/.lake/build -name '*.olean' 2>/dev/null | head -1)" ]]; then
  echo "  prewarm: building tooling dep absent from the mathlib olean cache: Cli"
  lake build Cli
fi
# Future-proofing: if a later Mathlib introduces another cache-absent dep,
# surface it loudly at setup time instead of a silent mid-run failure.
for pkgdir in .lake/packages/*/; do
  pkg="$(basename "$pkgdir")"
  [[ "$pkg" == "mathlib" ]] && continue
  if [[ -f "$pkgdir/lakefile.lean" || -f "$pkgdir/lakefile.toml" ]] && \
     [[ -z "$(find "$pkgdir/.lake/build" -name '*.olean' 2>/dev/null | head -1)" ]]; then
    echo "  prewarm: WARNING dependency '$pkg' has source but no oleans after prewarm — 'lake build $pkg' may be required" >&2
  fi
done
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

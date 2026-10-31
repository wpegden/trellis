#!/usr/bin/env bash
# Install elan (the Lean toolchain manager) for the current user.
#
# Idempotent: if elan is already on PATH or `~/.elan/bin/elan` exists,
# the script reports the version and exits.
#
# Prefers the upstream `elan-init.sh` per-user install over the apt
# package because the upstream installer auto-injects PATH into common
# shell rc files; the apt package leaves PATH wiring to the user.
#
# Usage:
#   bash scripts/install_lean_toolchain.sh
set -euo pipefail

ELAN_BIN="${HOME}/.elan/bin/elan"
ELAN_ENV="${HOME}/.elan/env"

if command -v elan >/dev/null 2>&1; then
  echo "elan already on PATH: $(command -v elan)"
  elan --version
  exit 0
fi

if [[ -x "$ELAN_BIN" ]]; then
  echo "elan installed at $ELAN_BIN but not on PATH."
  echo "Run \`source $ELAN_ENV\` (or open a fresh shell) to pick it up."
  "$ELAN_BIN" --version
  exit 0
fi

echo "Installing elan via upstream elan-init.sh (no default toolchain)..."
curl --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh \
  | sh -s -- -y --default-toolchain none

if [[ ! -x "$ELAN_BIN" ]]; then
  echo "ERROR: elan-init.sh ran but $ELAN_BIN is missing." >&2
  exit 1
fi

# shellcheck disable=SC1090
source "$ELAN_ENV"

echo "---"
elan --version
echo "elan installed. Future shells will pick it up via $ELAN_ENV"
echo "(elan-init.sh edits ~/.profile / ~/.bashrc / ~/.zshrc to source it)."
echo
echo "Next: install a Lean toolchain by running e.g."
echo "  cd <project-with-lean-toolchain-file> && lake --version"
echo "elan will auto-install the pinned toolchain on first lake/lean invocation."

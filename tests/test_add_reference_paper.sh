#!/usr/bin/env bash
# Behavioral tests for scripts/add_reference_paper.sh, centered on the
# sha-idempotency rule (VIEWER_RUN_CREATION_DESIGN.md §3.2 / M3a): re-running a
# registration whose source produces the bytes already stored is a success that
# rewrites nothing, an existing file with different content is an immutability
# hard stop, and the crash window between the file write and the config append
# is repaired rather than wedged.
#
# Everything is real: the script itself, git, the transcode, the normalizer.
# The repo is a minimal fixture (git + trellis.config.json), because that is
# all the script reads.
#
# Usage: tests/test_add_reference_paper.sh
#        KEEP_TEST_ROOT=1 tests/test_add_reference_paper.sh   # keep artifacts

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ADD_REF="$SOURCE_ROOT/scripts/add_reference_paper.sh"

TEST_SCRATCH_ROOT="${TEST_SCRATCH_ROOT:-$SOURCE_ROOT/.trellis/test_add_reference_paper}"
mkdir -p "$TEST_SCRATCH_ROOT"
TEST_ROOT="${TEST_ROOT:-$(mktemp -d "$TEST_SCRATCH_ROOT/run.XXXXXX")}"
TEST_ROOT="$(cd "$TEST_ROOT" && pwd)"
echo "test root: $TEST_ROOT"

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "  ok   - $1"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL - $1"; }

check() { # check <description> <condition-cmd...>
  local desc="$1"
  shift
  if "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi
}

check_not() { # check_not <description> <condition-cmd...>
  local desc="$1"
  shift
  if "$@" >/dev/null 2>&1; then bad "$desc"; else ok "$desc"; fi
}

check_eq() { # check_eq <description> <expected> <actual>
  if [ "$2" = "$3" ]; then
    ok "$1"
  else
    bad "$1"
    echo "         expected: $2"
    echo "         actual:   $3"
  fi
}

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------
REPO="$TEST_ROOT/repo"
mkdir -p "$REPO/.trellis/tmp"
git -C "$REPO" init -q
git -C "$REPO" config user.name "test"
git -C "$REPO" config user.email "test@localhost"
printf '{\n  "workflow": {}\n}\n' > "$REPO/trellis.config.json"
git -C "$REPO" add trellis.config.json
git -C "$REPO" commit -q -m "fixture config"

REF_SRC="$TEST_ROOT/reference.tex"
cat > "$REF_SRC" <<'TEX'
\documentclass{article}
\begin{document}
\begin{lemma}\label{lem:ref}
Reference grounding prose.
\end{lemma}
\end{document}
TEX

LATIN1_SRC="$TEST_ROOT/latin1.tex"
printf '\\documentclass{article}\n\\begin{document}\nPoincar\xe9 wrote this.\n\\end{document}\n' \
  > "$LATIN1_SRC"

ADD_STATUS=0
run_add() { # run_add <log-file> <args...>
  local log="$1"
  shift
  bash "$ADD_REF" "$@" >"$log" 2>&1
  ADD_STATUS=$?
}

commit_count() { git -C "$REPO" rev-list --count HEAD; }

entry_summary() { # entry_summary <id> -> "tex_path|source_id" or "<absent>"; errors on duplicates
  python3 - "$REPO/trellis.config.json" "$1" <<'PY'
import json
import sys

data = json.loads(open(sys.argv[1], encoding="utf-8").read())
entries = [
    entry
    for entry in data.get("workflow", {}).get("reference_papers", [])
    if isinstance(entry, dict) and entry.get("id") == sys.argv[2]
]
if not entries:
    print("<absent>")
elif len(entries) > 1:
    print("<duplicated>")
else:
    print(f"{entries[0].get('tex_path')}|{entries[0].get('source_id')}")
PY
}

# ===========================================================================
echo ""
echo "[1] fresh registration writes the file, the entry, and one commit"
# ===========================================================================
run_add "$TEST_ROOT/add-1.log" "$REPO" "$REF_SRC" --id grounding --source-id "arXiv-0000.00000"
check_eq "fresh registration exits 0" "0" "$ADD_STATUS"
check "reference file written" test -f "$REPO/paper/refs/grounding.tex"
check_eq "config entry recorded" "paper/refs/grounding.tex|arXiv-0000.00000" \
  "$(entry_summary grounding)"
check_eq "one commit added" "2" "$(commit_count)"
STORED_SHA="$(sha256sum "$REPO/paper/refs/grounding.tex" | cut -d' ' -f1)"

# ===========================================================================
echo ""
echo "[2] identical re-registration is a no-op success, not an error"
# ===========================================================================
run_add "$TEST_ROOT/add-2.log" "$REPO" "$REF_SRC" --id grounding --source-id "arXiv-0000.00000"
check_eq "identical re-run exits 0" "0" "$ADD_STATUS"
check "re-run says the content is identical" \
  grep -q "identical content" "$TEST_ROOT/add-2.log"
check_eq "stored file untouched" "$STORED_SHA" \
  "$(sha256sum "$REPO/paper/refs/grounding.tex" | cut -d' ' -f1)"
check_eq "still exactly one config entry" "paper/refs/grounding.tex|arXiv-0000.00000" \
  "$(entry_summary grounding)"
check_eq "no empty commit piled up" "2" "$(commit_count)"

# ===========================================================================
echo ""
echo "[3] the crash window (file written, config entry lost) is repaired"
# ===========================================================================
# The file always lands before the config entry, so a crash in between — or a
# config later regenerated from its template — leaves exactly this state.
python3 - "$REPO/trellis.config.json" <<'PY'
import json
import sys

path = sys.argv[1]
data = json.loads(open(path, encoding="utf-8").read())
data["workflow"]["reference_papers"] = []
open(path, "w", encoding="utf-8").write(json.dumps(data, indent=2) + "\n")
PY
git -C "$REPO" commit -q -am "simulate config regenerated without the entry"
run_add "$TEST_ROOT/add-3.log" "$REPO" "$REF_SRC" --id grounding --source-id "arXiv-0000.00000"
check_eq "repair run exits 0" "0" "$ADD_STATUS"
check_eq "config entry restored" "paper/refs/grounding.tex|arXiv-0000.00000" \
  "$(entry_summary grounding)"
check_eq "stored file still untouched" "$STORED_SHA" \
  "$(sha256sum "$REPO/paper/refs/grounding.tex" | cut -d' ' -f1)"
check "repair was committed" \
  git -C "$REPO" diff --quiet HEAD -- trellis.config.json

# ===========================================================================
echo ""
echo "[4] different content under the same id is an immutability hard stop"
# ===========================================================================
OTHER_SRC="$TEST_ROOT/other.tex"
printf 'Different reference content.\n' > "$OTHER_SRC"
COMMITS_BEFORE="$(commit_count)"
run_add "$TEST_ROOT/add-4.log" "$REPO" "$OTHER_SRC" --id grounding --source-id "arXiv-0000.00000"
check "different content is refused" test "$ADD_STATUS" != "0"
check "refusal cites the immutability rule" \
  grep -q "reference papers are immutable" "$TEST_ROOT/add-4.log"
check_eq "stored file untouched by the refusal" "$STORED_SHA" \
  "$(sha256sum "$REPO/paper/refs/grounding.tex" | cut -d' ' -f1)"
check_eq "config untouched by the refusal" "paper/refs/grounding.tex|arXiv-0000.00000" \
  "$(entry_summary grounding)"
check_eq "no commit added by the refusal" "$COMMITS_BEFORE" "$(commit_count)"

# ===========================================================================
echo ""
echo "[5] identical content but a different source_id is refused, not adopted"
# ===========================================================================
run_add "$TEST_ROOT/add-5.log" "$REPO" "$REF_SRC" --id grounding --source-id "some-other-provenance"
check "mismatched source_id is refused" test "$ADD_STATUS" != "0"
check "refusal names the recorded entry" grep -q "different" "$TEST_ROOT/add-5.log"
check_eq "recorded source_id unchanged" "paper/refs/grounding.tex|arXiv-0000.00000" \
  "$(entry_summary grounding)"

# ===========================================================================
echo ""
echo "[6] idempotency is over the STORED bytes: a latin-1 source round-trips"
# ===========================================================================
# The stored file is the transcoded+normalized rendering, so re-running from
# the same latin-1 source must compare equal to it, not to the raw bytes.
run_add "$TEST_ROOT/add-6.log" "$REPO" "$LATIN1_SRC" --id latin --source-id "latin-src"
check_eq "latin-1 registration exits 0" "0" "$ADD_STATUS"
check "registration transcoded" grep -q "transcoded" "$TEST_ROOT/add-6.log"
check "stored file is valid UTF-8" \
  python3 -c 'import sys; open(sys.argv[1], encoding="utf-8").read()' \
  "$REPO/paper/refs/latin.tex"
COMMITS_BEFORE="$(commit_count)"
run_add "$TEST_ROOT/add-7.log" "$REPO" "$LATIN1_SRC" --id latin --source-id "latin-src"
check_eq "latin-1 re-run is idempotent" "0" "$ADD_STATUS"
check "re-run says the content is identical" \
  grep -q "identical content" "$TEST_ROOT/add-7.log"
check_eq "no commit added by the re-run" "$COMMITS_BEFORE" "$(commit_count)"

# ===========================================================================
echo ""
echo "[7] an entry whose file is missing is refused, not silently rewritten"
# ===========================================================================
# The file always lands before the entry, so entry-without-file means someone
# deleted a registered reference — anomalous, and not this script's to repair.
rm "$REPO/paper/refs/latin.tex"
run_add "$TEST_ROOT/add-8.log" "$REPO" "$LATIN1_SRC" --id latin --source-id "latin-src"
check "entry-without-file is refused" test "$ADD_STATUS" != "0"
check "refusal names the existing registration" \
  grep -q "already registers reference paper id" "$TEST_ROOT/add-8.log"
check_not "the deleted file was not quietly recreated" test -e "$REPO/paper/refs/latin.tex"

# ===========================================================================
echo ""
echo "passed: $PASS   failed: $FAIL"
if [ "$FAIL" -ne 0 ]; then
  echo "artifacts kept at $TEST_ROOT"
  exit 1
fi
if [ -z "${KEEP_TEST_ROOT:-}" ]; then
  rm -rf "$TEST_ROOT"
fi
exit 0

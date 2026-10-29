#!/usr/bin/env bash
# Stage-ledger / resumability tests for scripts/setup_repo.sh.
#
# VIEWER_RUN_CREATION_DESIGN.md §9 asks for: "clean run → kill at S9 →
# --resume → assert skipped/redone stage set; then the byte-equivalence check:
# clean-build repo vs killed-and-resumed repo, `git ls-files -s` output
# identical."
#
# WHAT IS REAL HERE: the whole of setup_repo.sh, the kernel CLI (real
# trellis_runtime_cli, for target resolution and sync_tablet_support), git,
# bwrap (both sandbox probes), the permission walk, the provider CLI probe,
# tar, and add_reference_paper.sh.
#
# WHAT IS STUBBED: `lake`, and only via $BURST_PATH — the env var setup_repo.sh
# already documents as the PATH for its in-bwrap validation and prewarm steps.
# The real prewarm downloads and builds several GB of Mathlib over the network,
# which is neither cheap nor deterministic. The stub creates exactly the
# artifacts the prewarm probe reads (Mathlib.olean plus a lake trace file), so
# the skip-vs-redo decisions under test are the real ones. The stub also
# doubles as the interrupt trigger: when told to, it kills setup's whole
# process group mid-stage, which is the closest reachable analogue of the
# host-reboot case the stage classes exist for.
#
# Everything is created under $TEST_ROOT. No live run directory is touched.
#
# Usage: tests/test_setup_stage_ledger.sh
#        KEEP_TEST_ROOT=1 tests/test_setup_stage_ledger.sh   # keep artifacts

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SETUP="$SOURCE_ROOT/scripts/setup_repo.sh"

# Artifacts land under the trellis checkout's own gitignored scratch area, not
# /tmp: this host's /tmp is a small shared partition, and a failing run keeps
# its repos (each carrying a .lake tree) for inspection.
TEST_SCRATCH_ROOT="${TEST_SCRATCH_ROOT:-$SOURCE_ROOT/.trellis/test_setup_stage_ledger}"
mkdir -p "$TEST_SCRATCH_ROOT"
TEST_ROOT="${TEST_ROOT:-$(mktemp -d "$TEST_SCRATCH_ROOT/run.XXXXXX")}"
mkdir -p "$TEST_ROOT"
# MUST be absolute. The lake stub is injected as a $BURST_PATH entry, and the
# prewarm shell `cd`s into the repo before invoking lake: a relative stub path
# silently stops resolving there, the REAL lake runs, and the "cheap stubbed
# test" becomes a multi-gigabyte mathlib clone and cache download.
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
PAPER="$TEST_ROOT/paper.tex"
cat > "$PAPER" <<'TEX'
\documentclass{article}
\begin{document}
\section{Intro}

\begin{theorem}\label{thm:main}
Every finite nonempty totally ordered set has a maximum.
\end{theorem}

\begin{corollary}\label{cor:side}
Every finite nonempty set of naturals has a maximum.
\end{corollary}

\end{document}
TEX

REF_PAPER="$TEST_ROOT/reference.tex"
cat > "$REF_PAPER" <<'TEX'
\documentclass{article}
\begin{document}
\begin{lemma}\label{lem:ref}
A reference paper contributes prose grounding only.
\end{lemma}
\end{document}
TEX

# Stand-in for the mathlib build tarball (contents of
# .lake/packages/mathlib/.lake/build): small, but shaped like the real thing —
# Mathlib.olean plus traces plus enough siblings that a partial extract is
# distinguishable from a complete one.
SEED_SRC="$TEST_ROOT/seed-src"
mkdir -p "$SEED_SRC/lib/lean"
printf 'olean-bytes-Mathlib\n' > "$SEED_SRC/lib/lean/Mathlib.olean"
printf 'trace\n' > "$SEED_SRC/lib/lean/Mathlib.trace"
for i in 1 2 3 4 5 6 7 8; do
  printf 'olean-bytes-%s\n' "$i" > "$SEED_SRC/lib/lean/Part$i.olean"
  printf 'trace\n' > "$SEED_SRC/lib/lean/Part$i.trace"
done
MATHLIB_TAR="$TEST_ROOT/mathlib-build.tar"
tar --create --file "$MATHLIB_TAR" --directory "$SEED_SRC" .

# The lake stub, driven per-invocation by:
#   LAKE_LOG         one line per lake invocation
#   LAKE_KILL_MATCH  glob over the joined argv; on match, kill setup's whole
#                    process group (simulated crash)
#   LAKE_KILL_SKIP   let this many matches through before killing
STUB_BIN="$TEST_ROOT/stub-bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/lake" <<'LAKE'
#!/usr/bin/env bash
set -uo pipefail
argv="$*"
if [ -n "${LAKE_LOG:-}" ]; then
  printf '%s\n' "$argv" >> "$LAKE_LOG"
fi
if [ -n "${LAKE_KILL_MATCH:-}" ]; then
  # shellcheck disable=SC2254
  case "$argv" in
    $LAKE_KILL_MATCH)
      seen=0
      if [ -n "${LAKE_KILL_COUNTER:-}" ] && [ -f "${LAKE_KILL_COUNTER}" ]; then
        seen="$(cat "$LAKE_KILL_COUNTER")"
      fi
      if [ "$seen" -ge "${LAKE_KILL_SKIP:-0}" ]; then
        pgid="$(ps -o pgid= -p $$ | tr -d ' ')"
        kill -9 -"$pgid"
        sleep 30
      fi
      if [ -n "${LAKE_KILL_COUNTER:-}" ]; then
        printf '%s' "$((seen + 1))" > "$LAKE_KILL_COUNTER"
      fi
      ;;
  esac
fi
build_dir=".lake/packages/mathlib/.lake/build"
case "$argv" in
  "update")
    mkdir -p ".lake/packages/mathlib/.lake"
    ;;
  "exe cache get")
    mkdir -p "$build_dir/lib/lean"
    printf 'olean-bytes-Mathlib\n' > "$build_dir/lib/lean/Mathlib.olean"
    printf 'trace\n' > "$build_dir/lib/lean/Mathlib.trace"
    ;;
  "build"*)
    mkdir -p ".lake/build/lib/lean"
    ;;
esac
exit 0
LAKE
chmod +x "$STUB_BIN/lake"

# ---------------------------------------------------------------------------
# Harness
# ---------------------------------------------------------------------------
BURST_HOME_DIR="$TEST_ROOT/burst-home"
STATIC_DIR="$TEST_ROOT/trellis-web"
SCRATCH_DIR="$TEST_ROOT/setup-scratch"
mkdir -p "$BURST_HOME_DIR" "$STATIC_DIR" "$SCRATCH_DIR"

LAKE_LOG=""
LAKE_KILL_MATCH=""
LAKE_KILL_SKIP=0
SETUP_STATUS=0
# Config template to run setup under; empty means setup's default template.
CONFIG_TEMPLATE_OVERRIDE=""
# Directory to run setup FROM. Empty means "wherever the suite was invoked",
# which is what every test but the containment one wants. Test [13] sets it to
# an empty directory it then asserts is still empty, because setup used to
# materialize a worker skeleton in its own cwd.
SETUP_CWD=""

run_setup() { # run_setup <log-file> <setup args...>
  local log="$1"
  shift
  local counter="$TEST_ROOT/kill-counter"
  rm -f "$counter"
  ( if [ -n "$SETUP_CWD" ]; then cd "$SETUP_CWD" || exit 1; fi
    env \
      BURST_PATH="$STUB_BIN:/usr/local/bin:/usr/bin:/bin" \
      BURST_HOME="$BURST_HOME_DIR" \
      STATIC_OUT="$STATIC_DIR" \
      SETUP_SCRATCH_ROOT="$SCRATCH_DIR" \
      CONFIG_TEMPLATE="$CONFIG_TEMPLATE_OVERRIDE" \
      LAKE_LOG="$LAKE_LOG" \
      LAKE_KILL_MATCH="$LAKE_KILL_MATCH" \
      LAKE_KILL_SKIP="$LAKE_KILL_SKIP" \
      LAKE_KILL_COUNTER="$counter" \
      setsid --wait bash "$SETUP" "$@" >"$log" 2>&1 )
  SETUP_STATUS=$?
}

ledger_status() { # ledger_status <repo> <stage>
  python3 - "$1/.trellis/setup_stages.json" "$2" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
if not path.is_file():
    print("<no-ledger>")
    raise SystemExit(0)
try:
    data = json.loads(path.read_text(encoding="utf-8"))
except Exception:
    print("<corrupt>")
    raise SystemExit(0)
record = data.get("stages", {}).get(sys.argv[2])
print(record.get("status") if record else "<absent>")
PY
}

config_has_reference() { # config_has_reference <repo> <id>
  python3 - "$1/trellis.config.json" "$2" <<'PY'
import json
import sys

data = json.loads(open(sys.argv[1], encoding="utf-8").read())
entries = data.get("workflow", {}).get("reference_papers", [])
print(any(isinstance(e, dict) and e.get("id") == sys.argv[2] for e in entries))
PY
}

config_main_result_envs() { # config_main_result_envs <repo> -> "<absent>" or CSV
  python3 - "$1/trellis.config.json" <<'PY'
import json
import sys

data = json.loads(open(sys.argv[1], encoding="utf-8").read())
workflow = data.get("workflow", {})
if "main_result_envs" not in workflow:
    print("<absent>")
else:
    print(",".join(workflow["main_result_envs"]))
PY
}

config_target_summary() { # config_target_summary <repo> -> one start:end:label per target, comma-joined
  python3 - "$1/trellis.config.json" <<'PY'
import json
import sys

data = json.loads(open(sys.argv[1], encoding="utf-8").read())
targets = data.get("workflow", {}).get("main_result_targets", [])
parts = []
for target in targets:
    parts.append(
        f"{target.get('start_line')}:{target.get('end_line')}:{target.get('tex_label', '-')}"
    )
print(",".join(parts))
PY
}

trees_equal() { # trees_equal <repo> <baseline-file>
  diff <(git -C "$1" ls-files -s) "$2" >/dev/null
}

json_loads() { python3 -c 'import json,sys; json.load(open(sys.argv[1])); print("ok")' "$1"; }

no_partial_seed_dir() {
  [ -z "$(find "$1/.lake/packages/mathlib/.lake" -maxdepth 1 -name 'build.partial.*' -print -quit 2>/dev/null)" ]
}

dump_on_failure() { # dump_on_failure <log>
  if [ "$SETUP_STATUS" != "0" ]; then
    echo "----- tail $1 -----"
    tail -25 "$1"
    echo "-------------------"
  fi
}

# ===========================================================================
echo ""
echo "[1] clean run, no new flags — the hand-operator path is unchanged"
# ===========================================================================
REPO="$TEST_ROOT/repo-a"
LAKE_LOG="$TEST_ROOT/lake-a.log"; LAKE_KILL_MATCH=""
run_setup "$TEST_ROOT/setup-a.log" --loogle off --yes \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO" "$PAPER" ledger-test
check_eq "clean setup exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-a.log"
# Fail loudly if the stub was bypassed: an empty stub log or a real mathlib
# checkout means the run reached for the network instead of the fixture.
check "the lake stub was the lake that ran" test -s "$TEST_ROOT/lake-a.log"
check_not "no real mathlib was cloned" test -e "$REPO/.lake/packages/mathlib/.git"
check "repo has an initial commit" git -C "$REPO" rev-parse HEAD
check "stage ledger written" test -f "$REPO/.trellis/setup_stages.json"
check_not "ledger is untracked (.trellis is gitignored)" \
  git -C "$REPO" ls-files --error-unmatch .trellis/setup_stages.json
check_eq "prewarm recorded done" "done" "$(ledger_status "$REPO" prewarm)"
check_eq "initial_commit recorded done" "done" "$(ledger_status "$REPO" initial_commit)"
check_eq "config is valid JSON" "ok" "$(json_loads "$REPO/trellis.config.json")"
# The env knob's absent-key default: a setup with no --main-result-envs writes
# NO workflow.main_result_envs key, keeping the config byte-identical to a
# pre-knob setup (VIEWER_RUN_CREATION_DESIGN.md §1.4).
check_eq "no-knob config carries no main_result_envs key" "<absent>" \
  "$(config_main_result_envs "$REPO")"

SEED_DIR="$REPO/.lake/packages/mathlib/.lake/build"
check "tar seed sentinel present" test -f "$SEED_DIR/.trellis-mathlib-seed-complete"
check "tar seed left no partial dir" no_partial_seed_dir "$REPO"
check "tar seed extracted the whole tree" test -f "$SEED_DIR/lib/lean/Part8.olean"
check_not "seed suppressed 'lake exe cache get'" grep -qx 'exe cache get' "$TEST_ROOT/lake-a.log"

BASELINE="$TEST_ROOT/baseline-ls-files.txt"
git -C "$REPO" ls-files -s > "$BASELINE"
check "baseline tracked tree is non-empty" test -s "$BASELINE"

echo ""
echo "[1b] re-running without --resume on an existing repo still refuses"
run_setup "$TEST_ROOT/setup-a2.log" --loogle off --yes "$REPO" "$PAPER" ledger-test
check_eq "second plain run exits 1" "1" "$SETUP_STATUS"
check "refusal names --resume" grep -q -- "--resume" "$TEST_ROOT/setup-a2.log"
check "refusal names --reset" grep -q -- "--reset" "$TEST_ROOT/setup-a2.log"

echo ""
echo "[1c] the confirmation gate is untouched, and declining still creates nothing"
TTY_REPO="$TEST_ROOT/repo-tty"
run_setup "$TEST_ROOT/setup-a3.log" --loogle off "$TTY_REPO" "$PAPER" ledger-tty
check "headless run without --yes refuses" test "$SETUP_STATUS" != "0"
check "refusal asks for --yes" grep -q -- "Re-run with --yes" "$TEST_ROOT/setup-a3.log"

# Same gate over a real pty, answering "no": the ledger must not have caused
# any repo write before the operator agreed.
( env \
    BURST_PATH="$STUB_BIN:/usr/local/bin:/usr/bin:/bin" \
    BURST_HOME="$BURST_HOME_DIR" \
    STATIC_OUT="$STATIC_DIR" \
    SETUP_SCRATCH_ROOT="$SCRATCH_DIR" \
    script -qec "bash $SETUP --loogle off $TTY_REPO $PAPER ledger-tty" /dev/null \
    <<< "n" > "$TEST_ROOT/setup-a4.log" 2>&1 )
check "declined prompt aborts" grep -q "Aborted." "$TEST_ROOT/setup-a4.log"
check_not "declined prompt leaves no repo behind" test -e "$TTY_REPO"

# ===========================================================================
echo ""
echo "[2] interrupt INSIDE the verify-skip stage (S9 prewarm) -> resume redoes it"
# ===========================================================================
rm -rf "$REPO" "$STATIC_DIR/ledger-test"
LAKE_LOG="$TEST_ROOT/lake-b1.log"; LAKE_KILL_MATCH="build Tablet"; LAKE_KILL_SKIP=0
run_setup "$TEST_ROOT/setup-b1.log" --loogle off --yes \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO" "$PAPER" ledger-test
check "interrupted run did not exit 0" test "$SETUP_STATUS" != "0"
check_eq "prewarm left mid-flight in the ledger" "running" "$(ledger_status "$REPO" prewarm)"
check_eq "no initial commit recorded" "<absent>" "$(ledger_status "$REPO" initial_commit)"

LAKE_LOG="$TEST_ROOT/lake-b2.log"; LAKE_KILL_MATCH=""
run_setup "$TEST_ROOT/setup-b2.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO" "$PAPER" ledger-test
check_eq "resume exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-b2.log"
check "resume re-ran the interrupted prewarm" grep -qx 'update' "$TEST_ROOT/lake-b2.log"
check_eq "prewarm now done" "done" "$(ledger_status "$REPO" prewarm)"
check "resumed tree == clean tree (git ls-files -s)" trees_equal "$REPO" "$BASELINE"

# ===========================================================================
echo ""
echo "[3] interrupt AFTER the verify-skip stage (S11) -> resume skips prewarm"
# ===========================================================================
rm -rf "$REPO" "$STATIC_DIR/ledger-test"
# The prewarm's own smoke check is the first `lake env lean`; S11's worker
# shared-access check is the second. Skip one, kill the other.
LAKE_LOG="$TEST_ROOT/lake-c1.log"; LAKE_KILL_MATCH="env lean*"; LAKE_KILL_SKIP=1
run_setup "$TEST_ROOT/setup-c1.log" --loogle off --yes \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO" "$PAPER" ledger-test
check "interrupted run did not exit 0" test "$SETUP_STATUS" != "0"
check_eq "prewarm completed before the interrupt" "done" "$(ledger_status "$REPO" prewarm)"
check_eq "worker_access left mid-flight" "running" "$(ledger_status "$REPO" worker_access)"

LAKE_LOG="$TEST_ROOT/lake-c2.log"; LAKE_KILL_MATCH=""; LAKE_KILL_SKIP=0
run_setup "$TEST_ROOT/setup-c2.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO" "$PAPER" ledger-test
check_eq "resume exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-c2.log"
check_not "resume skipped the prewarm entirely (no lake update)" \
  grep -qx 'update' "$TEST_ROOT/lake-c2.log"
check "resume announced the skip" grep -q "skip prewarm" "$TEST_ROOT/setup-c2.log"
check "resume re-ran the always-redo worker access check" \
  grep -q 'env lean' "$TEST_ROOT/lake-c2.log"
check "resumed tree == clean tree (git ls-files -s)" trees_equal "$REPO" "$BASELINE"

# ===========================================================================
echo ""
echo "[4] an interrupted tar seed is never trusted (design row 13)"
# ===========================================================================
# Reproduce what a killed `tar --extract` used to leave behind: Mathlib.olean
# present (so the old marker check would have skipped both the seed and
# `lake exe cache get`) with most of the tree missing and that olean truncated.
rm -rf "$SEED_DIR"
mkdir -p "$SEED_DIR/lib/lean"
printf 'truncated' > "$SEED_DIR/lib/lean/Mathlib.olean"
mkdir -p "$REPO/.lake/packages/mathlib/.lake/build.partial.stale"
printf 'junk\n' > "$REPO/.lake/packages/mathlib/.lake/build.partial.stale/junk"

LAKE_LOG="$TEST_ROOT/lake-d.log"; LAKE_KILL_MATCH=""
run_setup "$TEST_ROOT/setup-d.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO" "$PAPER" ledger-test
check_eq "resume over a partial seed exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-d.log"
check "partial seed was rejected by the probe and re-seeded" \
  test -f "$SEED_DIR/.trellis-mathlib-seed-complete"
check "re-seeded tree is complete" test -f "$SEED_DIR/lib/lean/Part8.olean"
check_eq "truncated olean was replaced" "olean-bytes-Mathlib" \
  "$(cat "$SEED_DIR/lib/lean/Mathlib.olean")"
check "stale partial dir was discarded" no_partial_seed_dir "$REPO"
check "tree still matches the clean build" trees_equal "$REPO" "$BASELINE"

# ===========================================================================
echo ""
echo "[5] reference papers: verify-skip, and the config entry survives resume"
# ===========================================================================
REPO2="$TEST_ROOT/repo-b"
LAKE_LOG="$TEST_ROOT/lake-e1.log"; LAKE_KILL_MATCH=""
run_setup "$TEST_ROOT/setup-e1.log" --loogle off --yes \
  --reference "grounding=$REF_PAPER:arXiv-0000.00000" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO2" "$PAPER" ledger-ref
check_eq "clean run with a reference exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-e1.log"
check "reference file written" test -f "$REPO2/paper/refs/grounding.tex"
check_eq "reference registered in config" "True" "$(config_has_reference "$REPO2" grounding)"
REF_BASELINE="$TEST_ROOT/baseline-ref-ls-files.txt"
git -C "$REPO2" ls-files -s > "$REF_BASELINE"
COMMITS_BEFORE="$(git -C "$REPO2" rev-list --count HEAD)"

LAKE_LOG="$TEST_ROOT/lake-e2.log"
run_setup "$TEST_ROOT/setup-e2.log" --loogle off --yes --resume \
  --reference "grounding=$REF_PAPER:arXiv-0000.00000" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO2" "$PAPER" ledger-ref
check_eq "resume with the same reference exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-e2.log"
check_eq "reference still registered after the config rewrite" "True" \
  "$(config_has_reference "$REPO2" grounding)"
check "resumed tree == clean tree with reference" trees_equal "$REPO2" "$REF_BASELINE"
# Re-registration reproduces byte-identical content, so it must not pile up an
# empty commit per resume.
check_eq "resume added no commit" "$COMMITS_BEFORE" "$(git -C "$REPO2" rev-list --count HEAD)"
check_not "no unreachable 'skip reference_paper' claim in any log" \
  grep -rq "skip reference_paper" "$TEST_ROOT"

echo ""
echo "[5a] resuming without the --reference spec refuses to drop the registration"
LAKE_LOG="$TEST_ROOT/lake-e2b.log"
run_setup "$TEST_ROOT/setup-e2b.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO2" "$PAPER" ledger-ref
check "dropped reference spec fails the run" test "$SETUP_STATUS" != "0"
check "failure names the pinned input" grep -q "reference_ids:" "$TEST_ROOT/setup-e2b.log"
# Put the registration back for the next case.
LAKE_LOG="$TEST_ROOT/lake-e2c.log"
run_setup "$TEST_ROOT/setup-e2c.log" --loogle off --yes --resume \
  --reference "grounding=$REF_PAPER:arXiv-0000.00000" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO2" "$PAPER" ledger-ref
check_eq "re-supplying the spec recovers" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-e2c.log"

echo ""
echo "[5c] a reference registered out of band is not silently dropped either"
bash "$SOURCE_ROOT/scripts/add_reference_paper.sh" "$REPO2" "$REF_PAPER" \
  --id extra --source-id extra-src > "$TEST_ROOT/add-ref-extra.log" 2>&1
check "out-of-band registration succeeded" test -f "$REPO2/paper/refs/extra.tex"
LAKE_LOG="$TEST_ROOT/lake-e2d.log"
run_setup "$TEST_ROOT/setup-e2d.log" --loogle off --yes --resume \
  --reference "grounding=$REF_PAPER:arXiv-0000.00000" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO2" "$PAPER" ledger-ref
check "resume that would drop the out-of-band id fails" test "$SETUP_STATUS" != "0"
check "failure names the unregistered id" \
  grep -q "no entry for 'extra'" "$TEST_ROOT/setup-e2d.log"
rm -f "$REPO2/paper/refs/extra.tex"
git -C "$REPO2" rm -q --cached "paper/refs/extra.tex" >/dev/null 2>&1 || true

echo ""
echo "[5b] a reference id whose stored file this setup did not write is refused"
printf 'different content\n' > "$REPO2/paper/refs/grounding.tex"
LAKE_LOG="$TEST_ROOT/lake-e3.log"
run_setup "$TEST_ROOT/setup-e3.log" --loogle off --yes --resume \
  --reference "grounding=$REF_PAPER:arXiv-0000.00000" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO2" "$PAPER" ledger-ref
check "mismatched reference content fails the run" test "$SETUP_STATUS" != "0"
check "failure cites the immutability rule" \
  grep -q "reference papers are immutable" "$TEST_ROOT/setup-e3.log"

# ===========================================================================
echo ""
echo "[6] a corrupt ledger is treated as absent, never as truth"
# ===========================================================================
REPO3="$TEST_ROOT/repo-c"
LAKE_LOG="$TEST_ROOT/lake-f1.log"
run_setup "$TEST_ROOT/setup-f1.log" --loogle off --yes \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO3" "$PAPER" ledger-corrupt
check_eq "clean run exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-f1.log"
printf '{"version": 1, "stages": {"prewa' > "$REPO3/.trellis/setup_stages.json"
LAKE_LOG="$TEST_ROOT/lake-f2.log"
run_setup "$TEST_ROOT/setup-f2.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO3" "$PAPER" ledger-corrupt
check_eq "resume over a corrupt ledger exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-f2.log"
check "corrupt ledger forced the prewarm to re-run" \
  grep -qx 'update' "$TEST_ROOT/lake-f2.log"
check_eq "ledger rewritten" "done" "$(ledger_status "$REPO3" prewarm)"

# ===========================================================================
echo ""
echo "[7] wipe-redo class: init_new_run.sh --resume rebuilds the runtime root"
# ===========================================================================
INIT_NEW_RUN="$SOURCE_ROOT/scripts/init_new_run.sh"
# The dev-only bring-up launcher does not ship in public releases; the class
# it guards (wipe-redo of a never-launched runtime root) is dev-host behavior.
if [[ ! -f "$INIT_NEW_RUN" ]]; then
echo "[7] SKIPPED: init_new_run.sh not present in this checkout (dev-only launcher)"
else
REPO4="$TEST_ROOT/repo-d"
RUNTIME4="$TEST_ROOT/repo-d-runtime"

run_init() { # run_init <log-file> <args...>
  local log="$1"
  shift
  ( env \
      BURST_PATH="$STUB_BIN:/usr/local/bin:/usr/bin:/bin" \
      BURST_HOME="$BURST_HOME_DIR" \
      STATIC_OUT="$STATIC_DIR" \
      SETUP_SCRATCH_ROOT="$SCRATCH_DIR" \
      LAKE_LOG="$LAKE_LOG" \
      LAKE_KILL_MATCH="" \
      bash "$INIT_NEW_RUN" "$@" >"$log" 2>&1 )
  SETUP_STATUS=$?
}

LAKE_LOG="$TEST_ROOT/lake-g1.log"
run_init "$TEST_ROOT/init-g1.log" --loogle off --yes --no-current --no-viewer-restart \
  --template "$SOURCE_ROOT/examples/trellis.config.json" \
  --runtime-root "$RUNTIME4" "$REPO4" "$PAPER" ledger-init
check_eq "init_new_run exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/init-g1.log"
check "runtime root created" test -d "$RUNTIME4"

# Plain re-run still refuses (unchanged behavior for the hand path).
LAKE_LOG="$TEST_ROOT/lake-g2.log"
run_init "$TEST_ROOT/init-g2.log" --loogle off --yes --no-current --no-viewer-restart \
  --template "$SOURCE_ROOT/examples/trellis.config.json" \
  --runtime-root "$RUNTIME4" "$REPO4" "$PAPER" ledger-init
check "plain re-run refuses an existing repo" test "$SETUP_STATUS" != "0"
check "refusal points at --resume" grep -q -- "--resume" "$TEST_ROOT/init-g2.log"

# The runtime root is wipe-redo: whatever a half-finished init left behind is
# removed, never re-entered.
rm -f "$RUNTIME4/protocol_state.json"
LAKE_LOG="$TEST_ROOT/lake-g3.log"
run_init "$TEST_ROOT/init-g3.log" --loogle off --yes --resume --no-current --no-viewer-restart \
  --template "$SOURCE_ROOT/examples/trellis.config.json" \
  --runtime-root "$RUNTIME4" "$REPO4" "$PAPER" ledger-init
check_eq "resumed init exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/init-g3.log"
check "runtime root re-created" test -d "$RUNTIME4"
check "half-initialized root was rebuilt, not re-entered" \
  test -f "$RUNTIME4/protocol_state.json"

# ...but the wipe's precondition ("nothing here is irreplaceable because the
# run never launched") is checked, not assumed.
printf '{"launched": true}\n' > "$RUNTIME4/launch_env.json"
mkdir -p "$RUNTIME4/sockets"
LAKE_LOG="$TEST_ROOT/lake-g4.log"
run_init "$TEST_ROOT/init-g4.log" --loogle off --yes --resume --no-current --no-viewer-restart \
  --template "$SOURCE_ROOT/examples/trellis.config.json" \
  --runtime-root "$RUNTIME4" "$REPO4" "$PAPER" ledger-init
check "resume over a launched runtime root is refused" test "$SETUP_STATUS" != "0"
check "refusal names the evidence" grep -q "launch_env.json" "$TEST_ROOT/init-g4.log"
check "the launched runtime root survived" test -f "$RUNTIME4/launch_env.json"
fi

# ===========================================================================
echo ""
echo "[8] a resume that drops --challenge-targets cannot silently unpin the toolchain"
# ===========================================================================
CHALLENGE_JSON="$TEST_ROOT/challenge_targets.json"
PINNED_TOOLCHAIN="leanprover/lean4:v4.99.0-pinned"
PINNED_REV="00000000000000000000000000000000deadbeef"
cat > "$CHALLENGE_JSON" <<JSON
{
  "schema_version": 1,
  "problem_id": "ledger_test",
  "toolchain": {
    "MATHLIB_TOOLCHAIN": "$PINNED_TOOLCHAIN",
    "MATHLIB_REV": "$PINNED_REV"
  },
  "targets": [
    {"id": "challenge:ledger_test", "kind": "theorem", "name": "ledger_test",
     "lean": "theorem ledger_test : True := by", "namespace_context": "", "informal": "",
     "provenance": {"problem_id": "ledger_test", "source_file": "Challenge.lean",
                    "source_sha256": "abc"}}
  ]
}
JSON

REPO5="$TEST_ROOT/repo-e"
LAKE_LOG="$TEST_ROOT/lake-h1.log"
run_setup "$TEST_ROOT/setup-h1.log" --loogle off --yes \
  --challenge-targets "$CHALLENGE_JSON" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO5" "$PAPER" ledger-challenge
check_eq "clean run with challenge targets exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-h1.log"
check_eq "toolchain pinned by the spec" "$PINNED_TOOLCHAIN" "$(cat "$REPO5/lean-toolchain")"
check "lakefile pinned by the spec" grep -q "$PINNED_REV" "$REPO5/lakefile.lean"

LAKE_LOG="$TEST_ROOT/lake-h2.log"
run_setup "$TEST_ROOT/setup-h2.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO5" "$PAPER" ledger-challenge
check "resume without --challenge-targets is refused" test "$SETUP_STATUS" != "0"
check "refusal names challenge_targets" grep -q "challenge_targets:" "$TEST_ROOT/setup-h2.log"
check "refusal names mathlib_rev" grep -q "mathlib_rev:" "$TEST_ROOT/setup-h2.log"
check_eq "toolchain still pinned after the refusal" "$PINNED_TOOLCHAIN" \
  "$(cat "$REPO5/lean-toolchain")"
check "lakefile still pinned after the refusal" grep -q "$PINNED_REV" "$REPO5/lakefile.lean"

LAKE_LOG="$TEST_ROOT/lake-h3.log"
run_setup "$TEST_ROOT/setup-h3.log" --loogle off --yes --resume \
  --challenge-targets "$CHALLENGE_JSON" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO5" "$PAPER" ledger-challenge
check_eq "resume with the same spec exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-h3.log"
check_eq "read-only challenge file was replaced in place" "0" "$SETUP_STATUS"

# --reconfigure is the deliberate way to change a pinned input.
LAKE_LOG="$TEST_ROOT/lake-h4.log"
run_setup "$TEST_ROOT/setup-h4.log" --loogle off --yes --resume --reconfigure \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO5" "$PAPER" ledger-challenge
check_eq "--reconfigure adopts the change" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-h4.log"
check "--reconfigure reports what it is changing" \
  grep -q "adopting changed inputs" "$TEST_ROOT/setup-h4.log"
check_not "toolchain is no longer the spec pin" grep -qx "$PINNED_TOOLCHAIN" "$REPO5/lean-toolchain"

# ===========================================================================
echo ""
echo "[9] a resume that drops --main-result-labels cannot silently widen targets"
# ===========================================================================
REPO6="$TEST_ROOT/repo-f"
LAKE_LOG="$TEST_ROOT/lake-i1.log"
run_setup "$TEST_ROOT/setup-i1.log" --loogle off --yes \
  --main-result-labels thm:main \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO6" "$PAPER" ledger-labels
check_eq "clean run with explicit labels exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-i1.log"
config_labels() {
  python3 -c 'import json,sys; print(",".join(json.load(open(sys.argv[1]))["workflow"]["main_result_labels"]))' \
    "$REPO6/trellis.config.json"
}
check_eq "reviewed target set is exactly thm:main" "thm:main" "$(config_labels)"

LAKE_LOG="$TEST_ROOT/lake-i2.log"
run_setup "$TEST_ROOT/setup-i2.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO6" "$PAPER" ledger-labels
check "resume without --main-result-labels is refused" test "$SETUP_STATUS" != "0"
check "refusal names main_result_labels" grep -q "main_result_labels:" "$TEST_ROOT/setup-i2.log"
check_eq "reviewed target set untouched by the refusal" "thm:main" "$(config_labels)"

# ===========================================================================
echo ""
echo "[10] --resume refuses a repo that has moved on into a run"
# ===========================================================================
printf 'theorem worker_authored : True := trivial\n' > "$REPO3/Tablet/WorkerNode.lean"
LAKE_LOG="$TEST_ROOT/lake-j1.log"
run_setup "$TEST_ROOT/setup-j1.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO3" "$PAPER" ledger-corrupt
check "resume over a run-bearing repo is refused" test "$SETUP_STATUS" != "0"
check "refusal explains why" grep -q "progressed past setup into a run" "$TEST_ROOT/setup-j1.log"
check "worker-authored file untouched" test -f "$REPO3/Tablet/WorkerNode.lean"
rm -f "$REPO3/Tablet/WorkerNode.lean"

# ===========================================================================
echo ""
echo "[11] concurrent setups against one repo are refused, not interleaved"
# ===========================================================================
LOCK_FILE="$TEST_ROOT/.repo-c.setup.lock"
flock -x "$LOCK_FILE" -c 'sleep 20' &
HOLDER_PID=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if ! flock -n "$LOCK_FILE" -c true 2>/dev/null; then break; fi
  sleep 0.2
done
LAKE_LOG="$TEST_ROOT/lake-k1.log"
run_setup "$TEST_ROOT/setup-k1.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO3" "$PAPER" ledger-corrupt
check "second concurrent setup is refused" test "$SETUP_STATUS" != "0"
check "refusal names the lock" grep -q "already running for this repo" "$TEST_ROOT/setup-k1.log"
# Killing the flock process leaves its `sleep` child holding the inherited
# descriptor, so wait on the lock itself rather than on the pid.
pkill -P "$HOLDER_PID" 2>/dev/null
kill "$HOLDER_PID" 2>/dev/null
wait "$HOLDER_PID" 2>/dev/null
check "lock released after the holder exits" flock -w 60 "$LOCK_FILE" -c true

# ===========================================================================
echo ""
echo "[12] a tarball swapped at the same path re-seeds instead of being ignored"
# ===========================================================================
printf 'olean-bytes-New\n' > "$SEED_SRC/lib/lean/NewPart.olean"
printf 'trace\n' > "$SEED_SRC/lib/lean/NewPart.trace"
sleep 1  # keep the mtime distinguishable at 1s granularity
tar --create --file "$MATHLIB_TAR" --directory "$SEED_SRC" .
LAKE_LOG="$TEST_ROOT/lake-l1.log"
run_setup "$TEST_ROOT/setup-l1.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO3" "$PAPER" ledger-corrupt
check_eq "resume after a tarball swap exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-l1.log"
check "the new tarball's content was seeded" \
  test -f "$REPO3/.lake/packages/mathlib/.lake/build/lib/lean/NewPart.olean"
check "re-seed was announced" grep -q "seed tarball changed" "$TEST_ROOT/setup-l1.log"
check "no partial dir left behind" no_partial_seed_dir "$REPO3"

# ===========================================================================
echo ""
echo "[13] setup writes nothing outside the repo it was given"
# ===========================================================================
# A run repo's parent directory is, on the operator's host, the directory that
# holds every OTHER run. Setup materializing anything there is not a cosmetic
# problem: `.trellis/` and `Tablet/` are the names by which tooling recognizes
# a run repo.
#
# The regression this pins: S4's bwrap preflight handed `probe_sandbox` the
# repo's PARENT as its work_dir whenever the repo already existed (so: every
# --reset and every --resume), and `Path.cwd()` when it did not. `wrap_command`
# treats work_dir as a repo and creates the worker's whole writable-path set
# inside it, so the parent grew Tablet/, reference/, .trellis/ and .lake/build.
#
# Exactly one thing may appear beside the repo, by documented design: the
# per-repo setup lock, which has to exist before the repo does and survive
# --reset's `rm -rf`. Everything else is a leak.
CONTAIN_PARENT="$TEST_ROOT/containment"
CONTAIN_REPO="$CONTAIN_PARENT/repo-z"
CONTAIN_CWD="$TEST_ROOT/containment-cwd"
mkdir -p "$CONTAIN_PARENT" "$CONTAIN_CWD"

dir_children() { # dir_children <dir> -> sorted space-separated names, dotfiles included
  ( cd "$1" && ls -A1 | LC_ALL=C sort | tr '\n' ' ' )
}

EXPECTED_PARENT_CHILDREN=".repo-z.setup.lock repo-z "

# The preflight's throwaway work_dir lives in setup's own scratch root. Counted
# before and after rather than asserted absent, because the interrupt tests
# above SIGKILL setup, which by construction skips its cleanup trap.
probe_dir_count() {
  find "$SCRATCH_DIR" -maxdepth 1 -name 'sandbox-probe.*' 2>/dev/null | wc -l
}
PROBE_DIRS_BEFORE="$(probe_dir_count)"

SETUP_CWD="$CONTAIN_CWD"
LAKE_LOG="$TEST_ROOT/lake-m1.log"; LAKE_KILL_MATCH=""
run_setup "$TEST_ROOT/setup-m1.log" --loogle off --yes \
  --mathlib-build-tar "$MATHLIB_TAR" "$CONTAIN_REPO" "$PAPER" ledger-contain
check_eq "clean setup into a fresh parent exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-m1.log"
check "the lake stub was the lake that ran" test -s "$TEST_ROOT/lake-m1.log"
check_eq "clean setup adds only the repo and its lock to the parent" \
  "$EXPECTED_PARENT_CHILDREN" "$(dir_children "$CONTAIN_PARENT")"
check_eq "clean setup leaves its own cwd empty" "" "$(dir_children "$CONTAIN_CWD")"

# The reported case: --reset against an existing repo.
LAKE_LOG="$TEST_ROOT/lake-m2.log"
run_setup "$TEST_ROOT/setup-m2.log" --loogle off --yes --reset \
  --mathlib-build-tar "$MATHLIB_TAR" "$CONTAIN_REPO" "$PAPER" ledger-contain
check_eq "--reset exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-m2.log"
check_eq "--reset adds nothing to the parent" \
  "$EXPECTED_PARENT_CHILDREN" "$(dir_children "$CONTAIN_PARENT")"
check_eq "--reset leaves its own cwd empty" "" "$(dir_children "$CONTAIN_CWD")"
# Named individually so a failure says which skeleton entry leaked rather than
# just diffing two directory listings.
for stray in Tablet reference .trellis .lake; do
  check_not "--reset leaves no stray $stray/ beside the repo" \
    test -e "$CONTAIN_PARENT/$stray"
done

# --resume took the same parent-directory branch.
LAKE_LOG="$TEST_ROOT/lake-m3.log"
run_setup "$TEST_ROOT/setup-m3.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$CONTAIN_REPO" "$PAPER" ledger-contain
check_eq "--resume exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-m3.log"
check_eq "--resume adds nothing to the parent" \
  "$EXPECTED_PARENT_CHILDREN" "$(dir_children "$CONTAIN_PARENT")"
check_eq "--resume leaves its own cwd empty" "" "$(dir_children "$CONTAIN_CWD")"
check_eq "completed setups clean up their preflight scratch" \
  "$PROBE_DIRS_BEFORE" "$(probe_dir_count)"
SETUP_CWD=""

# ===========================================================================
echo ""
echo "[14] --env-map and --main-result-envs: the viewer's settled env choices"
# ===========================================================================
# A paper whose main theorem lives in a title-less \newtheorem alias (invisible
# to auto-detection) plus a proposition. --env-map maps the alias onto theorem;
# --main-result-envs widens candidacy to include the proposition.
ALIAS_PAPER="$TEST_ROOT/alias-paper.tex"
cat > "$ALIAS_PAPER" <<'TEX'
\documentclass{article}
\newtheorem{mainthm}{}[section]
\begin{document}
\section{Intro}

\begin{mainthm}\label{thm:alias}
The aliased main theorem.
\end{mainthm}

\begin{proposition}\label{prop:main}
The proposition this run also targets.
\end{proposition}

\begin{theorem}\label{thm:plain}
A plain theorem.
\end{theorem}

\end{document}
TEX

REPO7="$TEST_ROOT/repo-g"
LAKE_LOG="$TEST_ROOT/lake-n1.log"; LAKE_KILL_MATCH=""
run_setup "$TEST_ROOT/setup-n1.log" --loogle off --yes \
  --env-map mainthm=theorem --main-result-envs theorem,proposition \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO7" "$ALIAS_PAPER" ledger-envs
check_eq "clean run with env flags exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-n1.log"
check_eq "config records the widened env set" "theorem,proposition" \
  "$(config_main_result_envs "$REPO7")"
check_eq "all three blocks resolved as targets" \
  "6:8:thm:alias,10:12:prop:main,14:16:thm:plain" "$(config_target_summary "$REPO7")"
check "the alias was normalized in the stored paper" \
  grep -q '\\begin{theorem}\\label{thm:alias}' "$REPO7/paper/alias-paper.tex"
check_not "no alias env survives in the stored paper" \
  grep -q 'begin{mainthm}' "$REPO7/paper/alias-paper.tex"
ENVS_BASELINE="$TEST_ROOT/baseline-envs-ls-files.txt"
git -C "$REPO7" ls-files -s > "$ENVS_BASELINE"

echo ""
echo "[14a] a resume that drops the env flags cannot silently narrow the run"
LAKE_LOG="$TEST_ROOT/lake-n2.log"
run_setup "$TEST_ROOT/setup-n2.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO7" "$ALIAS_PAPER" ledger-envs
check "resume without the env flags is refused" test "$SETUP_STATUS" != "0"
check "refusal names main_result_envs" grep -q "main_result_envs:" "$TEST_ROOT/setup-n2.log"
check "refusal names env_map" grep -q "env_map:" "$TEST_ROOT/setup-n2.log"
check_eq "config env set untouched by the refusal" "theorem,proposition" \
  "$(config_main_result_envs "$REPO7")"

LAKE_LOG="$TEST_ROOT/lake-n3.log"
run_setup "$TEST_ROOT/setup-n3.log" --loogle off --yes --resume \
  --env-map mainthm=theorem --main-result-envs theorem,proposition \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO7" "$ALIAS_PAPER" ledger-envs
check_eq "resume with the same flags exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-n3.log"
check "resumed tree == clean tree with env flags" trees_equal "$REPO7" "$ENVS_BASELINE"

echo ""
echo "[14b] a non-canonical --main-result-envs entry fails before any repo write"
REPO7B="$TEST_ROOT/repo-g-bad"
LAKE_LOG="$TEST_ROOT/lake-n4.log"
run_setup "$TEST_ROOT/setup-n4.log" --loogle off --yes \
  --main-result-envs theorem,conjecture \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO7B" "$ALIAS_PAPER" ledger-envs-bad
check "non-canonical env entry is refused" test "$SETUP_STATUS" != "0"
check "refusal names the canonical set" \
  grep -q "not a canonical TeX statement environment" "$TEST_ROOT/setup-n4.log"
check_not "no repo was created" test -e "$REPO7B"

echo ""
echo "[14c] a template carrying workflow.main_result_envs pins resolution too"
# The template's env set must be the one S2 resolves under — otherwise the
# preview the operator confirms and the config the run loads would disagree.
CUSTOM_TEMPLATE="$TEST_ROOT/custom.config.json"
python3 - "$SOURCE_ROOT/examples/trellis.config.json" "$CUSTOM_TEMPLATE" <<'PY'
import json
import sys

data = json.loads(open(sys.argv[1], encoding="utf-8").read())
data.setdefault("workflow", {})["main_result_envs"] = ["theorem"]
open(sys.argv[2], "w", encoding="utf-8").write(json.dumps(data, indent=2) + "\n")
PY
REPO7C="$TEST_ROOT/repo-g-template"
CONFIG_TEMPLATE_OVERRIDE="$CUSTOM_TEMPLATE"
LAKE_LOG="$TEST_ROOT/lake-n5.log"
run_setup "$TEST_ROOT/setup-n5.log" --loogle off --yes \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO7C" "$PAPER" ledger-envs-template
check_eq "template-pinned run exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-n5.log"
check_eq "config keeps the template env set" "theorem" \
  "$(config_main_result_envs "$REPO7C")"
check_eq "resolution ran under the template set (corollary excluded)" \
  "5:7:thm:main" "$(config_target_summary "$REPO7C")"
CONFIG_TEMPLATE_OVERRIDE=""

# ===========================================================================
echo ""
echo "[15] --targets-json: the viewer's confirmed selection is explicit mode"
# ===========================================================================
# A label selection plus an UNLABELED line-range selection — the case labels
# cannot express (design §5.6): the corollary at lines 9-11 is selected by its
# range while its label is deliberately not used.
SELECTION_JSON="$TEST_ROOT/selection.json"
cat > "$SELECTION_JSON" <<'JSON'
["thm:main", {"start_line": 9, "end_line": 11}]
JSON

REPO8="$TEST_ROOT/repo-h"
LAKE_LOG="$TEST_ROOT/lake-o1.log"; LAKE_KILL_MATCH=""
run_setup "$TEST_ROOT/setup-o1.log" --loogle off --yes \
  --targets-json "$SELECTION_JSON" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO8" "$PAPER" ledger-targets
check_eq "clean run with explicit targets exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-o1.log"
check_eq "config records exactly the selected targets, in order" \
  "5:7:thm:main,9:11:-" "$(config_target_summary "$REPO8")"
TARGETS_BASELINE="$TEST_ROOT/baseline-targets-ls-files.txt"
git -C "$REPO8" ls-files -s > "$TARGETS_BASELINE"

echo ""
echo "[15a] a resume that drops --targets-json cannot silently re-infer"
LAKE_LOG="$TEST_ROOT/lake-o2.log"
run_setup "$TEST_ROOT/setup-o2.log" --loogle off --yes --resume \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO8" "$PAPER" ledger-targets
check "resume without --targets-json is refused" test "$SETUP_STATUS" != "0"
check "refusal names targets_json" grep -q "targets_json:" "$TEST_ROOT/setup-o2.log"
check_eq "config targets untouched by the refusal" \
  "5:7:thm:main,9:11:-" "$(config_target_summary "$REPO8")"

LAKE_LOG="$TEST_ROOT/lake-o3.log"
run_setup "$TEST_ROOT/setup-o3.log" --loogle off --yes --resume \
  --targets-json "$SELECTION_JSON" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO8" "$PAPER" ledger-targets
check_eq "resume with the same selection exits 0" "0" "$SETUP_STATUS"
dump_on_failure "$TEST_ROOT/setup-o3.log"
check "resumed tree == clean tree with explicit targets" trees_equal "$REPO8" "$TARGETS_BASELINE"

echo ""
echo "[15b] the failure modes fail loudly, before any repo write"
REPO8B="$TEST_ROOT/repo-h-bad"
LAKE_LOG="$TEST_ROOT/lake-o4.log"
run_setup "$TEST_ROOT/setup-o4.log" --loogle off --yes \
  --targets-json "$SELECTION_JSON" --main-result-labels thm:main \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO8B" "$PAPER" ledger-targets-bad
check "--targets-json + --main-result-labels is refused" test "$SETUP_STATUS" != "0"
check "refusal explains the silent-drop hazard" \
  grep -q "contradictory" "$TEST_ROOT/setup-o4.log"
check_not "no repo was created (conflict)" test -e "$REPO8B"

STALE_JSON="$TEST_ROOT/stale-selection.json"
printf '["thm:not-in-this-paper"]\n' > "$STALE_JSON"
LAKE_LOG="$TEST_ROOT/lake-o5.log"
run_setup "$TEST_ROOT/setup-o5.log" --loogle off --yes \
  --targets-json "$STALE_JSON" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO8B" "$PAPER" ledger-targets-bad
check "a selection the paper cannot locate is refused" test "$SETUP_STATUS" != "0"
check "refusal comes from the kernel's match, naming the target" \
  grep -q "Could not locate paper text" "$TEST_ROOT/setup-o5.log"
check_not "no repo was created (stale selection)" test -e "$REPO8B"

MALFORMED_JSON="$TEST_ROOT/malformed-selection.json"
printf '[{"note": "no label, no lines"}]\n' > "$MALFORMED_JSON"
LAKE_LOG="$TEST_ROOT/lake-o6.log"
run_setup "$TEST_ROOT/setup-o6.log" --loogle off --yes \
  --targets-json "$MALFORMED_JSON" \
  --mathlib-build-tar "$MATHLIB_TAR" "$REPO8B" "$PAPER" ledger-targets-bad
check "an entry that is not a target is refused (kernel would drop it silently)" \
  test "$SETUP_STATUS" != "0"
check "refusal names the malformed entry" \
  grep -q "is not a target" "$TEST_ROOT/setup-o6.log"
check_not "no repo was created (malformed selection)" test -e "$REPO8B"

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

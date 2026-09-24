#!/usr/bin/env bash
# Two-phase create-job tests for scripts/trellis_create_run.sh.
#
# VIEWER_RUN_CREATION_DESIGN.md §9 asks for an env-stubbed bash test driving
# resolve -> confirm -> fail-at-setup -> retry. The stub trick is the same one
# tests/test_setup_stage_ledger.sh uses: the orchestrator's collaborators are
# replaced through the TRELLIS_CREATE_*_CMD env overrides it documents, so the
# state machine, the tmux process model, the heartbeat, the selection
# translation and the delete gate under test are all the real ones.
#
# WHAT IS REAL HERE: the whole of trellis_create_run.sh, tmux (on a PRIVATE
# socket — never the live `trellis` socket), and
# scripts/resolve_paper_targets.py in mirror-only mode (no kernel binary is
# resolvable from a worktree, and the script deliberately never `cargo run`s).
#
# WHAT IS STUBBED: setup_repo.sh, trellis.sh init, the checker server, the run
# supervisor, and trellis_pause.sh — each stub records its argv and produces
# exactly the artifacts the orchestrator's own probes read (the checker stub
# binds a REAL AF_UNIX socket, because the launch stage tests `-S`).
#
# The test root lives under ~/.cache with a SHORT name: the checker socket is
# an AF_UNIX path and the 108-byte cap is real.
#
# Usage: tests/test_create_run_job.sh
#        KEEP_TEST_ROOT=1 tests/test_create_run_job.sh   # keep artifacts
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CREATE="$SOURCE_ROOT/scripts/trellis_create_run.sh"

TEST_ROOT="$(mktemp -d "$HOME/.cache/tcr.XXXXXX")"
echo "test root: $TEST_ROOT"

# A private tmux server. Every session this test creates lives on this socket
# and dies with the kill-server in cleanup; the live `-L trellis` server is
# never touched.
export TRELLIS_TMUX_SOCKET="tcr-$$"
cleanup() {
  tmux -L "$TRELLIS_TMUX_SOCKET" kill-server 2>/dev/null
  if [ -z "${KEEP_TEST_ROOT:-}" ] && [ "$FAIL" -eq 0 ]; then
    rm -rf "$TEST_ROOT"
  else
    echo "artifacts kept at $TEST_ROOT"
  fi
}
trap cleanup EXIT

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "  ok   - $1"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL - $1"; }

check() { # check <description> <condition-cmd...>
  local desc="$1"
  shift
  if "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi
}

check_not() {
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

# wait_for_state <slug> <state> [<secs>] — poll the script's own status JSON.
wait_for_state() {
  local slug="$1" want="$2" secs="${3:-20}" i state
  for ((i = 0; i < secs * 5; i++)); do
    state="$(bash "$CREATE" status "$slug" 2>/dev/null \
      | python3 -c 'import json,sys
try:
    print(json.load(sys.stdin).get("state",""))
except Exception:
    print("")')"
    [ "$state" = "$want" ] && return 0
    sleep 0.2
  done
  echo "  (timed out waiting for state=$want; last state=$state)"
  return 1
}

status_field() { # status_field <slug> <field>
  bash "$CREATE" status "$1" 2>/dev/null \
    | python3 -c 'import json,sys
try:
    v = json.load(sys.stdin).get(sys.argv[1])
except Exception:
    v = None
print("" if v is None else v)' "$2"
}

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------
PROJECTS_ROOT="$TEST_ROOT/math"
mkdir -p "$PROJECTS_ROOT"
export TRELLIS_PROJECTS_ROOT="$PROJECTS_ROOT"

PAPER="$TEST_ROOT/paper.tex"
cat > "$PAPER" <<'TEX'
\documentclass{article}
\begin{document}
\section{Intro}
Our main result is Theorem~\ref{thm:main}.

\begin{theorem}\label{thm:main}
Every finite nonempty totally ordered set has a maximum.
\end{theorem}

\begin{corollary}
Every finite nonempty set of naturals has a maximum.
\end{corollary}

\end{document}
TEX

TEMPLATE="$SOURCE_ROOT/examples/trellis.config.json"

# ---------------------------------------------------------------------------
# Stubs. Each records its argv; the setup stub is failure-programmable via a
# flag file (fail once, then succeed — the retry path under test).
# ---------------------------------------------------------------------------
STUB_DIR="$TEST_ROOT/stubs"
mkdir -p "$STUB_DIR"

cat > "$STUB_DIR/setup_stub.sh" <<'SETUP'
#!/usr/bin/env bash
# Records argv; copies the --targets-json payload for assertions; creates the
# minimal repo shape phase B's later steps read (config + stage ledger).
set -euo pipefail
printf '%s\n' "$*" >> "$SETUP_LOG"
targets=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--targets-json" ]; then targets="$arg"; fi
  prev="$arg"
done
# setup_repo.sh's positional tail is <repo> <paper> [slug]; the orchestrator
# always passes all three, so the repo is the third-from-last argument.
repo="${@: -3:1}"
if [ -n "$targets" ]; then cp "$targets" "$SETUP_TARGETS_COPY"; fi
printf '%s\n' "${CONFIG_TEMPLATE:-}" >> "$SETUP_TEMPLATE_LOG"
# The ledger lands BEFORE the programmed failure: the real setup_repo.sh
# flushes stage records as it goes, so a mid-flight failure leaves one behind
# — which is exactly what makes the orchestrator's retry pass --reconfigure.
mkdir -p "$repo/.trellis"
printf '{"stages": {}}\n' > "$repo/.trellis/setup_stages.json"
if [ -f "$SETUP_FAIL_FLAG" ]; then
  rm -f "$SETUP_FAIL_FLAG"
  echo "setup stub: programmed failure" >&2
  exit 1
fi
# The real setup_repo.sh writes the run's config INTO the repo, merged from
# CONFIG_TEMPLATE. Phase B reads that file to decide whether to launch the
# closure sidecar, so the stub must reproduce that contract or the sidecar
# gate is untestable: copy the template through when there is one.
if [ -n "${CONFIG_TEMPLATE:-}" ] && [ -f "${CONFIG_TEMPLATE:-}" ]; then
  cp "$CONFIG_TEMPLATE" "$repo/trellis.config.json"
else
  printf '{"project": "stub"}\n' > "$repo/trellis.config.json"
fi
SETUP
chmod +x "$STUB_DIR/setup_stub.sh"

cat > "$STUB_DIR/init_stub.sh" <<'INIT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$INIT_LOG"
runtime="$2"
mkdir -p "$runtime"
printf '{"phase": "TheoremStating", "cycle": 0}\n' > "$runtime/protocol_state.json"
printf '{"repo_path": "stub"}\n' > "$runtime/runtime_metadata.json"
INIT
chmod +x "$STUB_DIR/init_stub.sh"

# Binds a REAL unix socket at <runtime>/sockets/checker.sock and holds it:
# the launch stage's readiness probe is `-S`, and a plain file must not pass.
cat > "$STUB_DIR/checker_stub.sh" <<'CHECKER'
#!/usr/bin/env bash
set -euo pipefail
runtime="$1"
mkdir -p "$runtime/sockets"
exec python3 - "$runtime/sockets/checker.sock" <<'PY'
import socket, sys, time
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(sys.argv[1])
server.listen(1)
time.sleep(300)
PY
CHECKER
chmod +x "$STUB_DIR/checker_stub.sh"

cat > "$STUB_DIR/run_stub.sh" <<'RUN'
#!/usr/bin/env bash
set -euo pipefail
printf 'run %s checker=%s\n' "$1" "${TRELLIS_CHECKER_SOCKET:-}" >> "$RUN_LOG"
sleep 300
RUN
chmod +x "$STUB_DIR/run_stub.sh"

cat > "$STUB_DIR/pause_stub.sh" <<'PAUSE'
#!/usr/bin/env bash
# `status <runtime> <repo>` — reports `running`, mirroring a healthy launch.
printf '{"state": "running"}\n'
PAUSE
chmod +x "$STUB_DIR/pause_stub.sh"

export SETUP_LOG="$TEST_ROOT/setup.log"
export SETUP_TARGETS_COPY="$TEST_ROOT/targets-received.json"
export SETUP_TEMPLATE_LOG="$TEST_ROOT/template.log"
export SETUP_FAIL_FLAG="$TEST_ROOT/setup-fail-once"
export INIT_LOG="$TEST_ROOT/init.log"
export RUN_LOG="$TEST_ROOT/run.log"
export SIDECAR_LOG="$TEST_ROOT/sidecar.log"

export TRELLIS_CREATE_SETUP_CMD="bash $STUB_DIR/setup_stub.sh"
export TRELLIS_CREATE_INIT_CMD="bash $STUB_DIR/init_stub.sh"
cat > "$STUB_DIR/sidecar_stub.sh" <<'SIDECAR'
#!/usr/bin/env bash
# Records that phase B launched the closure sidecar, and with what.
printf 'sidecar %s repo=%s\n' "$1" "${3:-}" >> "$SIDECAR_LOG"
sleep 3600
SIDECAR
chmod +x "$STUB_DIR/sidecar_stub.sh"

export TRELLIS_CREATE_CHECKER_CMD="bash $STUB_DIR/checker_stub.sh"
export TRELLIS_CREATE_RUN_CMD="bash $STUB_DIR/run_stub.sh"
export TRELLIS_CREATE_SIDECAR_CMD="bash $STUB_DIR/sidecar_stub.sh"
export TRELLIS_CREATE_PAUSE_CMD="bash $STUB_DIR/pause_stub.sh"
export TRELLIS_CREATE_HEARTBEAT_SECS=1
export TRELLIS_CREATE_STALE_SECS=2
export TRELLIS_CREATE_SOCKET_WAIT_SECS=10
export TRELLIS_CREATE_VERIFY_SECS=10
# No kernel binary must ever be reached for: mirror-only resolve is the
# worktree-deterministic path, and phase B's kernel work is stubbed anyway.
unset TRELLIS_TRELLIS_KERNEL_CMD

JOBS="$PROJECTS_ROOT/.trellis-viewer/create-jobs"

# ===========================================================================
echo ""
echo "[1] start -> phase A -> awaiting_targets with NO process (the park)"
# ===========================================================================
bash "$CREATE" start alpha --paper "$PAPER" --loogle off --template "$TEMPLATE" \
  > "$TEST_ROOT/start-alpha.log" 2>&1
check_eq "start exits 0" "0" "$?"
check "job dir claimed" test -d "$JOBS/alpha"
check "job.json written" test -f "$JOBS/alpha/job.json"
check "paper ingested into the job dir" test -f "$JOBS/alpha/paper.tex"

check "phase A reaches awaiting_targets" wait_for_state alpha awaiting_targets 30
check "targets_resolution.json written" test -f "$JOBS/alpha/targets_resolution.json"
# The §4 divergence: the park is process-DOWN. The phase session must be gone.
sleep 1
check_eq "no tmux session at the park" "False" "$(status_field alpha tmux_alive)"
check_eq "awaiting_targets is the effective state too" "awaiting_targets" \
  "$(status_field alpha effective_state)"
check "create.log captured the phase" test -s "$JOBS/alpha/create.log"

resolution_keys() {
  python3 -c 'import json,sys
data = json.load(open(sys.argv[1]))
print(",".join(c["key"] for c in data.get("candidates", [])))' \
    "$JOBS/alpha/targets_resolution.json"
}
KEYS="$(resolution_keys)"
check_eq "resolution found the labeled theorem and the unlabeled corollary" \
  "thm:main,lines:10-12" "$KEYS"
check "labeled candidate is preselected by ranking" \
  python3 -c 'import json,sys
data = json.load(open(sys.argv[1]))
c = {x["key"]: x for x in data["candidates"]}
assert c["thm:main"]["preselected"] is True
assert c["thm:main"]["first_class"] is True
assert c["lines:10-12"]["first_class"] is False' "$JOBS/alpha/targets_resolution.json"

echo ""
echo "[1b] the slug claim is atomic: a second start (and a taken repo) refuse"
bash "$CREATE" start alpha --paper "$PAPER" --loogle off \
  > "$TEST_ROOT/start-alpha2.log" 2>&1
check_eq "second start for the same slug exits 2" "2" "$?"
check "refusal names the existing job dir" grep -q "already exists" "$TEST_ROOT/start-alpha2.log"
mkdir -p "$PROJECTS_ROOT/taken"
bash "$CREATE" start taken --paper "$PAPER" --loogle off \
  > "$TEST_ROOT/start-taken.log" 2>&1
check_eq "a slug whose repo path exists refuses" "2" "$?"
check "refusal names the project path" grep -q "already exists at" "$TEST_ROOT/start-taken.log"
bash "$CREATE" start 'bad..slug' --paper "$PAPER" --loogle off \
  > "$TEST_ROOT/start-bad.log" 2>&1
check_not "a dotted slug is rejected outright" test "$?" = "0"

echo ""
echo "[2] confirm gates the build: bad keys refused, no --yes-blind path"
bash "$CREATE" confirm alpha > "$TEST_ROOT/confirm-none.log" 2>&1
check_not "confirm with no selection refuses" test "$?" = "0"
check "refusal explains at least one target is required" \
  grep -q "at least one" "$TEST_ROOT/confirm-none.log"
bash "$CREATE" confirm alpha --select thm:nonexistent > "$TEST_ROOT/confirm-bad.log" 2>&1
check_not "confirm with an unknown key refuses" test "$?" = "0"
check "refusal names the unknown key" grep -q "thm:nonexistent" "$TEST_ROOT/confirm-bad.log"

echo ""
echo "[3] confirm -> phase B fails at setup -> build_failed; retry recovers"
touch "$SETUP_FAIL_FLAG"
bash "$CREATE" confirm alpha --select thm:main --select lines:10-12 \
  > "$TEST_ROOT/confirm-alpha.log" 2>&1
check_eq "confirm exits 0" "0" "$?"
check "phase B records the setup failure" wait_for_state alpha build_failed 30
check "the repo bears the creating marker after a failed build" \
  test -e "$PROJECTS_ROOT/alpha/.trellis-creating"
check "error recorded in status.json" \
  grep -q "setup_repo.sh failed" "$JOBS/alpha/status.json"

# The selection reached setup TRANSLATED: label key as a label string, the
# unlabeled `lines:` key as a start/end object — never as a `lines:...` label.
check "setup received --targets-json" test -f "$SETUP_TARGETS_COPY"
check_eq "candidate keys were translated to raw_targets wire shape" \
  'ok' "$(python3 -c 'import json,sys
data = json.load(open(sys.argv[1]))
assert data == ["thm:main", {"start_line": 10, "end_line": 12}], data
print("ok")' "$SETUP_TARGETS_COPY")"
check "setup ran under the job template (CONFIG_TEMPLATE)" \
  grep -q "trellis.config.json" "$SETUP_TEMPLATE_LOG"
check "setup was driven --yes --resume (resumable, non-interactive)" \
  grep -q -- "--yes --resume" "$SETUP_LOG"

bash "$CREATE" retry alpha > "$TEST_ROOT/retry-alpha.log" 2>&1
check_eq "retry exits 0" "0" "$?"
check "retry reaches done" wait_for_state alpha done 40
check "second setup invocation passed --reconfigure (ledger existed)" \
  grep -q -- "--reconfigure" "$SETUP_LOG"
check "init stub ran against the runtime root" grep -q "alpha-runtime" "$INIT_LOG"
check "run session was launched with the checker socket" \
  grep -q "checker=$PROJECTS_ROOT/alpha-runtime/sockets/checker.sock" "$RUN_LOG"
check "creating marker removed as the last act of launch" \
  test ! -e "$PROJECTS_ROOT/alpha/.trellis-creating"
check "current symlink adopted (nothing held it)" \
  test -L "$PROJECTS_ROOT/current"
check_eq "current points at the new repo" "$PROJECTS_ROOT/alpha" \
  "$(readlink "$PROJECTS_ROOT/current")"
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-run-alpha 2>/dev/null
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-checker-alpha 2>/dev/null

echo ""
echo "[3b] done is terminal: retry, resolve and delete all refuse"
bash "$CREATE" retry alpha > "$TEST_ROOT/retry-done.log" 2>&1
check_not "retry after done refuses" test "$?" = "0"
bash "$CREATE" resolve alpha > "$TEST_ROOT/resolve-done.log" 2>&1
check_not "resolve after done refuses" test "$?" = "0"
bash "$CREATE" delete alpha > "$TEST_ROOT/delete-done.log" 2>&1
check_not "delete after done refuses (the repo is a run now)" test "$?" = "0"
check "delete refusal says why" grep -q "completed" "$TEST_ROOT/delete-done.log"

# ===========================================================================
echo ""
echo "[4] re-resolve clears the selection and re-runs phase A"
# ===========================================================================
bash "$CREATE" start beta --paper "$PAPER" --loogle off > "$TEST_ROOT/start-beta.log" 2>&1
check "beta reaches awaiting_targets" wait_for_state beta awaiting_targets 30
bash "$CREATE" resolve beta --main-result-envs theorem > "$TEST_ROOT/resolve-beta.log" 2>&1
check_eq "resolve exits 0" "0" "$?"
check "beta re-parks at awaiting_targets" wait_for_state beta awaiting_targets 30
check_eq "narrowed env set drops the corollary candidate" "thm:main" \
  "$(python3 -c 'import json,sys
data = json.load(open(sys.argv[1]))
print(",".join(c["key"] for c in data.get("candidates", [])))' \
    "$JOBS/beta/targets_resolution.json")"
check_eq "the env choice is recorded in job.json" "theorem" \
  "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("main_result_envs"))' \
    "$JOBS/beta/job.json")"

# ===========================================================================
echo ""
echo "[5] interrupted: stale heartbeat + dead session, then retry recovers"
# ===========================================================================
# A setup stub that blocks forever makes phase B park mid-`building`; killing
# the tmux session is the closest reachable analogue of reboot/OOM (the same
# stance test_setup_stage_ledger.sh takes with its kill trick).
cat > "$STUB_DIR/setup_hang.sh" <<'HANG'
#!/usr/bin/env bash
sleep 300
HANG
chmod +x "$STUB_DIR/setup_hang.sh"
export TRELLIS_CREATE_SETUP_CMD="bash $STUB_DIR/setup_hang.sh"
bash "$CREATE" confirm beta --select thm:main > "$TEST_ROOT/confirm-beta.log" 2>&1
check_eq "confirm exits 0" "0" "$?"
check "beta reaches building" wait_for_state beta building 30
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-create-beta 2>/dev/null
sleep 3  # > TRELLIS_CREATE_STALE_SECS: the heartbeat is dead and must age out
check_eq "state stays building on disk" "building" "$(status_field beta state)"
check_eq "effective state decays to interrupted" "interrupted" \
  "$(status_field beta effective_state)"

export TRELLIS_CREATE_SETUP_CMD="bash $STUB_DIR/setup_stub.sh"
bash "$CREATE" retry beta > "$TEST_ROOT/retry-beta.log" 2>&1
check_eq "retry of an interrupted build exits 0" "0" "$?"
check "interrupted build retries to done" wait_for_state beta done 40
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-run-beta 2>/dev/null
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-checker-beta 2>/dev/null

# ===========================================================================
echo ""
echo "[6] delete: confirmation must name every doomed path, exactly"
# ===========================================================================
touch "$SETUP_FAIL_FLAG"
bash "$CREATE" start gamma --paper "$PAPER" --loogle off > "$TEST_ROOT/start-gamma.log" 2>&1
check "gamma reaches awaiting_targets" wait_for_state gamma awaiting_targets 30
bash "$CREATE" confirm gamma --select thm:main > "$TEST_ROOT/confirm-gamma.log" 2>&1
check "gamma fails its build" wait_for_state gamma build_failed 30

bash "$CREATE" delete gamma > "$TEST_ROOT/delete-gamma-1.log" 2>&1
check_eq "delete without confirmation exits 3" "3" "$?"
check "the refusal names the job dir" grep -q "create-jobs/gamma" "$TEST_ROOT/delete-gamma-1.log"
check "the refusal names the repo" grep -q "\"$PROJECTS_ROOT/gamma\"" "$TEST_ROOT/delete-gamma-1.log"
check "nothing was deleted" test -d "$JOBS/gamma"

bash "$CREATE" delete gamma --confirm "$JOBS/gamma" > "$TEST_ROOT/delete-gamma-2.log" 2>&1
check_eq "a partial confirmation still exits 3" "3" "$?"
check "still nothing deleted" test -e "$PROJECTS_ROOT/gamma/.trellis-creating"

bash "$CREATE" delete gamma --confirm "$JOBS/gamma" --confirm "$PROJECTS_ROOT/gamma" \
  > "$TEST_ROOT/delete-gamma-3.log" 2>&1
check_eq "the full confirmation deletes" "0" "$?"
check_not "job dir removed" test -e "$JOBS/gamma"
check_not "repo removed" test -e "$PROJECTS_ROOT/gamma"

echo ""
echo "[6b] delete never touches a repo without the creating marker"
bash "$CREATE" start delta --paper "$PAPER" --loogle off > "$TEST_ROOT/start-delta.log" 2>&1
check "delta reaches awaiting_targets" wait_for_state delta awaiting_targets 30
# Simulate a repo that exists WITHOUT this job's marker (a graduated or
# foreign repo that happens to share the slug).
mkdir -p "$PROJECTS_ROOT/delta"
bash "$CREATE" delete delta --confirm "$JOBS/delta" --confirm "$PROJECTS_ROOT/delta" \
  > "$TEST_ROOT/delete-delta.log" 2>&1
check_not "delete refuses a marker-less repo" test "$?" = "0"
check "refusal names the marker rule" grep -q "trellis-creating" "$TEST_ROOT/delete-delta.log"
check "the repo survived" test -d "$PROJECTS_ROOT/delta"

# ===========================================================================
echo ""
echo "[7] resolve_stale is its own state, with re-resolve as its recovery"
# ===========================================================================
# Route 1 (translate): the selection names candidates the CURRENT resolution
# no longer holds — the on-disk shape a re-resolve of a changed paper leaves
# behind. Provoked by replacing targets_resolution.json between the failed
# build and the retry, which is exactly what phase A does under a new paper.
touch "$SETUP_FAIL_FLAG"
bash "$CREATE" start eps --paper "$PAPER" --loogle off > "$TEST_ROOT/start-eps.log" 2>&1
check "eps reaches awaiting_targets" wait_for_state eps awaiting_targets 30
bash "$CREATE" confirm eps --select thm:main > "$TEST_ROOT/confirm-eps.log" 2>&1
check "eps fails its first build" wait_for_state eps build_failed 30
python3 - "$JOBS/eps/targets_resolution.json" <<'PY'
import json, sys
path = sys.argv[1]
data = json.load(open(path))
data["candidates"] = [c for c in data["candidates"] if c.get("key") != "thm:main"]
json.dump(data, open(path, "w"))
PY
bash "$CREATE" retry eps > "$TEST_ROOT/retry-eps.log" 2>&1
check "eps lands in resolve_stale, not build_failed" wait_for_state eps resolve_stale 30
check "the error names re-resolve and re-confirm" \
  grep -q "re-resolve and re-confirm" "$JOBS/eps/status.json"
check "create.log names the missing keys" grep -q "thm:main" "$JOBS/eps/create.log"

# Retry from resolve_stale IS a re-resolve: phase A, selection cleared, park.
bash "$CREATE" retry eps > "$TEST_ROOT/retry-eps-2.log" 2>&1
check_eq "retry from resolve_stale exits 0" "0" "$?"
check "retry re-parks at awaiting_targets" wait_for_state eps awaiting_targets 30
check_eq "the stale selection was cleared" "" "$(status_field eps selected)"
check "the previous resolution was kept for diffing" \
  test -f "$JOBS/eps/targets_resolution.prev.json"
rm -f "$SETUP_FAIL_FLAG"
bash "$CREATE" confirm eps --select thm:main > "$TEST_ROOT/confirm-eps-2.log" 2>&1
check "a fresh confirm builds to done" wait_for_state eps done 40
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-run-eps 2>/dev/null
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-checker-eps 2>/dev/null

echo ""
echo "[7b] setup failing at match_block routes to resolve_stale too (§5.6)"
# Route 2: setup's own kernel re-resolve cannot locate the confirmed text —
# the stub prints the kernel's exact signature and fails.
cat > "$STUB_DIR/setup_stale.sh" <<'STALE'
#!/usr/bin/env bash
echo "ERROR: Could not locate paper text for resolved main-result target thm:main."
exit 1
STALE
chmod +x "$STUB_DIR/setup_stale.sh"
bash "$CREATE" start zeta --paper "$PAPER" --loogle off > "$TEST_ROOT/start-zeta.log" 2>&1
check "zeta reaches awaiting_targets" wait_for_state zeta awaiting_targets 30
TRELLIS_CREATE_SETUP_CMD="bash $STUB_DIR/setup_stale.sh" \
  bash "$CREATE" confirm zeta --select thm:main > "$TEST_ROOT/confirm-zeta.log" 2>&1
check "the match_block signature lands in resolve_stale" wait_for_state zeta resolve_stale 30
check "the error names the changed paper" \
  grep -q "paper changed between confirm and build" "$JOBS/zeta/status.json"

echo ""
echo "[8] create.log rotates between phases at the size cap, keeping last"
# ===========================================================================
python3 -c 'open(__import__("sys").argv[1], "a").write("x" * 5000 + "\n")' \
  "$JOBS/zeta/create.log"
TRELLIS_CREATE_LOG_ROTATE_BYTES=1000 \
  bash "$CREATE" resolve zeta > "$TEST_ROOT/resolve-zeta.log" 2>&1
check_eq "resolve (with rotation due) exits 0" "0" "$?"
check "previous log kept at create.log.1" test -s "$JOBS/zeta/create.log.1"
check "fresh log opens with the rotation notice" \
  grep -q "create.log rotated" "$JOBS/zeta/create.log"
check "zeta re-parks after the rotated resolve" wait_for_state zeta awaiting_targets 30

# ===========================================================================
echo ""
echo "[9] references: ingest validation, replacement at resolve, clearing"
# ===========================================================================
REF_A="$TEST_ROOT/refa.tex"
REF_B="$TEST_ROOT/refb.tex"
printf '\\begin{theorem}A\\end{theorem}\n' > "$REF_A"
printf '\\begin{theorem}B\\end{theorem}\n' > "$REF_B"

bash "$CREATE" start dup --paper "$PAPER" --loogle off \
  --reference "one=$REF_A" --reference "one=$REF_B" > "$TEST_ROOT/start-dup.log" 2>&1
check_not "duplicate reference ids refuse at start" test "$?" = "0"
check "the refusal names the duplicate id" grep -q "duplicate" "$TEST_ROOT/start-dup.log"
check_not "no job dir was left behind" test -d "$JOBS/dup"

bash "$CREATE" start eta --paper "$PAPER" --loogle off \
  --reference "refa=$REF_A:src-a" > "$TEST_ROOT/start-eta.log" 2>&1
check_eq "start with a reference exits 0" "0" "$?"
check "eta reaches awaiting_targets" wait_for_state eta awaiting_targets 30
check "the reference was ingested into the job dir" test -f "$JOBS/eta/refs/refa.tex"
check "job.json records the rewritten spec" \
  grep -q "refa=$JOBS/eta/refs/refa.tex:src-a" "$JOBS/eta/job.json"
check "status surfaces the references" \
  python3 -c 'import json,subprocess,sys
out = subprocess.run(["bash", sys.argv[1], "status", "eta"],
                     capture_output=True, text=True).stdout
refs = json.loads(out).get("references")
assert refs == [sys.argv[2]], refs' "$CREATE" "refa=$JOBS/eta/refs/refa.tex:src-a"

# Replacement is wholesale: the new set is what phase B will pass to setup,
# and stale ingested files are pruned (row 26: a changed file needs a NEW id).
bash "$CREATE" resolve eta --reference "refb=$REF_B" > "$TEST_ROOT/resolve-eta.log" 2>&1
check_eq "resolve with a replacement reference exits 0" "0" "$?"
check "eta re-parks" wait_for_state eta awaiting_targets 30
check "the new reference was ingested" test -f "$JOBS/eta/refs/refb.tex"
check_not "the replaced reference was pruned" test -e "$JOBS/eta/refs/refa.tex"
check "job.json now records only the new spec" \
  python3 -c 'import json,sys
job = json.load(open(sys.argv[1]))
assert [r.split("=")[0] for r in job["references"]] == ["refb"], job["references"]' \
  "$JOBS/eta/job.json"

bash "$CREATE" resolve eta --clear-references > "$TEST_ROOT/clear-eta.log" 2>&1
check_eq "clear-references exits 0" "0" "$?"
check "eta re-parks after clearing" wait_for_state eta awaiting_targets 30
check_eq "references cleared in job.json" "[]" "$(status_field eta references)"
check_not "no ingested refs remain" test -e "$JOBS/eta/refs/refb.tex"
bash "$CREATE" resolve eta --clear-references --reference "refa=$REF_A" \
  > "$TEST_ROOT/conflict-eta.log" 2>&1
check_not "clear + replace together refuse" test "$?" = "0"

# The confirmed build passes the CURRENT reference set through to setup.
bash "$CREATE" resolve eta --reference "refb=$REF_B:src-b" > /dev/null 2>&1
check "eta re-parks before confirm" wait_for_state eta awaiting_targets 30
: > "$SETUP_LOG"
bash "$CREATE" confirm eta --select thm:main > "$TEST_ROOT/confirm-eta.log" 2>&1
check "eta builds to done" wait_for_state eta done 40
check "setup received the replacement --reference spec" \
  grep -q -- "--reference refb=$JOBS/eta/refs/refb.tex:src-b" "$SETUP_LOG"
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-run-eta 2>/dev/null
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-checker-eta 2>/dev/null

# ===========================================================================
echo ""
echo "[10] disk preflight: the low-space warning lands in the build log"
# ===========================================================================
bash "$CREATE" start iota --paper "$PAPER" --loogle off > "$TEST_ROOT/start-iota.log" 2>&1
check "iota reaches awaiting_targets" wait_for_state iota awaiting_targets 30
# An absurd threshold makes any real filesystem "low"; the phase env snapshot
# carries the caller's TRELLIS_CREATE_* values into the tmux pane.
TRELLIS_CREATE_DISK_WARN_GB=1000000 \
  bash "$CREATE" confirm iota --select thm:main > "$TEST_ROOT/confirm-iota.log" 2>&1
check "iota builds to done despite the warning (warn, never block)" \
  wait_for_state iota done 40
check "the ENOSPC-preflight warning is in create.log" \
  grep -q "WARNING — only .* GB free" "$JOBS/iota/create.log"
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-run-iota 2>/dev/null
tmux -L "$TRELLIS_TMUX_SOCKET" kill-session -t trellis-checker-iota 2>/dev/null

# ===========================================================================
echo "[13] template overrides drive phase B end to end"
TEMPLATE_SHA_BEFORE="$(sha256sum "$TEMPLATE" | cut -d' ' -f1)"
# Regression: the override branch in phase B once called an undefined `log`
# helper. Under the script's own `set -euo pipefail` that killed phase B the
# instant an override was present — and the whole suite stayed green, because
# nothing here had ever driven phase B WITH an override set. So this section
# exercises the branch itself, not just the validator.
bash "$CREATE" start ovr --paper "$PAPER" --loogle off --template "$TEMPLATE" \
  --role-model worker=gpt-5.6-luna --role-effort worker=high \
  --role-model easy_worker=gpt-5.6-terra --role-effort easy_worker=xhigh \
  --role-model hard_worker=gpt-5.6-sol --role-effort hard_worker=xhigh \
  --role-model reviewer=gpt-5.6-luna --role-effort reviewer=high \
  --role-model verification.correspondence_agents=gpt-5.6-terra \
  --role-effort verification.correspondence_agents=high \
  --role-model verification.soundness_agents=gpt-5.6-sol \
  --role-effort verification.soundness_agents=xhigh \
  --role-model verification.substantiveness_agents=gpt-5.6-luna \
  --role-effort verification.substantiveness_agents=xhigh \
  --grunts 3 \
  --remote-url git@github.com:wpegden/ovr_trellis.git \
  > "$TEST_ROOT/start-ovr.log" 2>&1
check_eq "start with overrides exits 0" "0" "$?"
check "phase A reaches awaiting_targets" wait_for_state ovr awaiting_targets 30

rm -f "$SETUP_FAIL_FLAG"
bash "$CREATE" confirm ovr --select thm:main > "$TEST_ROOT/confirm-ovr.log" 2>&1
check_eq "confirm with overrides exits 0" "0" "$?"
check "phase B runs to done with overrides set" wait_for_state ovr done 40

# The direct regression guard: an undefined command in the phase body prints
# "command not found" into the pane log and aborts. Neither may happen.
check_not "no interpreter error in the job log" \
  grep -q "command not found" "$JOBS/ovr/create.log"

# setup must have been handed the JOB-LOCAL derived copy, never the shared
# examples/ template — two concurrent creations must not edit each other's.
check "setup ran under the job-local derived template" \
  grep -q "$JOBS/ovr/config-template.json" "$SETUP_TEMPLATE_LOG"
# Compare CONTENT, not git cleanliness: `git status` conflates "this run
# modified the template" with "the operator has an unrelated uncommitted
# edit to it", so the git form false-failed the moment the shipped default
# was changed in the working tree.
check_eq "the shared template file was not modified" \
  "$TEMPLATE_SHA_BEFORE" "$(sha256sum "$TEMPLATE" | cut -d' ' -f1)"

check_eq "overrides landed in the derived config" 'ok' "$(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
expected = {
    "worker": ("gpt-5.6-luna", "high"),
    "easy_worker": ("gpt-5.6-terra", "xhigh"),
    "hard_worker": ("gpt-5.6-sol", "xhigh"),
    "reviewer": ("gpt-5.6-luna", "high"),
}
for lane, (model, effort) in expected.items():
    assert d[lane]["model"] == model, (lane, d[lane])
    assert d[lane]["effort"] == effort, (lane, d[lane])
verification_expected = {
    "correspondence_agents": ("gpt-5.6-terra", "high"),
    "soundness_agents": ("gpt-5.6-sol", "xhigh"),
    "substantiveness_agents": ("gpt-5.6-luna", "xhigh"),
}
for pool, (model, effort) in verification_expected.items():
    for agent in d["verification"][pool]:
        assert agent["model"] == model, (pool, agent)
        assert agent["effort"] == effort, (pool, agent)
# Assert what the LOADER RESOLVES, never the JSON we wrote. The daemon
# reads grunts/wall from sub-blocks; written flat they are silently
# ignored and the run takes defaults. A test that checked the written
# keys passed for weeks while the values had no effect.
import importlib.util, sys as _s
_s.path.insert(0, "${TRELLIS_ROOT:-/path/to/trellis}/src/trellis")
from trellis.sidecar.config import SidecarConfig
from pathlib import Path as _P
_c = SidecarConfig.load(_P(sys.argv[1]))
assert _c.enabled is True, _c
# The durable job record keeps the complete independent selection map so a
# phase-B retry re-derives the same template without the browser.
job = json.load(open(sys.argv[1].replace("config-template.json", "job.json")))
assert set(job["role_overrides"]) == {
    "worker", "easy_worker", "hard_worker", "reviewer",
    "verification.correspondence_agents",
    "verification.soundness_agents",
    "verification.substantiveness_agents",
}, job["role_overrides"]
# 3, deliberately NOT the default of 2: a flat-key regression resolves
# to the default, and asserting the default cannot detect that.
assert _c.grunts == 3, f"resolved grunts={_c.grunts}, wanted 3"
assert _c.attempt_wall_seconds > 0, _c
assert d["git"]["remote_url"] == "git@github.com:wpegden/ovr_trellis.git", d["git"]
print("ok")' "$JOBS/ovr/config-template.json" 2>&1)"

# Absent overrides must still mean "inherit": a job with none must not get a
# derived copy at all.
bash "$CREATE" start noovr --paper "$PAPER" --loogle off --template "$TEMPLATE" \
  > "$TEST_ROOT/start-noovr.log" 2>&1
check_eq "start without overrides exits 0" "0" "$?"
check "phase A reaches awaiting_targets" wait_for_state noovr awaiting_targets 30
bash "$CREATE" confirm noovr --select thm:main > "$TEST_ROOT/confirm-noovr.log" 2>&1
check "phase B runs to done without overrides" wait_for_state noovr done 40
check "no derived template when nothing was overridden" \
  test ! -e "$JOBS/noovr/config-template.json"

# ===========================================================================
echo "[14] the closure sidecar is launched when the config enables it"
# Regression: --grunts wrote sidecar.enabled=true into the config and NOTHING
# started the daemon, so the kernel queued eligible nodes and no worker ever
# claimed them. The viewer's grunts page showed a growing queue, nothing in
# flight, and no attempts — a toggle that silently did nothing. A user
# starting a run from the browser must get working grunts, not a config flag.
: > "$SIDECAR_LOG"
bash "$CREATE" start grunty --paper "$PAPER" --loogle off --template "$TEMPLATE" \
  --grunts 2 > "$TEST_ROOT/start-grunty.log" 2>&1
check_eq "start with --grunts exits 0" "0" "$?"
check "phase A reaches awaiting_targets" wait_for_state grunty awaiting_targets 30
rm -f "$SETUP_FAIL_FLAG"
bash "$CREATE" confirm grunty --select thm:main > "$TEST_ROOT/confirm-grunty.log" 2>&1
check "phase B reaches done" wait_for_state grunty done 40
check "the sidecar daemon was launched" test -s "$SIDECAR_LOG"
check "it was pointed at this run's runtime root" \
  grep -q "$PROJECTS_ROOT/grunty-runtime" "$SIDECAR_LOG"
check "it was given the live repo" grep -q "repo=$PROJECTS_ROOT/grunty" "$SIDECAR_LOG"

# ...and NOT launched when the config leaves it disabled, so a run that never
# asked for grunts pays nothing.
: > "$SIDECAR_LOG"
bash "$CREATE" start nogrunt --paper "$PAPER" --loogle off --template "$TEMPLATE" \
  > "$TEST_ROOT/start-nogrunt.log" 2>&1
check "phase A reaches awaiting_targets" wait_for_state nogrunt awaiting_targets 30
bash "$CREATE" confirm nogrunt --select thm:main > "$TEST_ROOT/confirm-nogrunt.log" 2>&1
check "phase B reaches done" wait_for_state nogrunt done 40
check "no sidecar launched when the config disables it" test ! -s "$SIDECAR_LOG"

# ===========================================================================
echo "[15] the grunt wall is settable from the create flow"
: > "$SIDECAR_LOG"
bash "$CREATE" start walled --paper "$PAPER" --loogle off --template "$TEMPLATE" \
  --grunts 2 --grunt-wall 1800 > "$TEST_ROOT/start-walled.log" 2>&1
check_eq "start with --grunt-wall exits 0" "0" "$?"
check "phase A reaches awaiting_targets" wait_for_state walled awaiting_targets 30
rm -f "$SETUP_FAIL_FLAG"
bash "$CREATE" confirm walled --select thm:main > "$TEST_ROOT/confirm-walled.log" 2>&1
check "phase B reaches done" wait_for_state walled done 40
check_eq "the wall landed in the derived config" 'ok' "$(python3 -c '
import json, sys
import sys as _s
_s.path.insert(0, "${TRELLIS_ROOT:-/path/to/trellis}/src/trellis")
from trellis.sidecar.config import SidecarConfig
from pathlib import Path as _P
_c = SidecarConfig.load(_P(sys.argv[1]))
assert _c.attempt_wall_seconds == 1800, f"resolved wall={_c.attempt_wall_seconds}, wanted 1800"
print("ok")' "$JOBS/walled/config-template.json" 2>&1)"

# Out-of-range walls are refused at claim time, not at build time.
bash "$CREATE" start walled2 --paper "$PAPER" --loogle off --grunt-wall 30 \
  > "$TEST_ROOT/start-walled2.log" 2>&1
check_not "a wall below the floor is refused" test "$?" = "0"
check "the refusal names the range" grep -q "300-21600" "$TEST_ROOT/start-walled2.log"
check "no job dir was claimed for the refused slug" test ! -d "$JOBS/walled2"

# ===========================================================================
echo ""
echo "passed: $PASS   failed: $FAIL"
if [ "$FAIL" -ne 0 ]; then
  exit 1
fi
exit 0

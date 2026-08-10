# trellis

`trellis` is a formalization harness for guiding agents from a natural-language mathematical proof to a verified Lean development.

> ⚠️ **Security.** Trellis runs LLM agent CLIs fully autonomously with all approval prompts disabled (YOLO mode). The `bwrap` sandbox limits filesystem reach but is **not** a hard security boundary — agents run as you, with network access and read access to your provider credentials. **Only run Trellis on a dedicated machine with no private or valuable data.** See [SECURITY.md](SECURITY.md).

The core problem is not just generating Lean code. It is managing a long, error-prone, stateful process in which agents propose local proof steps, add intermediate claims, revise structure, and respond to verification feedback without losing semantic coherence. In this setting, failures are expensive: they are often discovered late, after multiple slow agent calls and several layers of context have already drifted.

## Goals

- guide agents through a multi-cycle formalization process controlled by deterministic checks instead of treating formalization as one giant prompt
- maintain explicit state about what part of the proof graph exists, what has been checked, and what still needs work
- separate proof generation from verification and scheduling so that agent outputs are judged by stable rules rather than prompt drift
- make the protocol precise enough to model in TLA+, implement in Rust, and operationalize with a thin Python wrapper
- reduce expensive downstream failures by making acceptance gates deterministic and inspectable

## Method

Trellis works over a proof tablet: a DAG of formalization nodes, each with a Lean artifact and a natural-language counterpart. A node may be a theorem-like statement or a definition. Dependencies are explicit. Verification status is explicit. Active work is explicit.

The system runs in cycles with distinct roles:

- a worker proposes edits or new structure within an authorized scope
- paper-faithfulness, substantiveness, correspondence, and NL soundness are the four agent-verified checks, evaluated in that order against deterministic gates
- a reviewer decides how to advance based on current blockers and verification state
- the runtime persists the result and schedules the next action

This is intentionally stricter than a free-form agent workflow. The aim is not to maximize agent freedom. The aim is to make the formalization process reliable enough that a long-running proof effort does not accumulate hidden semantic drift.

## Design Principles

### Protocol Before Prompts

The formalization workflow is treated as a protocol, not a pile of agent prompts. The authoritative rules for request issuance, blocker handling, allowed transitions, and accepted outcomes belong in the protocol model.

### TLA+ And Rust In Lockstep

The TLA+ spec in [spec/](spec/SupervisorProtocol.tla) defines the abstract contract. The Rust kernel in [kernel/](kernel/Cargo.toml) implements that contract. Any semantic change is supposed to be reflected on both sides, then checked with TLC and Rust tests before it is trusted.

This is not documentation theater. The point is to keep the deployed semantics and the modeled semantics aligned closely enough that the model is useful for finding real bugs.

### Python As A Thin Operational Layer

Python exists to do the parts that are operationally convenient outside the kernel:

- bridge/orchestration
- agent API integration
- prompt rendering from kernel-authored contracts
- launching external deterministic tools
- moving artifacts around the filesystem

Python should not be the place where protocol meaning is invented. The long-term direction of the repo is to keep semantic authority in Rust and mirror it in TLA+.

### Deterministic Gates Around Agent Work

Agents are useful for proposing proof edits, proof structure, and review judgments. They are not treated as the authority on whether those proposals are acceptable. Acceptance should be driven by deterministic checks tied to explicit contracts, so that the same candidate state is judged the same way no matter which agent produced it.

### State That Survives Long Proof Efforts

Formalizing a paper is not a single burst. It is a long process with many partial results, reversions, repairs, and local retries. Trellis persists protocol state, verification state, and runtime artifacts so that the system can resume intelligently rather than starting over every cycle.

## Repository Structure

- [kernel/](kernel/Cargo.toml): Rust kernel, runtime, and CLI entrypoints
- [trellis/runtime/](trellis/runtime/bridge.py): Python runtime bridge and operational plumbing
- [trellis/checking.py](trellis/checking.py): stable Python-facing checking facade
- [trellis/atomic_actions/](trellis/atomic_actions/README.md): atomic local tool runners still exposed to the checker facade
- [spec/](spec/SupervisorProtocol.tla): TLA+ protocol model and TLC harness
- [scripts/](scripts/trellis.sh): setup, runtime, viewer, and checker helper scripts
- [tests/](tests): carried regression and migration tests
- [INSTALLATION.md](INSTALLATION.md): system requirements (32 GB RAM minimum,
  48 GB recommended; SSD), which distros run the sandbox without root
  (recommended: Ubuntu 26.04 LTS — no root setup needed), host setup,
  dependencies, worker sandbox, and macOS/VM guidance
- [FILESPEC.md](FILESPEC.md): file-shape and structural constraints for tablet artifacts

## Operating Runs

The supported operator surface is small. Use the existing scripts and keep repo state, runtime state, and the supervisor workspace in sync. Most painful failures in this project have come from partial resets.

### 1. Create Or Recreate A Repo

To build a repo from a paper:

```bash
./scripts/setup_repo.sh <repo_path> <paper_tex_path> [project_slug]
```

Give `<repo_path>` its own directory **outside the trellis source tree** —
conventionally under the projects root the viewer reads (default `~/math`),
e.g. `~/math/connectivity`. Any location works; point the viewer at it with
`TRELLIS_PROJECTS_ROOT`.

For a clean rebuild in place:

```bash
./scripts/setup_repo.sh --reset --yes <repo_path> <paper_tex_path> [project_slug]
```

Useful flags:

- `--mathlib-build-tar <path>`: seed the worker-side `mathlib/.lake/build` tree from a local tarball
- `--main-result-labels <labels>`: override the labels used to identify the main paper result

This is the right tool for creating or reseeding a repo. Do not try to reconstruct a repo manually by mixing copied `Tablet/`, `.lake/`, and `.trellis/` state.

### 2. Initialize And Run A Runtime

**Launch from a shell where every CLI resolves.** The supervisor builds the
worker burst's `PATH` and the read-only CLI/elan binds from `shutil.which` on
its **own** PATH at burst time (`trellis/host_runtime.py`:
`worker_provider_bin_dirs` / `worker_elan_home` / `worker_path_env`). Whichever
shell launches `restart_configured_run.sh` (or `trellis.sh run`) must therefore
resolve every tool a burst needs — nvm sourced, elan on `PATH`. This is the
single most likely silent failure for a new host. Check first:

```bash
which codex claude gemini lake node    # all must succeed
```

Then run the provider preflight, which exercises the real worker sandbox and
every configured provider end to end (CLI resolution inside the bwrap binds,
repo write-protection, auth, and model availability) so problems surface now
rather than mid-run:

```bash
python3 -m trellis.provider_check --config <repo>/trellis.config.json
```

The supported one-command launcher is:

```bash
./scripts/restart_configured_run.sh <config_path> <runtime_root>
```

`<config_path>` is the run's `trellis.config.json` (it carries `repo_path`);
`<runtime_root>` is where runtime state lives. The launcher performs a clean
restart end to end: it recreates the repo with `setup_repo.sh --reset`,
reinitializes the runtime with `trellis.sh init`, starts the viewer, starts the
unified-checker server in a sibling tmux session, waits for it to bind its UNIX
socket, exports `TRELLIS_CHECKER_SOCKET`, and launches `trellis.sh run` in tmux
with that socket in its environment. All acceptance lake checks route through
the checker server (there is no host-lake fallback), so this socket export is
mandatory — running the bare CLI sequence by hand without it fails at the first
acceptance check.

The launcher prints the tmux session names it created. Watch the run with:

```bash
tmux -L trellis attach -t trellis-run-<project_slug>      # the supervisor
tmux -L trellis attach -t trellis-checker-<project_slug>  # the checker server
```

`<project_slug>` is the basename of the repo. Useful flags:

- `--no-run`: set up the workspace but do not launch the supervisor.
- `--no-current`: skip refreshing the `$HOME/math/current` symlinks.
- `--check-only`: dry-run; print the planned tmux invocations and exit.

**Runtime-root form.** The checker server derives the repo from `<runtime_root>`
and only accepts two layouts: the inner form `<repo>/.trellis/runtime/<name>`,
or the outer (sibling) form `<parent>/<repo_basename>-runtime`. Pass a
`<runtime_root>` in one of these forms; an arbitrary path that `trellis.sh init`
would otherwise accept will be rejected by the checker server.

The underlying runtime CLI wrapper is `scripts/trellis.sh`:

```bash
./scripts/trellis.sh init <config_path> <runtime_root>
./scripts/trellis.sh show <runtime_root>
./scripts/trellis.sh preview <runtime_root>
./scripts/trellis.sh step <runtime_root>
./scripts/trellis.sh run <runtime_root> [max_steps]
```

If you must drive these by hand instead of using the launcher, you have to
reproduce what the launcher does: start the checker server and export its socket
before `run`, otherwise acceptance checks fail. Launch **both** the checker
server and the supervisor from a shell where your toolchain resolves (the
launch-shell PATH note above): the supervisor derives the worker burst PATH from
its own, and the checker server runs `lake` for acceptance checks.

```bash
# 1. Initialize the runtime.
./scripts/trellis.sh init path/to/trellis.config.json path/to/<repo>-runtime
# 2. Start the unified-checker server (binds <runtime_root>/sockets/checker.sock).
./scripts/trellis_checker_server.sh path/to/<repo>-runtime   # leave running
# 3. Run the supervisor with the socket exported into its environment.
export TRELLIS_CHECKER_SOCKET=path/to/<repo>-runtime/sockets/checker.sock
./scripts/trellis.sh run path/to/<repo>-runtime
```

For bounded execution, append a step count to `run` (the socket must still be
exported):

```bash
./scripts/trellis.sh run path/to/<repo>-runtime 1
```

or drive it manually with:

```bash
./scripts/trellis.sh preview path/to/<repo>-runtime
./scripts/trellis.sh step path/to/<repo>-runtime
```

That is the closest thing the current repo has to a supported "pause" mechanism. There is no dedicated generic pause script.

### 3. Restarting Cleanly

To start over from a known-good state, re-run the same launcher (§2): it
recreates repo, runtime, checker server, and supervisor together. Treat those as
a single unit — most painful failures in this project have come from partial
resets, which is exactly what recreating all of it at once avoids.

### 4. Watching A Run — Set Up The Viewer

**The web viewer is the primary way a human is expected to interact with the
formalization process — set it up for any real run** (INSTALLATION §2e;
`./scripts/start_viewer.sh`, default <http://127.0.0.1:3301/trellis/>). This
matters most at the human gates: **approval gates and `NeedInput` escalations
surface as banners in the viewer**, with approve / send-input as the primary
actions — a run parked at a gate waits indefinitely for you, and the viewer is
where you find out. Beyond the gates it carries the live DAG and per-node
verification state, halt banners with their diagnostics, pause/resume controls
and the weekly-budget pause floor, the Grunts tab, usage, and a notice when a
newer Trellis release is available. The CLI surfaces below cover the same
state read-only and are fine for a terminal check-in; the gates and controls
live in the viewer.

High-level runtime state:

```bash
./scripts/trellis.sh show <runtime_root>
```

For a live split-panel TUI — cycle/phase/active node, live and committed node
counts, coarse-DAG shallow-closed progress, last review/worker, kernel-contract
counters, and a tail of the active burst's chat — run from the repo:

```bash
python -m trellis.cli_monitor              # live; q to quit
python -m trellis.cli_monitor --once       # one snapshot, no alt-screen
```

It reads the same on-disk JSON the web viewer reads (no Node/HTTP). Useful when
you want progress numbers in a terminal alongside the tmux session.
`cli_monitor` is the one part of the repo that needs a pip package — install it
with `pip install rich` (the supervisor and test suite are stdlib-only).

**Known limitation — live chat tail.** The live chat/activity panel (both
`cli_monitor` and the web viewer) currently tails `codex` output as it streams;
`gemini` and `claude` transcripts render only after a burst completes. The stat
panels (cycle/phase/node counts) are provider-agnostic.

If you launched the supervisor under tmux, attach to the session you created.
When the tmux session is not enough, the next things to inspect are:

- `<runtime_root>/protocol_state.json`
- `<runtime_root>/event_log.jsonl`
- the repo-local staging directory under `.trellis/runtime/<runtime-name>/staging/`

As a rule, prefer these durable state files over guessing from leftover worker processes.

### 5. Stopping, Stepping, And Resuming

For a deliberate pause, use the pause state machine:

```bash
./scripts/trellis_pause.sh arm    <runtime_root> <repo_path> [--reason TEXT] [--by WHO]
./scripts/trellis_pause.sh status <runtime_root> <repo_path>   # JSON: running | arming | paused | down
./scripts/trellis_pause.sh resume <runtime_root> <repo_path>
```

`arm` writes a durable, reason-carrying `<runtime_root>/pause_request.json`
beside the stop sentinel; the run stops cleanly at its next checkpoint (or at
an open human gate) and the *state sits on disk with the process down*, so a
pause left overnight cannot decay with its environment. `resume` relaunches by
replaying `<runtime_root>/launch_env.json` — the exact environment the run was
launched with, captured at every start — rather than guessing one, refuses if
the source tree the run was launched from has moved, and clears the pause
record only once the relaunch confirms. The viewer shows a paused run on its
own attention tier with the recorded reason, and can arm/resume the same state
machine from the UI. Two automatic arms exist: a human gate left open past
`gate_park_after_minutes` (default 120) is parked into a pause rather than left
polling in a killable process, and the weekly budget bar's "pause run at" floor
(default 5% remaining) arms one server-side.

Below the pause machinery, the primitives:

- use `run <runtime_root> <max_steps>` for bounded execution
- use `preview` + `step` for manual stepping
- request a graceful reload stop by touching the repo-local sentinel:

  ```bash
  touch "$(jq -r '.repo_path' <runtime_root>/runtime_metadata.json)/.trellis-stop-after-checkpoint"
  ```

  The long-running `run` loop checks this file between persisted steps, removes
  it, prints `stop-after-checkpoint sentinel detected`, and exits without
  killing the in-flight worker/verifier/reviewer. After changing or rebuilding
  the kernel, resume with the same runtime root:

  ```bash
  export TRELLIS_CHECKER_SOCKET=<runtime_root>/sockets/checker.sock  # if the checker server is no longer running, restart it first (see §2)
  ./scripts/trellis.sh run <runtime_root>
  ```

  This reloads from `<runtime_root>/protocol_state.json` and refreshes the
  in-flight request from current kernel code. The checker server and socket
  export are required here too.
- if the supervisor is running in tmux, stop it by ending that tmux session or killing the supervisor process tree

What is not a good operational pattern:

- deleting selected runtime files by hand
- restarting only the runtime while leaving the repo dirty
- keeping a stale supervisor workspace while reusing an older runtime root
- relaunching on top of half-finished worker edits and hoping the supervisor sorts it out

If a run is in a questionable state, prefer a clean restart over ad hoc surgery.

**Footguns worth knowing:**

- **Commit config edits.** The run loop does `git reset --hard` / `git clean
  -fd` against the project repo (`kernel/src/bin/runtime_cli.rs`), so an
  **uncommitted** edit to `trellis.config.json` (e.g. a provider/model change)
  is silently reverted. Commit the change in the project repo before launching.
- **Re-applying config changes may need a fresh runtime root.** The runtime
  copies config at `init`; to make a config change take effect cleanly you may
  need to `rm -rf <runtime_root>` before re-running `trellis.sh init` (or just
  use the launcher, which recreates it).
- **A run killed mid-burst leaves stale state that `git clean -fd` does NOT
  clear.** `git reset --hard HEAD && git clean -fd` restores tracked files (and
  preserves the gitignored `.lake/` + mathlib cache, so no rebuild) — but it
  leaves the rest of the gitignored `.trellis/` untouched, including leftover
  worker `.done` markers and result artifacts under
  `<repo>/.trellis/runtime/<name>/staging/`. The kernel consumes those, so a
  "fresh" launch can silently **reuse or skip** that burst (e.g. the worker
  "already finished" against an empty Tablet) instead of re-running it — and
  deleting only the sibling `<runtime_root>` doesn't help, because the
  repo-internal `.trellis/runtime/` is separate. For a genuinely clean start use
  the supported restart (`restart_configured_run.sh`, or `setup_repo.sh
  --reset`), which recreates the repo, runtime, and supervisor workspace
  together (§3); a hand-rolled `git clean` is not equivalent.
- **`system_feedback` is logged by default; halting on it is opt-in.** When an
  agent burst returns a non-empty `system_feedback`, the emission is appended
  to `<runtime_root>/system_feedback_log.jsonl` (full text, stable
  fingerprint, cycle/request provenance), a notice is printed to the run log,
  and the run continues. `system_feedback` signals a design gap or harness
  bug, so review the log periodically (the viewer serves it at
  `/api/system-feedback.json`). Operators who prefer the fail-loudly behavior
  — every emission freezes the run for review — can set top-level
  `"system_feedback_halt": true` in `trellis.config.json` (or export
  `TRELLIS_SYSTEM_FEEDBACK_HALT=1`; the env var beats the config in both
  directions). With halting enabled, the supervisor stops dispatching new
  bursts and writes `<runtime_root>/system_feedback_halt.json`; that is a
  review checkpoint, not a crash. Read the file (it carries the diagnostic and
  `clear_instructions`), fix the underlying cause, then resume by deleting it
  — `rm <runtime_root>/system_feedback_halt.json` — and re-running (§2). A
  marker already on disk always halts the run regardless of the knob, and
  known-benign fingerprints can be acknowledged (`ack_system_feedback`) to
  log-and-continue without disabling halts for novel feedback.

**Upgrading Trellis mid-run.** A long formalization does not need to be
sacrificed to take an upgrade; the pause machinery above is the supported
path. In order:

1. **Pause the run**: `./scripts/trellis_pause.sh arm <runtime_root>
   <repo_path> --reason "upgrade"`. This writes the durable pause record and
   the stop sentinel; the supervisor stops cleanly at its next checkpoint (or
   straight from an open human gate) and exits. Wait until
   `trellis_pause.sh status` reports `paused` — at that point no supervisor
   process is left and all run state is on disk.
2. **Drain the closure sidecar, if one is running**: `touch
   <runtime_root>/sidecar/drain`. In-flight grunt attempts keep running and
   the post-upgrade daemon adopts them — never `stop` for an upgrade, which
   kills and charges them (`SIDECAR_OPERATIONS.md` §4).
3. **Upgrade in place**: `git pull` (or check out the release tag) in the
   Trellis source tree the run was launched from, rebuild the kernel binary
   (§Validation), and restart the viewer (`./scripts/start_viewer.sh`).
   Upgrading *in place* matters: resume verifies the run's recorded source
   tree and refuses one that has moved.
4. **Restart the checker server** against the same runtime root (§2) so it
   runs the upgraded code before the supervisor comes back.
5. **Resume**: `./scripts/trellis_pause.sh resume <runtime_root> <repo_path>`.
   This replays the exact launch environment recorded at the last start
   (`launch_env.json`) — no reconstructing env vars by hand — and clears the
   pause record only once the relaunch confirms.

The plain sentinel (`touch <repo>/.trellis-stop-after-checkpoint`, above) is
the bare-hands version of step 1 and still works, but it records no reason and
leaves resume to you; prefer the pause script for anything that stays down
longer than the edit you are making.

### 6. Best Practices

- Treat repo state, runtime state, and supervisor workspace state as one unit.
- Assume a semantically dirty worktree is not safe to relaunch on unless you are intentionally resuming that exact in-progress attempt.
- Use `show`, `protocol_state.json`, and `event_log.jsonl` as the authoritative view of progress.
- Use bounded `run ... <max_steps>` or `step` if you want explicit control points.
- When in doubt, choose the path that recreates more state, not less.

### 7. Public Tablet Viewers

For a finished tablet repo, build a static public viewer with:

```bash
./scripts/build_public_tablet_viewer.py \
  <repo_path> \
  /tmp/<viewer-name> \
  --title "<Formalization Title>" \
  --github-base https://github.com/<owner>/<repo>/blob/<branch>
```

The wrapper builds `Tablet`, precomputes recursive Mathlib imports, computes
semantic closures, writes build information including top-level target
`#print axioms` output, and packages the result as `/tmp/<viewer-name>.tar.gz`.
By default, Lean/Lake work is throttled with one job/thread, `nice -n 19`,
idle I/O priority, and CPU affinity to core 0.

For a quick UI-only preview, use:

```bash
./scripts/build_public_tablet_viewer.py <repo_path> /tmp/<viewer-name> \
  --semantic skip --no-build --no-cache-get
```

Deploy the generated static files to any static web directory. Keep the
trailing slash on the source path:

```bash
rsync -av --delete /tmp/<viewer-name>/ <host>:<public-web-dir>/<viewer-name>/
```

### 8. Benchmark Runs (Challenge Targets)

Trellis can target benchmarks that prescribe the exact Lean form of the goal —
e.g. the [Lean AI formalization leaderboard](https://lean-lang.org/eval/),
where each problem fixes supporting definitions and a theorem statement that a
solution must prove verbatim. A run then carries **challenge targets**
alongside (or instead of) paper targets: prescribed declarations that must be
covered by nodes whose Lean text the kernel enforces byte-for-byte at
acceptance, while proofs, decomposition, and the rest of the DAG remain
worker-authored and pass through every verification lane as usual. One node
may cover both a challenge target and a paper target.

The pipeline:

1. Download the problem's files and derive the spec deterministically —
   prescribed text is never hand-transcribed:

   ```bash
   scripts/import_challenge_targets.py <problem-download-dir> --out challenge_targets.json
   ```

2. Create the run with the spec plus a reference TeX paper proving the result
   (the paper does not reference the challenge; it remains the grounding input
   for the faithfulness and correspondence lanes):

   ```bash
   scripts/setup_repo.sh --challenge-targets challenge_targets.json <repo> <paper.tex>
   ```

   The spec lands in `trellis.config.json` as
   `workflow.challenge_targets_path`, and the benchmark's toolchain pins
   recorded by the importer are applied automatically (an explicitly exported
   `MATHLIB_TOOLCHAIN` / `MATHLIB_REV` that disagrees with the spec is an
   error). Initialize and run as in
   sections 1–2; uncovered challenge targets block phase advance and
   completion exactly as uncovered paper targets do.

3. After the run completes, export the submission artifact and validate it
   the way the benchmark's CI will:

   ```bash
   scripts/export_submission.py <repo> challenge_targets.json <problem-download-dir> --out <out-dir>
   ```

   A zero exit means `<out-dir>` is a directly submittable repo
   (`Submission.lean`, `Submission/`, `lakefile.toml`), verified by replaying
   the benchmark's own comparator against a pristine copy of the problem
   workspace.

`CHALLENGE_TARGETS_DESIGN.md` records the design and its decisions.

### 9. Revision Runs

When a paper gets a new version that strengthens a result, adds theorems, or
drops some exposition, Trellis can revise an existing tablet against the newer
source instead of reformalizing from scratch. A revision run starts from an
already-formalized basis — a closed tablet that passed every lane on the old
paper — and reopens only the additive/improving delta, judged against the new
paper.

The starting point is sound by construction. A closed tablet that passes
correspondence and soundness on every node is genuinely formalizable, so nothing
in it is false; a newer version cannot be *correcting* a result that was already
proven. So the default at import is to **inherit every prior approval** and
reopen only what a relevant tablet change reopens under the existing kernel
rules. The verification surface collapses to the new and restated nodes.

Concretely, import carries the prior correspondence and soundness approvals
verbatim (both are tablet-side fingerprints — neither reads the paper — so a
paper-version swap does not perturb them); inherits paper-faithfulness per
target while force-invalidating every changed and added target; and re-baselines
substantiveness to `Pass` for every present node against the *new* paper, so a
pure version swap reopens no unchanged node while a restated or new node still
re-enters the lane.

Prepare the working repo with the operator script:

```bash
./scripts/setup_revision_repo.sh \
  --base-repo <existing-tablet-repo> \
  --old-paper <old-main-tex> \
  --new-paper <new-main-tex> \
  --out-repo <new-working-repo> \
  [--target-map <target-map-json>] \
  [--old-source-id arXiv:...v1] [--new-source-id arXiv:...v3]
```

`--base-repo` must hold the prior full state at
`.trellis-history/supervisor_state.json`. The script copies the base working
tree to `--out-repo` (it fails rather than overwrite), copies both paper sources
to `paper/revision/{old,new}.tex`, points `workflow.paper_tex_path` at the new
source and records a `workflow.revision` block:

```json
{
  "workflow": {
    "paper_tex_path": "paper/revision/new.tex",
    "revision": {
      "old_paper_tex_path": "paper/revision/old.tex",
      "old_source_id": "arXiv:...v1",
      "new_source_id": "arXiv:...v3"
    }
  }
}
```

`paper_tex_path` (the new source) remains the current paper for every ordinary
verifier lane. The script then calls the `import_revision_project` runtime CLI
action, which diffs the two papers, classifies each target as unchanged /
changed / added / removed, computes the frozen and editable node sets, applies
the inheritance above, and writes the initial state — phase `RevisionStating`,
stage `StuckMathAudit` — so the next supervisor cycle dispatches the first
**revision-planning** audit.

The script does not run the planner. Point the normal launcher (§2) at the
prepared repo and runtime root:

```bash
./scripts/restart_configured_run.sh <new-working-repo>/trellis.config.json <runtime-root>
```

From there the run proceeds: the revision-planning `StuckMathAudit` reads both
papers and the current tablet and proposes a scoped update plan (which nodes to
restate, copy, freeze, or add); the reviewer and worker restate authorized
*editable* nodes and add new ones, with frozen nodes — preamble, axioms, and the
covering/closure nodes of unchanged carried targets — protected by validation;
changed targets and changed nodes re-enter the verifier lanes while unchanged
approvals inherit. A `RevisionStating → HumanGate → ProofFormalization`
transition gates the revised statement set: HumanGate approval advances the run
into ordinary proof formalization over the changed cone.

`revision_plan.md` records the design and its decisions.

### 10. Closure Sidecar (Grunt Workers)

A run can put a small pool of cheap proof-search agents — "grunts" — to work
beside the main loop, each trying to close one open proof node. A grunt writes
only the proof body below its node's `-- BODY` marker, so it closes a node
exactly when the proof fits there from the existing imports and dependency
lemmas; anything needing new imports, a helper, or restructuring stays primary
work. Only a complete closure is accepted — every candidate passes the same
kernel gates and authoritative checker as primary work — and the primary
workflow always wins a conflict.

The queue has two lanes over one kernel ranking: the reviewer's lane (worked
first — queueing a node means "work this first", not "work this at all") and a
kernel lane that keeps a standing ranked list of every eligible node, refilled
each cycle boundary, so a free grunt idles only when both lanes are exhausted.
Auto-dispatch stops refilling a node after the pool has failed it 5 times
(`max_attempts`, counted per node content, so a repaired node regains its
allowance). Closures are attributed per node in `closure_provenance` (`worker`
or `sidecar`), and the viewer's **Grunts** tab shows the pool, queue, and
attempt history.

**The feature is inert unless enabled**: with no `sidecar` block in
`trellis.config.json`, or `"enabled": false`, nothing is exported, the reviewer
is never told grunts exist, and no daemon runs.

#### Setup

Grunt attempts run through the codex CLI (`codex exec`), defaulting to the
**`gpt-5.6-luna`** model — cheap enough to grind long proof searches beside the
main loop. There is no separate API key: grunts authenticate the same way your
main-loop codex does. Each grunt gets its own private `CODEX_HOME` (so a grunt
credential failure can never unauthenticate the formalization loop), re-seeded
from your `~/.codex/auth.json` at every attempt launch — if `codex` works on
the host, grunts work.

Enable it in `trellis.config.json`, where `daemon.grunts` is the pool size (the
only volume throttle) and `budgets.attempt_wall_seconds` caps one attempt —
wall clock is the sole per-attempt budget:

```json
{"sidecar": {
  "enabled": true,
  "daemon": {"grunts": 2},
  "budgets": {"attempt_wall_seconds": 5400}
}}
```

The model block is optional and defaults to
`{"provider": "codex", "name": "gpt-5.6-luna", "reasoning_effort": "high"}`;
set `sidecar.model.name` to point grunts at a different codex model. The agent
runs untrusted: inside a bwrap sandbox under the dedicated `grunt` role, its
candidate is harvested from the node file (its own success claim is discarded),
scanned against banned tokens and top-level declarations, and compiled by the
harness before anything reaches the kernel.

Then start the daemon alongside the supervisor:

```bash
./scripts/trellis_sidecar.sh <runtime-root> --repo <working-repo>
```

It owns `<runtime-root>/sidecar/` (spool, per-grunt workspaces, `status.json`,
ledger). Two sentinels stop it, and the difference is what happens to the
attempts that are still running:

* `<runtime-root>/sidecar/stop` — **hard stop**: in-flight attempts are
  cancelled and rolled back, then the daemon exits.
* `<runtime-root>/sidecar/drain` — **drain**: the daemon stops assigning and
  exits immediately, leaving its attempt processes running. They finish on
  their own (each enforces its own wall), and the next daemon started over the
  same runtime root ADOPTS them off `slots.json`: same grunt slots, same
  attempt ids, same start times, results reaped and reported normally. Use this
  to restart or redeploy the daemon without throwing away an hour of grunt
  work. A `stop` sentinel present at the same time wins.

Both sentinels are consumed (unlinked) on the way out. A drain leaves live
children behind, so the *code they are running* must stay compatible for as
long as they run: deploy into a new worktree and point the relaunch at it
rather than editing the source tree under a live attempt, and never roll the
daemon BACK to a build without adoption while attempts are in flight (pre-drain
code cannot see them and will re-attempt their generations).

#### Operating it

```bash
./scripts/trellis_sidecar.sh status <runtime-root>          # or --json
```

Exit codes: `0` running and fresh, `1` not running, `2` degraded (stale status
or a pending sentinel), `3` no sidecar directory, `4` undeterminable.

Two rules worth knowing before anything else:

* **Never identify the daemon by process name.** Each in-flight attempt runs as
  `python3 -m trellis.sidecar.attempt …`, which contains the manager's own
  module name, and those children deliberately outlive the manager across a
  drain. `pgrep -f trellis.sidecar` therefore reports a dead manager as alive;
  worse, `pkill -f` on that pattern is unscoped across runs. Use `status`,
  which answers from the daemon's pid lock.
* **`stop` spends the queue generations of everything it kills** — the reviewer
  must re-add those nodes. `drain` is the restart path and costs nothing.

`SIDECAR_OPERATIONS.md` is the full operations reference: the three argv
shapes, what each `last_pass` verdict means, the spool lanes and their
ownership direction, the stale-sentinel trap, and a troubleshooting table keyed
by symptom.

## Validation

Build the kernel binary **before** running `pytest` — the Python suite invokes
it, so without it ~16 tests fail with `cannot find cargo for trellis kernel
invocation`. The full suite also needs a Lean / `lake` install (see
`INSTALLATION.md`). There are no Python *package* dependencies, but the test
suite is not toolchain-free.

Build the **debug** profile — `kernel/target/debug/trellis_runtime_cli`, which
is what the plain `cargo build` below produces. Two independent resolvers both
land there by default, and they are not the same mechanism:

* the **runtime** (`trellis/runtime/kernel_cli.py`) takes
  `$TRELLIS_TRELLIS_KERNEL_CMD` if set, else a vendored `bin/trellis_runtime_cli`
  beside the source tree, else `kernel/target/debug/`, else falls back to
  `cargo run`. It never looks at a release build;
* the **sidecar e2e test** (`tests/test_sidecar_e2e.py`) takes
  `$TRELLIS_KERNEL_BIN` if set — and a path it names that does not exist is a
  hard error rather than a fallthrough to a different binary — else
  `kernel/target-worktree/debug/`, `kernel/target/debug/`, `kernel/target/release/`
  in that order.

`TRELLIS_KERNEL_BIN` is read only by that test; the runtime ignores it.

```bash
# Build the kernel binary first.
cargo build --bin trellis_runtime_cli --manifest-path kernel/Cargo.toml

CARGO_BUILD_JOBS=2 cargo test -q --manifest-path kernel/Cargo.toml
PYTHONDONTWRITEBYTECODE=1 python3 -m pytest tests/ -q

# Viewer (Node). Needs `npm ci` in viewer/ once; exits non-zero on any failure.
npm --prefix viewer test
```

Some failures are environment-gated, not regressions: the Lean-dependent tests
(`lean_semantic_*`, `print_axioms`) need a working Lean/`lake` toolchain.

The `local_closure_smoke` kernel target is the regression harness for two
resolved local-closure soundness holes, so it fails loudly rather than passing
vacuously when its Lean fixture is unbuilt. Build the fixture once (fast — it
avoids Mathlib):

```bash
(cd kernel/tests/fixtures/local_closure_smoke && lake build)
```

On a host with no Lean toolchain, opt out of the harness explicitly instead:

```bash
TRELLIS_ALLOW_FIXTURE_SKIP=1 CARGO_BUILD_JOBS=2 cargo test -q --manifest-path kernel/Cargo.toml
```

## License

Trellis is source-available for academic and noncommercial use under the PolyForm Noncommercial License 1.0.0. See `LICENSE`.

Commercial use requires a separate written commercial license. See `COMMERCIAL.md`.

Contributions: external pull requests will not be reviewed without prior discussion. See `CONTRIBUTING.md`.

Third-party dependencies remain under their own licenses. See `THIRD_PARTY_NOTICES.md`.

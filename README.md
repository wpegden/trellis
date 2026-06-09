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
- [INSTALLATION.md](INSTALLATION.md): host setup, dependencies, worker sandbox
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

### 4. Watching A Run

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

There is no single supported `pause.sh` or `rewind.sh` in the current repo.

What is supported:

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
- **A `system_feedback` halt is deliberate, not a crash.** When an agent burst
  returns a non-empty `system_feedback`, the supervisor stops dispatching new
  bursts and writes `<runtime_root>/system_feedback_halt.json`. This is by
  design: `system_feedback` signals a design gap or harness bug — and because
  the project is under active development, that is often a freshly introduced
  one — so halting immediately surfaces it for a human instead of letting the
  run burn agent budget cycle after cycle against a broken state. It is a
  review checkpoint, not a failure. Read the file (it carries the diagnostic
  and `clear_instructions`), fix the underlying cause, then resume by deleting
  it — `rm <runtime_root>/system_feedback_halt.json` — and re-running (§2).
  In practice these cluster during initial install/shakeout; once the deployment
  is sound you should fully expect a run of hundreds of cycles to complete
  end-to-end without any `system_feedback`-related halt.

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

## Validation

Build the kernel binary **before** running `pytest` — the Python suite invokes
it, so without it ~16 tests fail with `cannot find cargo for trellis kernel
invocation`. The full suite also needs a Lean / `lake` install (see
`INSTALLATION.md`). There are no Python *package* dependencies, but the test
suite is not toolchain-free.

```bash
# Build the kernel binary first.
cargo build --bin trellis_runtime_cli --manifest-path kernel/Cargo.toml

CARGO_BUILD_JOBS=2 cargo test -q --manifest-path kernel/Cargo.toml
PYTHONDONTWRITEBYTECODE=1 python3 -m pytest tests/ -q
```

Some failures are environment-gated, not regressions: the Lean-dependent tests
(`lean_semantic_*`, `print_axioms`) need a working Lean/`lake` toolchain.

## License

Trellis is source-available for academic and noncommercial use under the PolyForm Noncommercial License 1.0.0. See `LICENSE`.

Commercial use requires a separate written commercial license. See `COMMERCIAL.md`.

Contributions: external pull requests will not be reviewed without prior discussion. See `CONTRIBUTING.md`.

Third-party dependencies remain under their own licenses. See `THIRD_PARTY_NOTICES.md`.

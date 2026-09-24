# Changelog

All notable changes to the public Trellis releases are documented here. This
project adheres to [Semantic Versioning](https://semver.org/).

## v0.3.1 — 2026-09-24

- Bursts no longer trigger Lake's "URL has changed; deleting" reclone of Mathlib
  inside the sandbox. The cause was git refusing a checkout owned by a different
  user ("dubious ownership") in the raw burst shell, which Lake swallowed and
  treated as a changed remote. Managed Lake calls had always received exact
  `safe.directory` grants; ordinary burst shells now receive the same grants for
  the project and its `.lake/packages/*` checkouts, and a preflight gate inside
  the sandbox writes a durable launch report under
  `$HOME/.trellis/git-preflight/` and stops the burst only when Lake's own
  deletion condition is established. (a311da81)
- The Sound verifier no longer demands a `\noderef{Preamble}` citation that
  acceptance then rejects. FILESPEC and the canonical SOUNDNESS rubric now say
  once that `Preamble.tex` items are shared context used without a citation,
  and the rendered soundness contract carries the same exception. (e8223110)
- The new-run and PV launchers lead with a launch preset — astra-guided sol,
  all sol, sol-guided luna, all luna — that sets every role from one choice; a
  guided preset puts the guide model on the audit lane only. The per-role
  table stays available behind "show per-role settings". All-sol is the
  default, the loogle selector defaults to off, and the launcher no longer
  enforces Phase-0 lane model independence. (06f62a8a, c3abc50b, fe2bd9c8)
- The viewer stays responsive while a run page is open: the live-state and
  state-at reads that spawned the python adapter synchronously on every poll
  now refresh in the background with single-flight coalescing. (fe2bd9c8)
- Certification is several times faster on import-heavy tablets: parsed Lean
  imports are memoized by exact content with bounded retention, and definite
  local-closure cache misses skip replay validation. Certification decisions,
  evidence and hashes are unchanged; measurement-only instrumentation records
  per-request timing and lock intervals. (3ce8a54e, 90b44900, 8a54643c)
- Codex burst homes no longer share mutable CLI state with the quota probe:
  the probe is a read-only usage query that launches no Codex process, and
  teardown waits a bounded grace after `turn.completed` before forcing exit.
  Non-object JSON lines in Codex burst logs are ignored. (e4aa0b12, 88d5470f)
- The sandbox mounts the npm provider runtime dependencies read-only, and
  rollback certificates are mirrored as root-compatible transactions.
  (296d33f3, 153db097)

## v0.3.0 — 2026-09-08

- A run can now be created from the browser, end to end. The viewer's root URL
  is a landing page — one card per run under the projects root, each carrying
  the lifecycle state read from disk (never started, live, paused, halted,
  finished, abandoned), host RAM/swap/disk against the documented
  requirements, supervisor/checker/sidecar liveness and provider budget — and
  from it a wizard takes an uploaded `.tex` through target selection, build,
  launch and verification with a live log tail. `scripts/trellis_create_run.sh`
  owns the state machine and the on-disk formats and the viewer is a thin
  caller, so the same job is drivable by hand and reports identical progress.
  Creation is deliberately two-phase and parks *process-down* at
  `awaiting_targets`: the state a human sits in for hours holds no process that
  can decay, so a reboot during the wait needs no recovery and a retry
  relaunches only the current phase. The wizard refuses to confirm while a
  configured lane has no provider credentials (printing the exact login
  command), warns on free disk before the prewarm can spend an hour finding
  ENOSPC, mines recovery hints out of a failed phase's log, and lets reference
  papers, the Loogle choice, the git remote, sidecar width and per-role
  model/effort all be settled on the way in. (32ffaeb9, 5fa12f5c, 0219018e)
- **The viewer's access model changed, and an operator who reaches it over the
  network must act.** It binds `127.0.0.1` and no longer answers from the
  network unless `TRELLIS_VIEWER_BIND` is set, which it warns about loudly;
  the endorsed way in is an SSH tunnel (`ssh -L 3301:127.0.0.1:3301 <host>`),
  which supplies encryption and authentication with no TLS to configure.
  Because the viewer pauses and relaunches supervisors as your user, its
  controls now live under a secret 8-character path segment:
  `<base>/<token>/` is the same landing page with pause/resume and feedback
  enabled, every plain URL keeps working read-only, and `start_viewer.sh`
  prints the whole URL on its `viewer_control_url=` line (the token also sits
  at `<projects root>/.trellis-viewer/control-token`, mode 0600 — delete it and
  restart to rotate). Writes additionally carry a custom `X-Trellis-Control`
  header, which is what a hostile page you visit cannot forge cross-origin.
  Every request must also carry a `Host` this machine actually goes by, which
  closes DNS rebinding with nothing to configure; reach it through an alias or
  a `Host`-rewriting proxy and add that name to
  `TRELLIS_VIEWER_ALLOWED_HOSTS`. Once the bind is not loopback, reads require
  the token URL too (`TRELLIS_VIEWER_READ_TOKEN`, `auto` by default). Set
  `TRELLIS_VIEWER_CONTROL=0` before exposing the viewer anywhere.
  (738ff785, 9e8eda47, 2490e136)
- Both nginx installers now refuse to publish an unguarded plaintext viewer
  unless `AUTH_FILE=` or `ALLOW_FROM=` guards it or you set
  `ACCEPT_PLAINTEXT_EXPOSURE=1` deliberately — over plain HTTP the control
  token rides in the URL and therefore on the wire, so read exposure becomes
  control exposure. They also emit the rule the control subtree needs; a
  hand-written nginx site must proxy `^/trellis/[A-Za-z0-9]{8}(/|$)` to the
  viewer port, ahead of any `/api/control/` deny rule, because the static
  export has no token in it. SECURITY.md gains a "Deployment modes" section
  stating the three options and their verdicts. (eb9c8677, 02c5de84)
- **Isabelle/HOL is a second selectable proof backend.** Set
  `workflow.default_target: "isabelle_hol"` in `trellis.config.json` — the
  shipped `examples/trellis.isabelle.config.json` and
  `examples/trellis.isabelle.policy.json` are ready-made templates, passed to
  `setup_repo.sh` through `CONFIG_TEMPLATE` — and setup builds a `Tablet_Base`
  warm session parented on HOL-Probability into shared system heaps, binds the
  distribution into the worker sandbox, and installs `FILESPEC_isabelle.md` as
  that backend's file-shape contract. Isabelle 2025-2 or newer is required and
  enforced (2025-1 admitted `sorry` in ways the soundness gate assumes
  impossible); point `TRELLIS_ISABELLE_HOME` at your installation. A warm
  checker session holds the accepted prefix elaborated so a re-check costs
  only the in-flight theory, cross-checked against a cold rebuild on a cadence
  and halting the run on any warm-vs-cold disagreement; `quick_and_dirty=false`
  is the enforced floor and closure is attributed oracle-free. Workers get an
  `isa-query` discovery helper emitted into the run repo
  (`find_theorems`/`find_consts`/`solve_direct`/`sledgehammer`/`thm_oracles`)
  and a matching skill. Lean behaviour is unchanged by construction: a repo
  with no config, an unreadable one, or one without the key resolves to Lean in
  both the kernel and Python, and every backend-routed function keeps its
  historical Lean expression. The backend is validated end to end on one paper
  — a G(n,p) connectivity-threshold formalization, 73 nodes, no sorries, all
  verifier lanes Pass — and should be treated as new. (45882c95, 404d1bdc,
  908ed6c6)
- **A Lean-closed node is a stronger claim than it was.** `lake build` alone is
  not a kernel-validity capability: Lean options such as `debug.skipKernelTC`
  can make elaboration emit unchecked declarations, and the process doing the
  verifying could previously rewrite the source it was certifying. The
  authoritative supervisor build now runs with `Tablet/` read-only and
  independently replays every built Tablet module through `leanchecker` before
  recording artifact provenance or letting the artifact be reused; a replay
  failure rejects and purges the whole requested closure. Attestations are
  keyed on olean sha256 plus toolchain, so an unchanged module costs nothing
  and a full re-attestation of a 231-module tablet takes seconds rather than
  twenty minutes. Because provenance is now artifact-native — every serialized
  declaration is captured — the FILESPEC *relaxes* where it used to guess:
  `local` `macro`/`macro_rules`/`elab`/`elab_rules`/`syntax`/`binder_predicate`
  and `#eval` are allowed, and the reserved-shaped-name ban is gone;
  `run_cmd`/`run_elab`/`run_meta`/`by_elab`/`initialize` and explicit `partial`
  are banned, and syntax bans are now stated as defense in depth rather than
  the trust boundary. Policy violations surface as explicit worker-gate
  rejections instead of an `internal_error` out of the closure probe. Related:
  the axiom probe names the exact root declaration (`#print axioms
  _root_.<Node>`), so a node whose name collides with a core or mathlib export
  can no longer be made permanently uneditable by an ambiguous two-verdict
  probe. (cb88be9d, 865274c3, 914f7b4c)
- **The default Lean toolchain and mathlib pin move to the v4.33.0 stable
  release** (from `v4.30.0-rc1`), mathlib at the `v4.33.0` tag. A tagged
  release has a reliable `lake exe cache get`, and the bump is verified through
  the olean cache, the sandbox exec probes and the sorry gate. Crucially,
  **a routine restart will not silently migrate an existing run onto it**:
  `restart_configured_run.sh --reset` recreates the repo from current defaults,
  which would have carried a months-old run across four Lean releases and
  thousands of mathlib commits with nothing naming the cause. Setup now
  refuses, prints both pins, and requires `--adopt-toolchain` to migrate
  deliberately; a run's own pin outranks the shipped default. A separate
  manifest-reader regression that failed every module of a v4.30 corpus before
  reading a single artifact is fixed and pinned by a conformance matrix over
  every supported toolchain. (d9515c87, f500cf00, 4fd04cc4)
- **Grunts now work throughout theorem-stating**, and the sidecar shuts itself
  down when it has nothing left to do. The closure sidecar's window used to
  stay shut during TheoremStating/RevisionStating until every configured paper
  target had a covering node, so on a fresh run the pool idled through the
  whole of stating; that gate was a cost guard, not a soundness condition, and
  it now defaults off — set `sidecar.phases.stating_after_coverage: true` to
  restore it. Node eligibility (substantiveness plus correspondence Pass) is
  untouched, and the `TRELLIS_SIDECAR_STATING_IGNORE_COVERAGE` env bypass is
  removed rather than inverted, because replay must not depend on process
  environment. At the other end, the daemon reads the run's phase from the
  kernel's export and exits on `Cleanup` or `Complete`, cancelling in-flight
  attempts and releasing the grunt checkouts — previously it idle-polled for
  hours holding tens of GB of clones that every resume prep then recopied.
  `status.json` gains a `stopped` block, a `phase_complete` verdict and a
  `workspaces` list of the checkouts actually on disk (as against the
  configured pool width); `SIDECAR_OPERATIONS.md` documents the new states and
  says plainly that deleting an orphaned `grunts/<K>/repo` by hand is safe.
  (8e3ec84d, 4378a732)
- Sidecar defaults are retuned against measurement, and three ways the pool
  could be silently dead are fixed. The per-attempt wall drops from 90 to 60
  minutes (`sidecar.budgets.attempt_wall_seconds`, and it is a hard kill, so
  the number is a real trade rather than a safety margin), settable per run via
  `--grunt-wall` or the wizard field within 300–21600s. The attempt ceiling
  drops from 5 to 3: across 723 measured attempts no closure ever landed above
  effective index 2, while indices 3–5 were 30% of grunt spend for none — a
  content change still resets a node's allowance. The boundary drain goes from
  1 apply per boundary (with a 300s budget) to 10 (2700s): the same hook
  refills the lane and then drained one, and a claim rejected as stale still
  spent its slot, so on a long run most finished grunt work never landed —
  and an unapplied closure keeps drawing NL soundness work on a node that is
  already proved. Separately, `grunts` and the wall were being written flat
  while the loader reads `sidecar.daemon.grunts` and
  `sidecar.budgets.attempt_wall_seconds`, so runs silently took defaults; a
  browser-created run wrote sidecar config but never launched the daemon, and
  every attempt had been crashing on a missing burst home; and the credential
  re-seed wrote a `CODEX_HOME` the launcher did not pin, so a fresh credential
  could sit in a file the grunt never opened. (322eac03, 3042dadd, 041ad720)
- Sidecar hardening, both halves of the untrusted-grunt boundary. `run_tac`,
  `run_elab` and `run_meta` — all three demonstrably able to execute arbitrary
  IO at elaboration time under the tablet toolchain, and `run_tac` reachable
  directly inside a proof body — join `run_cmd` in the pre-compile ban scan
  that runs before the trusted compile. And the reviewer's view of grunt
  attempts is rebuilt from a fail-closed allowlist of kernel- and
  daemon-controlled fields: attempt rows previously carried a `detail` string
  lifted verbatim from the Lean compiler log, which quotes the grunt's own
  proof body, giving an untrusted agent a free-text channel into a trusted
  one. Kernel-controlled fields pass through a bounded identifier filter so a
  smuggled newline cannot split the reviewer's table. (aab256cb)
- Cleanup gains real capabilities. Two new task kinds: **DeadCodeElim**
  (mechanical dead-line removal, gated on a scoped typecheck and a strict line
  decrease) and **ExtractShared** (cross-node deduplication into one shared
  helper, accepted on outcomes — at least two parents call it, every changed
  parent shrinks, and the net including the helper shrinks). **ExtractHelper**
  lets cleanup create nodes for the first time, moving a proof block out of an
  oversized node under the narrowest creating envelope the protocol has, and a
  new correspondence-repair route lets the reviewer answer corr blockers during
  Cleanup — which required splitting the acceptance predicate so cleanup work
  that legitimately produces something still to verify is acceptable at all.
  Bursts are batchable: kind-homogeneous multi-task batches (LintFix and
  ExtractHelper, up to six) amortize the per-burst cost, and batched
  ExtractHelper acceptance admits k helpers for k dispatched parents. The audit
  round ceiling rises from 2 to 10, each round still on an explicit reviewer
  request, with Pending tasks rendered ahead of resolved history so prompt
  truncation cannot hide them. (0694b940, 5e2cdf1c, f114462d)
- Cleanup also stops wedging and stops halting the supervisor on healthy work.
  Extraction helpers were caught between two claim rules that both required and
  forbade reporting a claim, and ExtractShared's single helper tripped a
  node-kind sweep whose exemption was ExtractHelper-only — either way three
  rejections latch `cleanup_force_done` and end the phase, so in practice the
  lane wedged on its first extraction. The per-node repair budget
  (`CLEANUP_REPAIR_MAX_ATTEMPTS`) is removed, having been the only thing that
  could manufacture a correspondence-Fail livelock; tasks whose target node was
  deleted become undispatchable and are dismissed; a transport-flagged
  Malformed response leaves its task Pending instead of burning it; consumers
  invalidated by a cleanup edit are re-certified before the validity preflight,
  which had been raising a fail-loud disagreement halt on essentially every
  substantive cleanup edit; and the checker and engine now share one acceptance
  predicate, so a pre-existing orphan no longer makes every cleanup burst
  unacceptable. Two false-acceptance holes are closed alongside: LintFix must
  show the named warning actually disappeared, against real pre-burst compiler
  diagnostics preserved across checker cache hits, and Substitution acceptance
  binds the declared replacement. The kernel also deletes target-unreachable
  nodes at the start of every Cleanup cycle, so leftovers from the layered
  statement-construction window cannot strand the phase with work it can never
  accept. (05f592b5, 8c0ce111, f3181d7c)
- Fresh runs no longer die at cycle 0 looking abandoned. A brand-new run could
  fail its first Start with `materialize-tablet-oleans failed … no
  configuration file with a supported extension`, because initial local-closure
  issuance runs before anything has synced the supervisor mirror, so the
  checker built in a workspace holding only a `Tablet/` subtree with no
  lakefile. No supervisor process survived and the viewer showed the project as
  `abandoned` with nothing on disk explaining why; the workspace is now synced
  before the first issuance, which covers direct launches as well as the create
  script. Two more ways a run could be killed from outside itself: burst tmux
  session names were derived from the runtime root's *parent* directory, so
  every run on a host got byte-identical names and one run's session sweep
  could destroy another live run's worker — names are now namespaced by run
  slug — and a worker that finished a burst but never wrote its `.done` marker
  wedged the wait loop forever, where the marker is now written for it once the
  raw artifact parses with a non-empty outcome and has been stable for two
  minutes (`TRELLIS_FORGOTTEN_DONE_RECOVERY_SECONDS`). (71789d19, 8991247c,
  78cefd35)
- Closure records and compiled artifacts stop lying about state. Records are
  now minted at every authoritative apply site and the opportunistic supervisor
  backfill is gone: a missing or stale record raises a typed error naming the
  node and the site that should have minted it, rather than being papered over
  until a later global audit treats it as fatal and takes the supervisor down.
  Invalidation follows the full reverse-dependency closure of an edit, closing
  a second crash of the same family. On the artifact side, a rewind laid
  checkpoint sources over oleans built from the abandoned line — and bare
  `lean` does no freshness check — so observations read pre-rewind bytes and a
  correctly-rewound run refused to load with fingerprints that looked
  divergent; orphaned artifacts are now purged, and the purge decides staleness
  by content hash rather than an mtime cutoff, because a burst restamps almost
  every source mtime and the mtime sweep deleted thousands of artifacts where
  only a handful were genuinely stale. The sidecar side of the same story:
  one transiently stale record anywhere could abort an unrelated grunt's apply,
  and a completed closure whose queue generation had been pruned died
  `not_queued` — in both cases discarding a compiling, sorry-free proof and
  charging the node an attempt. Infrastructure discards are no longer
  chargeable. (7f79642f, 5d684653, fe45f127)
- `supervisor_state.json` is written atomically and in a structurally-shared
  encoding. The file every reader loads — kernel recovery, the viewer, the
  manual rewind recipes — was written with a bare `write_text`, so readers
  could catch a flush mid-write and a checkpoint commit could capture a
  truncated blob; it is now mkstemp/fsync/rename like every other history
  artifact. It is also now emitted in the `trellis-shared-state/1` form, which
  on a real long run is about eight times smaller (tens of MB down to single
  digits) — the plain form was growing steadily toward GitHub's 100 MB
  per-file limit, past which a run cannot push its own history. Decoders
  shipped first, in Python, JavaScript and Rust, so both forms are readable
  forever; every write verifies its own round trip and raises rather than
  falling back, and a payload at or above 90 MiB is encoded regardless of
  configuration. `history.shared_state: false` is a pure rollback with no
  migration in either direction. One consequence worth knowing: an ad-hoc
  `jq .state` on the raw file no longer works. (a350f5a7, e3c46402, ec68601d)
- The cleanup audit can now tell a 34-minute node from a 3-second one. The
  checker measures wall time, peak RSS and olean size for every genuinely
  recompiled node and the kernel persists them as advisory-only records —
  advisory by construction, with a source-scan test enforcing that
  machine-dependent numbers never gate a decision — and those numbers reach
  the audit as a per-node payload on Audit and Cleanup-Review requests.
  Decomposition candidates are ranked by elaboration cost rather than length
  alone, cost is admitted as grounds for an extraction, and nodes sitting near
  their heartbeat budget are flagged. Peak RSS is now sampled host-side from
  `/proc`: `wait4` under the sandbox's `--unshare-pid` had been echoing the
  checker server's own RSS, so any RSS figure recorded before this release is
  wrong and is invalidated by a version bump rather than migrated. (2b4901b5,
  47669acf, c04ce5f8)
- `setup_repo.sh` is resumable, its destructive paths are guarded, and target
  selection is inspectable before you commit to it. A stage ledger plus
  `--resume` makes the thirteen setup stages re-enterable, the mathlib tar seed
  and the config JSONs are written atomically (an interrupted extract used to
  leave a sentinel vouching for a truncated tree, surfacing much later as
  mid-run import failures), every invocation input is pinned across a resume so
  dropping a flag can no longer silently unregister targets while exiting 0,
  wipe paths verify their preconditions, and concurrent setups take a lock. The
  bwrap preflight no longer materializes a repo skeleton beside the repo — or
  in whatever directory you happened to run from — on every `--reset`. Three
  new flags carry settled choices: `--targets-json` (explicit raw targets
  including line ranges), `--env-map ALIAS=CANONICAL`, and
  `--main-result-envs`, the last recorded in the config so later loads and
  add-targets agree; with no flag the generated config is byte-identical to
  before. New `scripts/resolve_paper_targets.py` is a standalone diagnostic
  scanner — also the engine behind the wizard's target page — that mirrors the
  kernel's block extraction line for line, explains with reason codes and
  fix-its every theorem-like block that did *not* become a candidate, and
  cross-checks itself against the real kernel, reporting drift instead of
  diverging quietly. Two genuine scanner bugs are fixed behind it: a
  `\begin{Theorem}` was never closed by its own `\end{Theorem}`, so
  capitalized-environment papers could silently swallow intervening blocks and
  inherit a swallowed theorem's label, and a braceless `\end{` inside e.g.
  `\verb` reintroduced the same swallow. (a6bc743b, 2f4adc09, 272ae921)
- Model configuration is no longer half-applied. The shipped template's
  `verification` block — its own model plus the correspondence, soundness and
  substantiveness agent pools — had never moved with the rest, and the
  create-time `--model`/`--effort` override rewrote only four hand-listed
  lanes, so a run configured for one model could dispatch most of its
  verifier bursts on a superseded one; verifier bursts log as role "reviewer",
  which is why a role tally showed nothing. The override now walks every
  model-bearing spec under the known roots, so a block added later is covered
  without editing a list, and the shipped templates default to
  `gpt-5.6-sol`/`xhigh` throughout. The inert `verification.provider`,
  `model`, `thinking_budget` and `max_context_tokens` scalars are dropped from
  the templates (still accepted on disk for live runs), `substantiveness_agents`
  is parsed on the Python side too, and the shipped templates now pin the
  `stuck_math_audit` lane explicitly — absent from a config it still falls back
  to `reviewer`, but changing `reviewer` alone no longer moves it. Known
  effort tiers are registered per provider as data, which is what feeds the
  wizard's per-role suggestions. (83aec185, 11109578, a3ab3951)
- Run-behaviour corrections a long run would feel. Theorem-stating workers no
  longer receive the proof-formalization protocol — all ten formalization
  fragments could arrive phase-unconditioned, and workers dutifully followed
  the wrong phase's instructions — and closedness is now judged through the
  dependency closure, so a proof resting on a sorry-bearing import no longer
  presents as closed. The cycle-1 planner's plan, written against an empty
  Tablet, no longer routes the run forever: the planner re-runs on a cadence
  while an initial plan is live in TheoremStating and the plan is retired at
  the phase boundary. A theorem-phase reviewer Continue whose only action is
  requesting Sound verifier nodes dispatches those verifiers at the next cycle
  instead of burning a worker turn first. And the stuck-math audit lane now
  receives the kernel's own per-node verdicts in its prompt and is refused the
  worker-only `check_node.sh` — audits reaching for a script their sandbox can
  never run had halted the supervisor outright, and with verdicts in hand they
  stop re-litigating lanes that already passed. Finally, the reviewer is no
  longer told to defer node-level correspondence behind paper-faithfulness. That
  instruction could deadlock: paper-faithfulness cannot clear until every target
  is covered, while correspondence dispatch is topological — one Fail near the
  root of the dependency graph defers every descendant, and any non-Pass node
  blocks the soundness lane outright. Observed on a real run: nine root-level
  correspondence failures left 99 of 248 nodes undispatchable and the soundness
  lane with no assessments at all. The NL-soundness deferral, which has no such
  feedback loop, is unchanged. (ac3eed26, b052a021, 4f43fbec, 3e28f153)
- Halting and resuming are one coherent surface. The viewer's two contradictory
  stop dialogs became one row that states both the reason and liveness, and
  Resume now *lifts* a halt marker — renaming it to a `.lifted-<ts>.json`
  sibling so the diagnostic outlives the resume — instead of relaunching into
  the same marker. `trellis_pause.sh resume` refuses while a marker is on disk
  and names it: `--clear-halt` lifts a system-feedback marker and relaunches in
  one step, `--clear-halt-any-kind` covers a checker-disagreement or
  unparseable marker, where soundness is the open question and the triage
  belongs at the marker first. A lift settles one instance; acknowledging a
  known-benign fingerprint (`ack_system_feedback`) is still the way to stop it
  halting on every recurrence. README §5 no longer tells you to `rm` the
  marker. (817bedf8)
- Viewer scale and readability. The Usage endpoint streams the event log
  instead of materializing it and then folds it incrementally per cycle file,
  turning a tab that could exhaust V8's heap every 45 seconds on a large run
  into a warm response in milliseconds; the progress walk content-addresses its
  Tablet reads, taking a full run's checkpoint walk from over an hour to
  seconds. Rendered Lean now links node names — resolved through the node's
  declared imports, so a link can never invent a reference — and proofs can be
  read side by side; `--emit-semantic-closure` writes what the exporter
  resolved so a cloned public release can rebuild its tablet viewer with no
  Lean toolchain installed. A project's runtime is resolved by its declared
  `repo_path` rather than directory naming, a browser launch can no longer
  delete a live run, audit bursts no longer duplicate in the chat dropdown and
  are labelled by kind, and live runs sort above inert ones. (5726989a,
  a8837334, 9ac72f73)
- Docs. `INSTALLATION.md` states the toolchain and mathlib pins as v4.33.0,
  says to put swap on the SSD, and documents the viewer's bind, Host allowlist,
  read-token and control-token behaviour along with the nginx rule the control
  subtree needs. README gains §11 on reference papers — registering auxiliary
  sources as grounding authority for cited results, at setup via `--reference`
  or on a live run via `add_reference_paper.sh` — and links `ADD_TARGETS.md`.
  Both README §10 and INSTALLATION now say plainly that the closure sidecar is
  optional and off by default, that each grunt costs a workspace clone and a
  concurrent Lean build, and that grunts buy a modest speedup and some token
  savings rather than a better chance of formalizing anything: leave them off
  unless the stated system requirements are comfortably exceeded. Re-registering
  a byte-identical reference paper is now idempotent rather than a hard error,
  so a resumed create job or a re-run command works; different bytes under the
  same id remain a hard stop. (d308c927, cfa856db, 86587818)

## v0.2.5 — 2026-08-10

- Grunts now run on the codex CLI, defaulting to `gpt-5.6-luna`; the
  sidecar's own HTTP chat/completions arm is retired. There is no separate
  API key — each grunt authenticates from a private `CODEX_HOME`, re-seeded
  from the operator's `~/.codex/auth.json` at every attempt launch, so a
  grunt credential failure can never unauthenticate the formalization loop.
  The grunt agent runs untrusted in a bwrap sandbox under a dedicated
  `grunt` role, and the harness owns the verdict end to end: the agent's
  own success claim is discarded, the candidate body is harvested from the
  node file, scanned (banned tokens, no new top-level declarations, the
  declared-name gate), and compiled by the harness — several demonstrated
  bypasses of that gate, including an arbitrary-code-execution and an
  axiom-forgery vector, were closed in the process. A grunt that fails to
  compile retries within its wall (two rounds, carrying the compile error);
  the gate a grunt faces is local closure, matching primary work.
- The grunt queue gains a kernel lane: a standing ranked list of every
  sidecar-eligible node, refilled at each cycle boundary, that the pool
  falls back to when the reviewer's lane is empty — a free grunt idles only
  when both lanes are exhausted, and the kernel tops the lane up when the
  pool sits idle. Ranking prefers fewest prior attempts, then non-sketch,
  then shortest proof; auto-dispatch stops refilling a node once the pool
  has failed it 5 times (`max_attempts`, counted per node content, so a
  repaired node regains its allowance). The reviewer's lane is untouched:
  queueing a node means "work this first", not "work this at all".
- Pause and resume: `scripts/trellis_pause.sh` (arm / disarm / status /
  resume) turns "stopped" into a durable, reason-carrying state in
  `<runtime>/pause_request.json` that survives the supervisor being down —
  the stop sentinel remains the fire-once trigger, but the state no longer
  lives in a killable process. Resume replays `<runtime>/launch_env.json`,
  the exact environment captured at every launch, instead of guessing one,
  and refuses if the source tree the run was launched from has moved. The
  viewer drives the same state machine, renders a paused run on its own
  attention tier, and arms a pause automatically in two cases: a human gate
  left open past `gate_park_after_minutes` (default 120), and a weekly
  budget "pause run at" floor (default 5% remaining) enforced server-side.
- Build-performance guidance: `BUILD_PERFORMANCE.md`, a canonical
  build/elaboration diagnosis reference, is rendered to agents; worker and
  reviewer prompts now weigh a different approach or decomposition against
  raising heartbeat budgets when a build runs long (a raised budget is
  named as a durable cost paid on every future rebuild), and the
  incremental checker's giant-node fallback lines advise the fix keyed by
  the actual signal.
- The mandatory-LastClean reviewer mandate ships disabled. The
  `cycles_since_clean` trigger counts every checkpoint carrying any open
  blocker, so one stubborn side-node blocker could mark 40 cycles of real
  progress as a failed repair narrative and force their discard. Operators
  who want the mandate set a positive `TRELLIS_CSC_LAST_CLEAN_THRESHOLD`;
  the reviewer sees the mandate fragment exactly when the kernel will
  enforce it.
- Viewer: the Usage page states what its numbers actually are, and the
  experimental modeled rollups and the grunts failed-attempts table are
  dropped from it.
- Viewer: an update-available banner. The server checks the public repo's
  changelog every six hours and, when a newer release exists, shows a calm
  dismissible notice (per-version dismissal; deliberately outside the
  attention ladder — nothing about the run needs a human). The check never
  raises a banner on a guess: transient fetch failures keep the last good
  answer, and `TRELLIS_VIEWER_NO_UPDATE_CHECK=1` disables the outbound
  request entirely. Served at `/api/update-check.json`.
- Docs: the viewer is documented as the primary operator interface — the
  place approval gates and `NeedInput` escalations surface — in README §4
  and INSTALLATION §2e, rather than as an optional extra; README §5 gains
  the mid-run upgrade recipe (pause → drain sidecar → rebuild in place →
  restart checker → resume).
- Docs: `INSTALLATION.md` gains a system-requirements section — 32 GB RAM
  minimum (48 GB recommended), SSD strongly recommended, what needs root and
  how to proceed without it, and macOS-as-VM-host guidance — including which
  distros run the sandbox with no root at any point. **Ubuntu 26.04 LTS is
  the new recommended platform**: Trellis is validated end-to-end on it with
  zero root setup, via the shipped `bwrap-userns-restrict` profile
  (packaged `bwrap` only). Also root-free: Ubuntu 25.04+, Ubuntu 22.04,
  Mint 22 / Pop!_OS 24.04, WSL2, and the Debian/Fedora/RHEL/openSUSE/Arch
  families. Ubuntu 23.10 through 24.10 is the blocked island and keeps
  needing the one-time root sysctl.

## v0.2.4 — 2026-07-29

- A verifier revisit prompt now carries only the findings for the node it is
  verifying. The Sound and per-node Paper requests copied the whole stored
  previous-findings map into every request, and the next target is chosen
  independently of it, so a request routinely arrived framed as a revisit
  carrying the *preceding* node's finding — telling the verifier it had found
  an unsupported step in a proof it had never read. Both lanes now intersect
  that map with the request's own verify set, as Correspondence already did,
  and the revisit fragment is selected from the filtered map. Stored verdicts
  were never affected: a lane whose payload node disagrees with the request is
  rejected before it can be written.
- Reviewers can return a substantiveness Fail to the verifier during proof
  formalization. Substantiveness clause 2 reads a node against the content of
  every node importing it, while the fingerprint that re-opens the lane is
  node-local, so the repair the reviewer is directed to make — an edit to the
  importer — left the failing node's verdict stuck. Naming the blocker in
  `reset_blocker_ids` returns the node to the verifier frontier; the blocker
  itself is still retired only by a verifier pass. A Fail that a reset cannot
  move, such as one derived from a rejected deviation claim, is no longer
  offered in any phase.
- `memory_challenges` is advertised on the worker and review contracts when the
  run has active process memory. The channel was described to both roles and
  implemented end to end, but never named in the contract, so a role that found
  its evidence contradicting an entry had no field in which to say so.

## v0.2.3 — 2026-07-27

- Closure sidecar ("grunts"): a reviewer-queued pool of Lean-specialized models
  works beside the main loop, each trying to close one open proof node. A grunt
  writes the proof body only (below `-- BODY`), and a closure lands only through
  the full kernel apply sequence, complete and checker-passing; `closure_provenance`
  attributes each node to `worker` or `sidecar`. Inert unless a `sidecar` block
  in `trellis.config.json` enables it. Operator docs: README §10 and
  `SIDECAR_OPERATIONS.md`; the viewer gains a **Grunts** tab for the pool, queue,
  and attempt history.
- Sidecar liveness: one rule for the whole system (`trellis/sidecar/health.py`) —
  the daemon's own pid lock, corroborated by an exact `-m trellis.sidecar` match
  on `/proc/<pid>/cmdline` so an attempt child is never mistaken for the manager
  — surfaced as `scripts/trellis_sidecar.sh status <root>` with operator exit
  codes. `status.json` gains pid, phase, poll interval, transport suspension and
  last-pass verdict, and the reviewer's pool claims are gated on it, so a dead,
  suspended or still-bootstrapping daemon no longer reads as spare capacity.
- Drain and adopt: `<runtime>/sidecar/drain` stops assignment and exits at once,
  leaving in-flight attempts running; `slots.json` journals every assignment and
  the next daemon adopts those children, reaping whatever finished during the
  gap before its first assignment decision. Adoptions and kills are gated on
  `/proc/<pid>/cmdline` carrying the attempt id, so a zombie reads dead and a
  recycled pid is never signalled. The daemon clears both sentinels before
  bootstrap, so a forgotten `stop` file cannot kill the attempts of the next run.
- Queue entries retire on their own: a grunt gets one attempt per entry
  generation, and the spent generations are now reported to the kernel at the
  inter-cycle boundary, which removes the matching entries. The reviewer's
  capacity view no longer counts dead work.
- Sidecar retrieval: the driver's tablet and mathlib searches route through
  ripgrep with a fixed automaton engine and a subprocess timeout. The previous
  in-process regex path checked its deadline only between files, so a
  model-supplied pattern could run unbounded in an unsandboxed process; symlink
  containment now sits at the file open, and a malformed query returns a tool
  error instead of ending the attempt.
- Viewer: every "stopped, waiting for a human" state renders on one severity
  ladder. A halt marker, the fail-loud `NeedInput` gate (shown with its
  escalation reason and protocol-state source) and a routine advance or
  re-approval gate are now distinguishable without reading the text, and at a
  `NeedInput` gate sending input is the primary action while the empty approve
  is demoted. A runtime that reports no gate kind falls back to the previous
  reviewer-decision heuristic.

## v0.2.2 — 2026-07-24

- System feedback no longer halts the run by default. A burst that returns a
  non-empty `system_feedback` string now appends a record to
  `<runtime>/system_feedback_log.jsonl` and the run continues. Halting on
  system feedback is opt-in: set `system_feedback_halt: true` in
  `trellis.config.json`, or the environment variable
  `TRELLIS_SYSTEM_FEEDBACK_HALT` (which takes precedence). A halt marker left
  from a prior run is still honored regardless of the setting. The viewer
  exposes the recent feedback log at `/api/system-feedback.json`.
- Audit planning: the process-rules reference now states that Sound
  verification (including reviewer-requested re-verification) waits until every
  statement lane is clear, so audit plans sequence statement-lane repairs
  before the Sound certifications that depend on them.

## v0.2.1 — 2026-07-22

- Initial planner: every fresh run's first cycle dispatches a planning burst
  (math and both PV goal modes) that reads the manuscript and configured
  targets and writes an advisory construction plan (report + worker tasks);
  the reviewer works and dismisses the plan, workers keep decomposition
  authority.
- Coverage re-planning: while any configured paper target lacks a covering
  node, the planner re-runs on a fixed cycle cadence, assessing the live
  plan and superseding it (dismissal trail preserved). Dead once all targets
  are covered.
- Process rules: `PROCESS_RULES.md` — a capability-and-mechanism reference
  (per-role legality envelopes plus the full mechanism inventory) installed
  at the tablet-repo root by both setup scripts and consulted by every
  audit-lane scenario via a pointer fragment.
- Add-targets mode: `add_paper_targets` revives a Complete run into
  RevisionStating to state additional targets from the same paper; existing
  approvals stay byte-identical. Operator doc: `ADD_TARGETS.md`.
- Reference papers: an operator-registered registry of auxiliary papers
  (`paper/refs/<id>.tex`) that act as grounding authority for cited external
  results; workers claim them per node and the substantiveness lane verifies
  against the claimed text. Operator doc: `REFERENCE_PAPERS.md`.
- Orphan construction window: while any configured paper target has empty
  coverage, same-burst orphan rejection is waived so the statement DAG can
  be built up in layers; all-covered behavior unchanged.
- Soundness dispatch: no longer deferred until all paper targets are
  covered — a node's prose proof is verified once its own and its cited
  statements pass correspondence and substantiveness. A routed worker task
  always wins the cycle-start slot; soundness rides after the worker, one
  auto-dispatch per cycle, and kernel-scheduled soundness results are marked
  for the reviewer.
- Revision mode: statement editability is computed dynamically
  (present minus frozen), so nodes created mid-revision are repairable.
- Viewer: tablet-snapshot downloads include `paper/refs/`; README generator
  reports per-goal Decide polarity and the full assumption list.
- Setup: `normalize_paper_envs.py --map alias=canonical` handles papers
  whose `\newtheorem` declarations carry no usable title.

## v0.2.0 — 2026-07-04

- Process memory: a git-tracked, run-authored knowledge store
  (`process-memory/` in the tablet repo). Audits record refuted routes,
  constraints, and interface decisions via structured `memory_operations`
  (add / supersede / retire, tombstoned, never deleted); workers and
  reviewers file challenges that the next audit must adjudicate; entries
  render into every role's prompt. LastClean rewinds carry memory forward by
  default (`preserve_process_memory`), operator git rewinds keep plain-git
  semantics, and entries survive kernel worktree restores in rejection
  cycles. One-shot migration script for existing runs.
- Verifier findings: an `UNSOUND`/`STRUCTURAL` soundness rejection now
  enumerates every independent blocking gap, numbered, so one repair burst
  can address them all.
- Worker prompts: the active node's claimed deviations are listed with their
  `reference/` files and a read-before-working directive; the pre-`valid`
  self-audit gains a citation-surface table (each outside fact mapped to the
  cited node's statement clause).
- Retry context: an auto-retry no longer inherits the reviewer's
  fresh-context decision, so the failed burst's scratch handoff survives
  into the retry.
- Olean freshness: content-hash olean staleness detection end-to-end
  (mtime never consulted), with the olean hash folded into the kernel
  result-cache keys.
- Cleanup phase: active-node relegalization after deletions, closure
  revalidation after worker deltas, final-target deletion support, scoped
  final validation, structural-hash protection, and same-burst orphan
  deletions.
- Soundness lane: uncomputable empty passes reopen correctly; fingerprints
  backfilled for empty TeX model refs; completed proof formalization
  auto-advances.
- Program-verification (under-model) workflow: assumption slices with
  auditor adjudication, misroute guard, checkpoint workflow fixes, and a
  reproducible `examples/pv_dec2flt` seed + runbook.
- Worker handoff: the `last_invalid` WIP snapshot is also captured when a
  `valid` response is rejected by a kernel rule at apply time, so the retry
  prompt's promised snapshot always exists; orphan-cleanup attribution and
  the orphan gate now name only newly-created orphans.
- Assumptions framework (under-model): claim classes, conditioning check,
  mid-phase domain gate, assumptions-lane verdicts mirrored into reviewer
  evidence, and failed authoring bursts handed back to the enact loop.
- Operations: `trellis.sh` runs a prebuilt kernel binary when
  `TRELLIS_TRELLIS_KERNEL_CMD` is set (release-binary step loop); worker
  model A/B switch; viewer fixes (stale-cycle tag mixing, progress.json
  truncation on large repos, slimmer wire payload).

## v0.1.1 — 2026-06-12

- Challenge targets: benchmark mode for prescribed-statement problems
  (lean-eval). Deterministic importer from a problem download, kernel-enforced
  byte-exact coverage beside paper targets, and a submission exporter that
  replays the benchmark's comparator. Contract v38.
- Clearer kernel diagnostics: every review-legality rejection branch is named;
  acceptance skip notes, signature-drift, import-cycle, and empty
  next-active messages state their cause and remedy.
- FILESPEC: node auxiliaries are node-private (factor out to share a fact);
  heartbeat option placement specified by purpose.
- Audit roles: the audit's charge is to find the work or repair that puts a
  formalization on a closing route, not to weaken the verification regime.
- Per-burst tmux sessions are torn down at burst completion (previously only
  the burst window was killed, leaking an idle session per burst).

## v0.1.0

Initial public release.

First source-available release of Trellis, an agent-driven pipeline for
formalizing mathematics in Lean 4. This release establishes the public
baseline; subsequent entries will record notable changes against it.

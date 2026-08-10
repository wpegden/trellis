# Operating the closure sidecar

The closure sidecar is a separate daemon that runs alongside a Trellis run and
tries to close queued nodes with a cheap model (the codex CLI running
`gpt-5.6-luna` by default), in isolated workspaces, without touching the
supervisor's own loop. README §10 covers what it is and how to
turn it on. This document covers running it: how to tell whether it is alive,
how to start and stop it safely, and what the failure modes look like from the
outside.

Everything here comes from incidents. Where a rule looks fussy, it is because
the obvious alternative destroyed work.

---

## 1. Process model

Three different argv shapes exist, and **two of them overlap by name**:

| What | argv | Lifetime |
| --- | --- | --- |
| launcher | `bash scripts/trellis_sidecar.sh <runtime_root> --repo <repo>` | execs into the manager |
| **manager** (the daemon) | `python3 -m trellis.sidecar <runtime_root> --repo <repo>` | the whole session |
| **attempt child** (one per in-flight attempt) | `python3 -m trellis.sidecar.attempt <runtime_root> --grunt K --node N …` | one attempt |

The manager holds the pool, the queue bookkeeping and the spool. Each attempt
child runs one node's proof search in its own session (`start_new_session=True`,
so pgid == pid) inside `<runtime_root>/sidecar/grunts/<K>/repo`.

**Never identify either process by name.** `trellis.sidecar.attempt` begins with
`trellis.sidecar`, so any prefix or substring match over argv counts the
children as the manager. Attempt children routinely OUTLIVE the manager — that
is the drain design, not a bug — so "a process matching the name exists" is not
evidence that the manager is up. Identity is:

* the **manager** is the process holding `flock` on
  `<runtime_root>/sidecar/daemon.pid`, and its argv has `-m` immediately
  followed by exactly `trellis.sidecar`;
* an **attempt child** is a process whose argv has `-m` immediately followed by
  exactly `trellis.sidecar.attempt` and which names its own `--attempt-id`.

`trellis/sidecar/adopt.py` and `trellis/sidecar/health.py` implement exactly
those two predicates. Use them rather than writing a third.

### Files the daemon owns

Everything under `<runtime_root>/sidecar/`:

| Path | Meaning |
| --- | --- |
| `daemon.pid` | singleton lock (`flock`), held for the manager's lifetime |
| `status.json` | the daemon's heartbeat + pool/attempt surface (rewritten every pass) |
| `candidates.json` | **kernel → daemon**: the export the daemon reads (both lanes); the daemon never writes it |
| `reviewer_candidates.json` | **kernel → reviewer**: the same export with `kernel_queue` redacted; the only export file the reviewer sandbox binds |
| `attempted.json` | one row per spent `(node, entry_seq)` generation |
| `slots.json` | the in-flight assignment journal that makes adoption possible |
| `manager_cursor.json` | last-seen export cycle (rewind detection) |
| `ledger.jsonl` | per-attempt spend/telemetry, append-only |
| `grunts/<K>/` | one isolated workspace + result/log files per grunt |
| `spool/` | the two-way handshake with the kernel (§6) |
| `stop`, `drain` | sentinels (§4) |

---

## 2. Is it alive?

```bash
scripts/trellis_sidecar.sh status <runtime_root>
scripts/trellis_sidecar.sh status <runtime_root> --json
# equivalently:
python3 -m trellis.sidecar.health <runtime_root>
```

It answers from the pid lock (not from a process name, and not from the age of
a file the daemon may have stopped writing), corroborates the pid against
`/proc/<pid>/cmdline`, and prints the pool, the live attempt processes, the
sentinels, the spool lane counts and the export age.

Exit codes — scripts may key on these:

| Code | Meaning |
| --- | --- |
| 0 | running, and `status.json` is fresh |
| 1 | not running — reported **before** staleness or sentinels are considered, so a dead daemon is 1 whatever else is on disk |
| 2 | degraded: running, but `status.json` is stale or a `stop`/`drain` sentinel is pending |
| 3 | no `<runtime_root>/sidecar/` directory — the daemon has never run here |
| 4 | undeterminable (e.g. the pid file cannot be read) |
| 64 | usage error |

Two readings that look alarming and are not:

* **not running + live attempt processes** — that is a drain window. The
  manager exited on purpose and its children are still working; the next
  manager adopts them. `status` prints the live attempt processes for exactly
  this reason.
* **running + stale status** — the daemon is up but has not completed a pass
  recently. During `startup()` that is normal: bootstrapping N grunt
  workspaces (a clone plus an olean copy each) takes minutes, and the file says
  `"phase": "bootstrapping"` throughout.

### DO NOT

* **`pgrep -f trellis.sidecar`** — wrong twice over. `pgrep` patterns are
  **ERE**, so the `.` matches any character and `trellis_sidecar.sh` matches
  too; and the pattern matches every `trellis.sidecar.attempt` child. A dead
  manager with live children reads UP. That is precisely how a 15-minute
  outage went unnoticed.
* **`pkill -f trellis.sidecar` / `pkill -f sidecar`** — the same pattern, now
  aimed at a signal, and unscoped across runs: it kills every run's daemon and
  every run's attempt children on the box. This killed a production daemon.
  If you must signal a specific process, get its pid from `status` and signal
  **that pid only**.
* **Reading `status.json` and believing its contents** without checking the
  daemon's liveness first. A dead daemon and a healthy idle pool leave
  identical files. (The reviewer prompt block now gates on this; a human at a
  terminal should use `status`.)
* **Judging liveness by `daemon.pid`'s contents.** The pid in that file is
  stale the moment the daemon dies. Only the `flock` on it means anything.

---

## 3. Starting it

```bash
tmux -L trellis new-session -d -s "trellis-sidecar-<slug>" \
    "scripts/trellis_sidecar.sh <runtime_root> --repo <live_repo>"
```

* The session name **`trellis-sidecar-<slug>` is load-bearing.**
  `scripts/restart_configured_run.sh` kills every session on the shared
  `tmux -L trellis` socket whose name *starts with* `trellis-<namespace>-`,
  where `<namespace>` is the runtime root's directory name with a trailing
  `-runtime` removed, non-alphanumerics folded to `-`, truncated to 48 chars
  (a `startswith` test, not a glob). A session named `trellis-<slug>-sidecar`
  matches that prefix and dies with the next run restart, killing the daemon
  mid-attempt; `trellis-sidecar-<slug>` does not, and survives.
* The daemon is **inert** (exits 0, prints a note) unless `trellis.config.json`
  carries an enabled `sidecar` block. That edit must be **committed** to the run
  repo: a checkpoint `git reset --hard` restores tracked config, so an
  uncommitted enablement silently reverts.
* Refusals are loud and exit 2: another daemon already holds the lock
  (`SingletonError`), the sandbox is configured but `bwrap` is missing, or
  `/proc` is unreadable while `slots.json` has rows.
* Only ONE manager per runtime root. The lock enforces it; do not work around
  it.

---

## 4. Stopping and restarting

Two sentinels, and the difference is what happens to work in flight.

```bash
touch <runtime_root>/sidecar/drain    # restart/redeploy path
touch <runtime_root>/sidecar/stop     # hard stop
```

| | `drain` | `stop` |
| --- | --- | --- |
| new assignments | stop | stop |
| in-flight attempts | **keep running**, unsupervised | `killpg`'d |
| bookkeeping | **nothing recorded** — they have not ended | a `cancelled` row each |
| queue generations | **untouched** | **SPENT and published** |
| next daemon | adopts them off `slots.json` | nothing to adopt |

`stop` wins when both are present.

**`stop` spends the queue generations of everything it kills.** Each killed
attempt gets an attempted-set row and a published spent-generation outcome, so
the kernel retires those queue entries: the reviewer must re-add the node to get
another attempt. An attempt 80 minutes into a 90-minute wall is thrown away and
charged. Use `drain` for anything routine — a kernel redeploy, a daemon code
change, a host maintenance window — and keep `stop` for "stop spending money
now".

A drain leaves live children running the code they were STARTED with, so:
deploy into a new worktree and relaunch from there rather than editing the tree
underneath them, and never relaunch a build that predates adoption while
attempts are in flight (pre-adoption code cannot see them and re-attempts their
generations).

### The stale-sentinel trap

A sentinel is consumed by the daemon on its way out. If no daemon is running
when you write one — a `touch .../stop` against a daemon that had already died,
or an exit that never reached the unlink — **the file stays on disk**.

Historically, the next daemon then did the worst possible thing: `startup()`
runs first and ADOPTS every live orphan into its slots, and only afterwards
does the loop see the sentinel and `killpg` all of them, spending and
publishing their generations. A file somebody forgot to delete destroyed live
proof work, and specifically the longest-running attempts.

The daemon now clears both sentinels immediately after taking the pid lock and
**before** `startup()`, logging what it removed: a sentinel means "stop the
daemon that is running now", so one that predates the daemon means nothing.
Operationally:

* after any unclean daemon exit, run `status` — **a `SENTINEL:` line at all**
  means one is on disk. Read it together with the daemon line, because the exit
  code differs by which case you are in:
  * **exit 1** + `SENTINEL:` — the daemon is gone and a sentinel is still
    there. This is the forgotten one. (`exit_code` reports not-running before
    it ever considers staleness or sentinels, so a dead daemon is 1 regardless
    of what else is true.)
  * **exit 2** + `SENTINEL:` — the daemon is alive and a stop/drain is IN
    PROGRESS. Nothing to clean up; wait for it.
* to stop a daemon that is running, write the sentinel and confirm it is gone
  afterwards;
* if you wrote a sentinel and nothing happened, the daemon was already dead —
  delete the file rather than leaving it for the next start.

---

## 5. What "idle" can mean

`status.json` carries `last_pass.status`, the verdict of the daemon's most
recent pass. An idle-looking pool is one of these, and they are not
interchangeable:

| `last_pass.status` | Meaning |
| --- | --- |
| `assigned` | it just started at least one attempt |
| `idle` | nothing assignable this pass (everything spent, in flight, or blocked) |
| `queue_empty` | BOTH lanes are empty — the pool has nothing to do |
| `window_shut` | the kernel's export says the sidecar window is closed |
| `suspended` | repeated transport failures; no assignments for 30 minutes (`suspended_until_ms`) |
| `no_api_key` | legacy: a non-codex `model.provider` is configured and its key file/variable produced nothing (the codex default authenticates from `CODEX_HOME` and never hits this) |
| `stale_export` | `candidates.json` is older than the configured window: the supervisor is down or wedged, and the daemon refuses to act on a frozen view |
| `budget_cap` | a configured token cap is tripped (caps are off by default) |
| `journal_error` | **the daemon cannot write `slots.json`.** It reaps and cancels but refuses every new assignment, forever, until that file is writable — an attempt it cannot journal is one the next daemon cannot adopt. Check disk space and permissions on `<runtime_root>/sidecar/`. |

### The kernel queue: a standing ranked list the pool falls back to

The grunt queue has TWO lanes over one kernel list:

* the **reviewer lane** (`queue` in the export) — what the reviewer adds,
  removes, and sees;
* the **kernel lane** (`kernel_queue`) — a full ranked list of every eligible
  node, refilled at every boundary.

The daemon drains the reviewer lane first and falls back to the kernel lane, so
a free grunt idles only when BOTH are exhausted. This is what fixed the pool's
5.1% utilisation: the old rule refilled an EMPTY queue to the idle pool's
width, which the pool drained in one attempt-length and could not refill until
the next boundary, 20 to 150 minutes later.

Which nodes: sidecar-eligible now, not already queued in either lane, and not
one whose generation was spent earlier in the same cycle. Ranked by fewest
prior grunt attempts (`attempted.json`), then non-sketch before sketch — a
`SKETCH:` proof counts as longer than every real one — then shortest NL proof,
then node id. Fewest-attempts-first is also the anti-starvation rule: a node
that fails is re-minted a cycle later carrying one more attempt and sinks below
everything tried less often.

Entries are RESIDENT: a boundary mints a generation only for a node the lane
does not already hold, and the entry keeps that generation until it is spent
(then the outcome ingest retires it and the node returns to the ranking one
cycle later). A dead daemon changes nothing — the lane simply sits full.

The reviewer is not shown the kernel lane. It reads
`reviewer_candidates.json`, the same export with `kernel_queue` redacted, and
that is also the only export file its sandbox binds. A reviewer `add` naming a
node the kernel lane holds is legal and PROMOTES it (same generation, moved to
the back of the reviewer lane); a `remove` naming one is rejected as "not in
the sidecar queue" — the reviewer manages its own lane, and the refill would
put a kernel entry straight back anyway. Queueing a node therefore means "work
this FIRST", not "work this at all".

Off switches (config edit, no rebuild, and like every `sidecar` key they must
be COMMITTED to the run repo's `trellis.config.json`):

```json
"sidecar": { "auto_dispatch": {
  "enabled": false,
  "kernel_queue_max": 512,
  "max_attempts": 0
} }
```

`kernel_queue_max` is a safety bound on the resident lane, not a scheduling
parameter — the default sits well above any real run's eligible population.
`max_attempts` (legacy spelling `max_attempts_per_node`; the new key wins when
both are present) stops refilling a node once the pool has failed it that many
times. It DEFAULTS ON at 5 — measured whole-run, attempts #1–5 produced every
grunt closure ever landed while #6–18 went 0-for-297 — and `0` turns it off.
Attempts are counted per node CONTENT (the daemon stamps `node_file_sha256`
into each `attempted.json` row), so repairing a node restores its allowance;
rows recorded before the stamping always count. Kernel lane only, and a supply
gate on new mints — resident entries and in-flight attempts are untouched, and
the reviewer lane never sees it. The supervisor logs when it acts:
`trellis sidecar: attempt cap (5) withheld 3 node(s) from the kernel lane: ...`

The supervisor logs each refill:
`trellis sidecar: kernel queue refilled with 12 node(s) (lane held 96): ...`

---

## 6. Spool lanes

`<runtime_root>/sidecar/spool/` is the two-way handshake with the kernel. The
ownership direction is the invariant: **the daemon writes only into `pending/`
and `outcomes/`, and only by atomic rename out of `inflight/`; the kernel alone
moves files out of those two.**

| Lane | Direction | Contents |
| --- | --- | --- |
| `pending/` | daemon → kernel | a published closure, awaiting ingest |
| `claimed/` | kernel-private | mid-boundary processing |
| `applied/` | kernel → daemon | closure accepted (verdict appended) |
| `rejected/` | kernel → daemon | closure refused (verdict appended) |
| `outcomes/` | daemon → kernel | a SPENT generation that did not close the node |
| `claimed_outcomes/` | kernel-private | mid-boundary processing |
| `outcomes_consumed/` | kernel → daemon | spent-generation report ingested |
| `inflight/` | daemon-private | a record under construction |
| `abandoned/` | daemon-private | `inflight/` leftovers from a crash or a cancel |

The two daemon → kernel lanes carry opposite news about the same queue entry:
`pending/` says "this generation produced a proof, close the node"; `outcomes/`
says "this generation had its one attempt and did not close the node, retire
the entry". A success is deliberately never published to `outcomes/` — expiring
its entry would make the kernel reject its own closure as `not_queued`.

Files piling up in `outcomes/` mean the kernel is not consuming them (a stopped
supervisor, usually). Files in `abandoned/` are forensic only.

---

## 7. Troubleshooting

| Symptom | Likely cause | Check | Action |
| --- | --- | --- | --- |
| Reviewer says grunts are idle but nothing runs | daemon dead; frozen `status.json` | `status` → exit 1 | restart it (§3) |
| `status` exit 1, live attempt processes listed | drain window | `status` process list | wait, or start the daemon — it adopts them |
| `status` exit **1**, `SENTINEL:` line | forgotten `stop`/`drain` (daemon already gone) | `ls <runtime>/sidecar/{stop,drain}` | delete it if you did not write it |
| `status` exit **2**, `SENTINEL:` line | a stop/drain is in progress right now | the daemon line says RUNNING | wait for the exit; nothing to clean up |
| Daemon up, nothing ever assigned | see §5 | `last_pass.status` in `status.json` | fix the named cause |
| Daemon up, `last_pass: journal_error` | `slots.json` unwritable | `df -h`, permissions on `<runtime>/sidecar/` | free space / fix ownership; it recovers on its own |
| Daemon up, `last_pass: stale_export` | supervisor down or wedged | export age in `status` | fix the supervisor; the daemon resumes by itself |
| Daemon up, `SUSPENDED` | 3 consecutive transport failures | `ledger.jsonl` tail, `suspended_until_ms` | check codex auth (`~/.codex/auth.json` is re-seeded into each grunt's `CODEX_HOME` at attempt launch — re-authenticate `codex` on the host if it is dead); it retries after 30 min |
| Queue entry stays SPENT across cycles, daemon healthy | usually a report still queued, not a fault | `spent with no publication marker` + the `outcomes=` count in `status` | if `outcomes/` is non-empty the kernel just has not consumed it — wait a boundary. Only if it is empty is the report genuinely lost: remove the entry as reviewer, and **never** hand-write a spool record |
| Same, but the daemon is down | normal — the daemon is what retires them | `status` | none |
| Daemon refuses to start, "already running" | another manager holds the lock | `status` → pid | do not force it; stop the other one properly |
| Daemon refuses to start, `/proc` unreadable | fail-closed adoption guard | `slots.json` row count | fix `/proc`; freeing those slots would double-attempt their generations |
| Two attempts on one grunt index | duplicate adoption (should self-heal) | daemon log | the daemon keeps the older and cancels the rest |

`spent_without_outcome` counts attempt rows with **no publication marker**,
which is not the same as a broken pipeline. Three things produce it, and only
the last needs a human:

* the row predates the `outcome_published` marker. Such rows never acquire one
  and read this way until their entries cycle out of the queue — expect a small
  standing count immediately after this version is deployed;
* the outcome is published and merely unconsumed: the record is sitting in
  `outcomes/` waiting for the kernel's next boundary;
* the publication genuinely failed (the spool write is best-effort and swallows
  its errors).

The `outcomes=` lane count in `status` separates the second from the third.

It is **reported only** and never repaired automatically. Republishing was
designed and cut: `sidecar_queue_seq` lives in `ProtocolState`, so a rewind the
daemon did not witness reuses `entry_seq` values, and a republished outcome
would expire a live, never-attempted entry.

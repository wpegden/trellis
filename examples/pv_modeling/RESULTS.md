# Prototype evaluation, 2026-09-07

Every live role used `gpt-5.6-terra`, high effort, through Trellis's existing
Codex executor and bubblewrap sandbox. No suggested repairs were supplied to
the end-to-end runs. Prompts and infrastructure were refined during prototype
development; these are exploratory runs, not a fixed-version benchmark.

The first packed-counter run completed with unchanged Rust and one paired
definition, `CounterView`, interpreting the signed low byte and high field.
Both goals remained targeted, including the false strict-increase claim.
Cargo, Charon, Aeneas, and Lean succeeded. Its 16 completed role calls covered
reviewer, worker, audit, correspondence, substantiveness, and goal-faithfulness.
The run and handoff are at `/tmp/trellis-modeling-counter-v1`.

A repeat completed after a successful pause/resume check. It produced three
definitions, including `tick_readings`, a predicate assembling the reading
goal from the extracted function and two existing interpretations. Its
substantiveness lane accepted that unnecessary authoring. Treat the reported
`ready` status in `/tmp/trellis-modeling-counter-v2` as an observed failure of
the initial minimality review.

The five-goal source test at `/tmp/trellis-modeling-dec2flt-v2` reached a clean
Cargo/Charon/Aeneas/Lean check after extensive Rust rewriting. Its adaptation
audit accepted the candidate. Independent inspection found the native-float
fast branch replaced with another conversion path; a separate debug-build
comparison also found that original `parse_number` panics on an empty slice
while the replacement returns `None`. Charon's original LLBC retains the
enabled assertion. The driver was stopped after the unsupported audit
approval, before a reviewed model handoff. The candidate was not hand-fixed.
The independent comparison is at `/tmp/trellis-modeling-differential`.

These failures prompted two general revisions: phase-specific criteria for
substantiveness and adaptation auditing, and smaller independent-review
contexts that omit other agents' endorsements. Fresh isolated Terra reviews
of the same candidate artifacts then returned `revise`:

- `/tmp/trellis-modeling-probe-substantiveness`: identified `tick_readings` as
  theorem-stating work and requested its removal.
- `/tmp/trellis-modeling-probe-audit`: rejected unsupported branch, domain,
  parser, and decimal-shift substitutions and requested correspondence evidence
  or restoration of equivalent control flow.

The probes received the source, complete goals, candidate artifacts, and fresh
tool observations, without the independently discovered defects or expected
verdicts. They exercise individual roles; they do not constitute another
end-to-end run of the revised process.

Mechanical validation: 19 prototype tests and 27 existing sandbox tests pass.
Separate Lean checks validated both arrow-style and bound-argument axiom
replacement, including reduction through a generated caller. Tests cover
stage gates, goal reviews with an empty node inventory, stale results,
declines, reopening, closed-stage edits, interrupted work, and Markdown goal
inventory coverage.

The prototype demonstrates the workflow and preserves useful failure evidence.
Robust autonomous control of adaptation scope remains to be established in
further complete runs. Production theorem-stating dispatch is not integrated.

## Module-aware tooling update

The update in `${TRELLIS_ROOT:-/path/to/trellis}/src/trellis-modeling-next` was developed separately
while `/tmp/trellis-modeling-dec2flt-v3` continued with its original prototype.
It adds explicit goal collections, grouped semantic nodes, native Aeneas module
and external-definition support, source-mapped inspection, stage/module caching,
source-copy comparisons, and focused role context. The shared stage criteria now
identify extraction as adaptation's mechanical gate and composed Lean checking
as part of model completion.

Validation: 57 tests pass, including cache ownership and invalidation, all-module
Lean gating, elaboration-placeholder rejection, source mapping, grouped native
external modules, and comparison timeouts. Real tool checks at
`/tmp/trellis-modeling-next-smoke` pass for single-file and split-file extraction
and for a node containing multiple definitions. Adding that node reused Cargo,
Charon, Aeneas, and the unchanged generated Lean modules. A separate real check at
`/tmp/trellis-modeling-next-native-external` compiled an authored ASCII predicate
through Aeneas's native external-module interface. These are tooling tests, not
claims of autonomous semantic acceptance. The current full Terra run has not
been migrated to this implementation.

The full `/tmp/trellis-modeling-dec2flt-v3` run subsequently ended after 24 role
calls with audit-approved termination and no model handoff. Independent
inspection found a decimal-shift regression introduced during adaptation, which
never received approval. The run's diagnosis of its float-related Lean failures
was also unsupported: its final operation-dictionary probe compiles after adding
only the missing `F32` and `F64` type aliases. This diagnostic repair leaves
external-operation semantics unresolved. Evidence is retained in
`/tmp/trellis-modeling-dec2flt-v3-evaluation`.

The updated packed-counter run at `/tmp/trellis-modeling-next-live` passed setup
and adaptation with unchanged source, then strayed into theorem statements and
proofs before verification. It was stopped at role 10 after diagnosing the prompt
design. The goal-assembly boundary was visible only to substantiveness reviewers;
the worker's broad remaining-work summary fed later proof work back to the
reviewer. Full delivered-context inspection also found that the new lane names
silently selected stock Codex base instructions. Notes, original prompts, and
the initial model are retained in `/tmp/trellis-modeling-next-live-evaluation`.

Shared scope, reviewer readiness criteria, worker reporting, role-instruction
placement, and base-instruction routing were corrected. The next all-Terra run,
`/tmp/trellis-modeling-next-live-v2`, reached verification without assigning
proofs. Full verifier-prompt inspection exposed accidental inclusion of the
scheduler's remaining queue and repeated reviewer endorsements. Those inputs
were removed and the run was stopped during role 12.

A further run exercised the corrected prompt assembly and exposed the need to
stop the driver before its agent: killing only the agent could trigger a
transport retry. Driver-first cancellation was verified live on
`/tmp/trellis-modeling-next-live-v3`. Prompt context now reuses Trellis's
collection truncation and adds text and total-preview bounds, with full role
context retained on disk. A large-context rendering check was recorded in that
run's evaluation directory. No additional repository unit tests were authored.

The fresh all-Terra run at `/tmp/trellis-modeling-next-live-v4` included these
updates, but its reviewer again treated an empty node inventory as a Modeling
gap and requested goal proofs. It was stopped at role 8. Complete-prompt review
identified an ambiguous readiness criterion and earlier reports propagating
behavioral proof obligations into Modeling. Evidence is retained in
`/tmp/trellis-modeling-next-live-v4-evaluation`.

The next run, `/tmp/trellis-modeling-next-live-v5`, uses the agreed descriptive
process overview: a long version of the current phase, short versions of the
other phases, and explicit completion conditions. The full reference remains
on disk. Setup passed its audit with unchanged Rust. Role 5 wrote a malformed
JSON response with a leading `+`; the driver crashed while parsing it. The raw
artifact, prompts, and implementation snapshot are retained in
`/tmp/trellis-modeling-next-live-v5-evaluation`.

Malformed response artifacts now produce a recorded rejection and return to
the reviewer while preserving the closed-stage edit guards. A temporary
recovery check and the 30 existing modeling/tool tests passed; no new repository
unit tests were added. The fresh all-Terra run
`/tmp/trellis-modeling-next-live-v6` included the recovery fix from the start
and ended with final audit approval after 15 role calls. Rust stayed unchanged,
both goals remained pursued, and the reviewer requested verification and final
audit without assigning Lean proof work. Every delivered task prompt and role
model/base setting was checked against the saved assembly; runtime files stayed
identical to the launch snapshot.

Minimality remains weak. The authored node contains four transparent wrappers
over existing library operations. Substantiveness and final audit called them
necessary because the extracted module lacked the named interpretations.
Independent goal-statement elaboration succeeded using only the generated
module and existing libraries, with no authored modeling node imported.
The full evidence and remaining navigation/environment issues are recorded in
`/tmp/trellis-modeling-next-live-v6-evaluation/RESULT.md`. This run demonstrates
improved phase discipline and a complete acceptance path on a small fixture;
it does not establish minimal authoring or large-source adaptation reliability.


## Modeling with actual goal statements

Phase 0 now produces paired Lean/TeX goal nodes over the extracted model,
with proofs left open, and amends the model as appropriate for faithful goal
expression. Completion requires actual statement coverage and successful Lean
statement checks as well as the existing semantic reviews and final audit.
Reviewer questions now reach the verifier prompts without the scheduler queue.

The fresh run `/tmp/trellis-modeling-goal-stating-live-v3` completed with final
audit approval after 19 role calls, all Terra/high. Rust stayed unchanged.
The product has three shared interpretation definitions and two goal theorems,
each explicitly requiring an output returned by the extracted function. The
false strict-increase claim remains intact for later disproof. Final audit
independently reran extraction and all Lean checks uncached; only the two
intentional goal proofs remain open. Delivered prompts match their saved
assemblies and implementation hashes stayed unchanged throughout the run.
The 30 existing modeling/tool tests passed; no additional test cases were added.

This product is a useful starting point for argument development: it exposes
actual obligations and avoids the unused predicates and result wrappers in the
immediately preceding aborted candidates. Shared names are justified by their
use and readability, not by an absence of equivalent library expressions.
The review process still repeats broad whole-artifact assessments, and some
verifiers read previous verdicts from the state file despite their omission
from the assembled prompts. These remain efficiency and review-isolation
concerns. This small fixture does not validate difficult source adaptation or
production downstream dispatch. Full evidence and evaluation are retained in
`/tmp/trellis-modeling-goal-stating-live-v3-evaluation/RESULT.md`; earlier v1/v2
attempts were stopped and preserved before final approval.


The next fresh run, `/tmp/trellis-modeling-goal-stating-live-v4`, incorporated
role-specific review-history guidance, optional questions routed to individual
reviews, and definition guidance based on faithful, clear goal statements.
It completed with final audit approval after 19 Terra/high calls, unchanged
source, and a fresh uncached full check. Reviewer 9 requested verification with
an empty instruction; actual verifier prompts contained no broadcast question
or full-decision-log pointer. Node assessments were more focused than v3.
However, several verifiers independently opened prior review records on disk,
so these prompt changes do not enforce review isolation. The artifact contains
essentially the same three interpretations, with explicit return existence in
Reading and a conditional strict-increase statement accepted by the reviews.
It does not establish improved artifact minimality. The 30 existing modeling/
tool tests passed; no test functions were added. Full evidence is in
`/tmp/trellis-modeling-goal-stating-live-v4-evaluation/RESULT.md`.

## Shared-verifier v5 and argument continuation

`/tmp/trellis-modeling-goal-stating-live-v5` completed Phase 0 with final-audit
approval in 21 all-Terra/high role calls. Decisions and evidence now use the
shared kernel contract/validation/normalization/reconciliation path. Eight
current verifier results passed; the final audit ran an uncached pipeline check.
The accepted statement collection omits explicit success on every input, despite
the supplied reading goal requiring return. Faithfulness and final audit missed
this. The v4 statement collection was stronger on that obligation.

The accepted artifact continues in the normal argument runtime at
`/tmp/trellis-modeling-goal-stating-live-v5-argument-v2`. The first argument attempt
is preserved separately. Bootstrap and launcher fixes addressed complete crate
copying, compatible dependency-cache reuse, PV startup without a paper, external
library mounts, provider-specific role-home seeding, and paired TeX wrapping.
The corrected continuation completed initial planning and reached its reviewer.
Full evaluation: `/tmp/trellis-modeling-goal-stating-live-v5-evaluation/RESULT.md`.

## Verifier dispatch correction after v5

The prototype introduced reviewer-written questions into verifier assignments,
and used those questions to invalidate passing reviews. Production verifier
contracts have no such communication channel. That deviation was unjustified;
the earlier notes endorsing it are superseded. The channel and its pass-reset
behavior have been removed. Verifier assignments now select only system-owned
subject fields, including when rendering an old queued task.

Shared result validation, normalization, reconciliation, and own-finding evidence
remain in use. Phase-specific source/goal rubrics and artifact navigation adapt
those reviews to the modeling workspace. The prototype still has separate
scheduling and whole-work fingerprint invalidation; these are remaining
mechanical differences, not evidence of full production parity.

The 30 existing modeling/tool checks pass after updating the affected regression
case. A full prompt reassembled from v5 faithfulness call 18 retains the original
goal and removes the reviewer question; it is saved under
`/tmp/trellis-modeling-verifier-dispatch-review/faithfulness.prompt.txt`.
This correction has not yet had a fresh live Phase 0 experiment. The existing
argument-development continuation is left running.

## Native verifier lifecycle

Modeling now uses the production kernel phase scheduler and bridge. The standalone
driver ends after setup and adaptation audits. Native Modeling shares statement
frontiers, dependency ordering, per-review fingerprints, blocker resets, panel
execution, normalization, evidence retention and malformed-artifact retries.
There is no prototype verifier queue or separate verifier evidence adapter.
Phase-specific contracts select the modeling rubrics and permit open goal proofs.
The native completion audit advances to argument development only with current
passing semantic gates. Exception requests use the audit route. Source/extraction
reopening conservatively terminates the current runtime for a new preparation.

The bootstrap currently retains the existing single-generated-module limitation;
multi-module/external-module authoring still needs an observation/bootstrap adapter.
The shared native verifier lifecycle itself is independent of the Rust fixture.


## Native lifecycle restart after assembled-prompt review

v6 passed setup and adaptation audits and entered the production Modeling worker.
The worker followed an inherited `target_false_under_model` contract and stopped
without stating the goals. Its retained transcript demonstrates a phase-contract
conflict. Modeling now omits that outcome and the later-phase substantiveness
validity rubric. Other phases retain their existing behavior. v7 was interrupted
during preparation to include the complete correction.

The replacement is `${TRELLIS_ROOT:-/path/to/trellis}/pv_runs/trellis-native-phase0-live-v8`, frozen
at implementation commit `dae3893303e2bad3b215ea42c4af06c7553ccda8` in
`${TRELLIS_ROOT:-/path/to/trellis}/pv_runs/trellis-native-implementation-v3`. All roles use
`gpt-5.6-terra` with high effort. The native repository/runtime names are
`${TRELLIS_ROOT:-/path/to/trellis}/pv_runs/trellis-native-phase0-v8` and the same path with `-runtime`.
At launch it has no completed Modeling audit; progress and evaluation belong in
the adjacent `trellis-native-phase0-live-v8-evaluation` directory.

Before launch, six complete role prompts were assembled through the shared
builders. Worker false-goal routing and substantiveness's later-phase rubric
were absent. A fresh extraction/Lean check passed with the preparation mounted
read-only and output directed to scratch. The shared kernel library suite passed
1811 tests; the focused native gate test also exercises non-acting exception
validation, failure halting, and bounded malformed-audit handling. Earlier Python
checks passed 180 tests; the affected preparation subset passed 21 after the
output-directory change. These checks do not establish live Modeling acceptance.


v8 reached the native completion audit, which returned `modeling_revise`. Its
full evaluation is retained in
`${TRELLIS_ROOT:-/path/to/trellis}/pv_runs/trellis-native-phase0-live-v8-evaluation/RESULT.md`.
The replacement v9 is running from frozen commit
`f04aea07e9c3b8d48c3f0a795a190d343b1db4ed`; its paths and launch status are in
`${TRELLIS_ROOT:-/path/to/trellis}/pv_runs/trellis-native-phase0-live-v9-evaluation/STATUS.md`.
It includes actual-audit-sandbox tool mounts, read-only support-file checks,
extraction-node authority in Modeling invariants, and a faithfulness criterion
for supplied obligations moved into premises. No Modeling approval is claimed
for the replacement at launch.

**Trellis parallel dispatch — approved implementation plan**

4 October 2026

The proposal is to build a complete parallel dispatch system around a simple reviewer interface: the reviewer assigns ordinary tasks to workers, chooses a model profile, and hears back when each attempt finishes. The supervisor handles isolation, queues, resource admission, integration, and recovery. The user approved implementation on 4 October 2026 after the process decisions below were agreed. Implementation and validation must preserve these decisions; any newly discovered process-semantics ambiguity must be brought back for agreement.

The checker work gives us the central service needed to manage compilation from workers, incremental checks, and authoritative checks. Parallel dispatch will replace the separate sidecar in this design. We still need task isolation, conflict rules, result integration, and enforcement of compiler access. The supported compilation paths use central admission. Agreed on 5 October 2026: use clear prompts and ordinary command guards for cooperative agents, without a new privileged helper or an adversarial compiler-execution boundary. Deliberate bypass is a resource-management risk; authoritative proof acceptance remains unchanged.

1. **Keep the reviewer interface small.**

   A reviewer response should be able to submit several worker assignments using the concepts it already uses: task context, mode, target or scope anchor, authorized edits, blockers, and a model profile. Each assignment also declares whether the worker needs managed Lean access, and may select a shorter timeout where its profile allows it. The reviewer should see a compact account of running tasks, completed attempts, available capacity, and eligible work.

   It should not have to coordinate worker messages, manage processes, build a second dependency graph, or arrange merges. Workers should receive a clear task and return a result. There is no need to introduce nested worker dispatch for this system.

   The supervisor validates each assignment and reserves its scope before launching it. A rejected assignment should produce a clear explanation: for example, its authorized edits conflict with an existing task, its dependency assumptions have changed, or its selected profile is unavailable. These messages and prompt changes should retain the existing clear, minimal style.

   Agreed on 4 October 2026: the reviewer assigns work into currently available worker slots. It does not maintain a backlog of assignments awaiting future worker capacity; subsequent openings trigger a fresh review based on current results. Accepted assignments reserve their slots while launching. Compiler jobs and required verifier work still have their own centrally managed queues.

2. **Wake the reviewer when useful dispatch becomes possible.**

   Worker completion, timeout, or another event that makes queue capacity and eligible work available should make the reviewer runnable. The reviewer can replenish the queue while other workers continue.

   Only one reviewer invocation should run at a time. Several simultaneous completions should be collected into one review when possible. Events arriving during a review must remain recorded and cause a follow-up when needed. This should be event driven, without a repeated idle review loop.

   A reviewer necessarily reasons from a snapshot while other work can finish. Its returned assignments must therefore be checked against current state before launch. An unrelated completion should not invalidate an otherwise sound assignment merely because the global event counter changed. A conflicting assignment should be rejected with the changed condition identified.

   We should distinguish agent capacity from compiler capacity. A worker can spend time reasoning or editing while another worker compiles. Availability of an agent slot does not imply that a compiler slot must already be free.

3. **Provide a launch-configurable menu of worker models.**

   A launch configuration should define named profiles. The reviewer selects a profile ID; the host resolves that ID to the provider, model, reasoning effort, execution limits, and any explicitly configured fallback policy.

   Proposed defaults are:

   | Profile | Model | Reasoning effort | Worker timeout |
   | --- | --- | --- | --- |
   | `sol-xhigh` | `gpt-6.1-sol` | `xhigh` | Four hours by default; launch configuration can allow longer bursts |
   | `luna-high` | `gpt-6-luna` | `high` | At most one hour under the default profile; reviewer may choose less per task |

   The current Luna model is `gpt-6-luna`. The requested effort levels are supported by the respective [GPT-6.1-sol documentation](https://developers.openai.com/api/docs/models/gpt-6.1-sol) and [GPT-6-luna documentation](https://developers.openai.com/api/docs/models/gpt-6-luna).

   Existing difficulty hints can inform the reviewer's choice, but should not silently override an explicit selection. Similarly, an unavailable profile should not silently turn into a different model unless the launch configuration explicitly allows that fallback. The recorded task should show both the requested profile and the actual model used.

   Agreed on 4 October 2026: in a run with multiple model profiles, weaker profiles default to **Lean-closure-only** work. The default `luna-high` profile may attempt the proof body of one existing Lean-open theorem, including an eligible SKETCH node, and add imports of existing support. It may not plan, decompose, restructure, author NL proofs, add obligations, change statements, remove existing imports, or integrate new helpers. Import additions remain subject to ordinary dependency and trust checks and must preserve the elaborated theorem statement. The stronger `sol-xhigh` profile permits ordinary tasks and closure attempts. These are launch-defined `allowed_tasks` permissions enforced during reservation, launch, and candidate validation, rather than advice the reviewer may override. Custom profiles without explicit permissions default conservatively to closure-only; a launch configuration may explicitly grant broader permissions. Model strength and compiler capability remain separate.

   Worker profiles and verifier panel policy are separate. Giving the reviewer a worker model menu must not let it weaken required verifier membership, independence, or acceptance rules.

   Agreed on 4 October 2026: timeout defaults must accommodate productive multi-hour Sol bursts. Use four hours as the default Sol limit, with longer durations configurable at launch. The default Luna profile caps attempts at one hour and lets the reviewer select a shorter task-specific limit. The supervisor validates that selection against the launch profile. Four hours is a Sol default, not a universal maximum.

   The reviewer should receive concise cost guidance derived from the run's configured profiles. Launch metadata can supply input, cached-input, and output API rates or an explicit relative-cost hint. When those rates make one model much cheaper, the prompt should explain that its API use can be treated as nearly free relative to the expensive model and encourage bounded attempts on promising independent tasks. Do not infer a price ratio merely from model names or hardcode a pricing comparison that may not apply to the launch. Worker slots and compiler capacity remain finite even when API use is inexpensive.

   In particular, reviewers should willingly try cheap models for early Lean closure before spending effort writing or verifying an NL proof where eligibility allows it. A successful authoritative closure can retire the node's NL-soundness obligation under the existing process, saving both proof-writing and proof-review work. This becomes an ordinary reviewer-dispatched worker assignment, with the existing statement, scope, dependency, and closure checks. Existing cheap-attempt history should be visible so reviewers can assess progress and choose whether to retry or use a stronger model.

   Agreed on 4 October 2026: the reviewer may dispatch Lean-closure attempts on existing SKETCH nodes. This intentionally extends the current grunt eligibility predicate, which excludes sketches. The assignment must still satisfy the applicable statement checks, authorized edit scope, dependency reservations, and authoritative closure checks. A failed or unfinished closure attempt does not clear the sketch's soundness blocker. This permission does not authorize creating arbitrary new sketch obligations or bypassing other existing task restrictions.

   The basic Trellis phase structure remains controlling. In theorem-stating, the reviewer's objective is to eliminate verifier blockers and reach the existing phase-advance gate. Lean closure is one permitted way to discharge an eligible blocker, such as the NL-soundness obligation; maximizing Lean-closure counts is not a separate theorem-stating objective. Checked closure does not waive paper-faithfulness, correspondence, substantiveness, protected approvals, or any other check not already waived by the current process. Do not make closure of every remaining Lean-open node a prerequisite for leaving theorem-stating.

   Cheap-model guidance must remain subordinate to that phase objective. Offer closure attempts when they plausibly remove blockers or save the work otherwise needed to remove them. Once the existing advance conditions are met, stop dispatching new speculative theorem-stating work and follow the agreed drain and human-gate mechanisms. Do not keep the phase open merely because more inexpensive Lean attempts are possible.

4. **Reserve disjoint edit scopes, with directional dependency conflicts.**

   Updated agreement on 7 October 2026: closure-only assignments may target any eligible existing Lean-open proof node, including SKETCH nodes and nodes outside the ordinary reviewer's coarse cone. They preserve the reviewer’s coarse focus and may add imports, but cannot change statements, remove existing imports, change options, edit TeX, or edit other nodes. Ordinary assignments retain their existing routing and scope restrictions.

   An authorized private closure task can run while verifier lanes are pending. Its private worker routing preserves all verifier evidence; canonical verifier ordering, phase gates, and acceptance checks remain unchanged. Advisory audit ordering does not require leaving spare slots idle when useful compatible closure work is available.

   Concurrent write scopes must be disjoint. If A depends on B, a closure-only task on B may overlap with an ordinary or closure-only task on A. An ordinary writer on B still conflicts with either kind of task on A. Apply this rule to the entire authorized edit scope and transitive semantic, Lean, and definition dependencies. Tasks may share unchanged dependencies; connected nodes within one task are allowed.

   Capture exact source hashes for every relevant input. For readonly proof dependencies, also capture the prefix through the BODY marker. A matching prefix permits a candidate to be rechecked after a dependency's proof body changes; it does not certify an unchanged module interface or authorize reuse of old compiled evidence. Own writes, TeX, definitions and global inputs remain exact. Legacy assignments without prefix identities remain exact as well.

   Acceptance overlays only the candidate's own edits onto current canonical sources and reruns authoritative validation. Publication rechecks the complete current source manifest; intervening changes require another acceptance round. Admission, reviewer self-checks, and the post-candidate graph check use the same directional conflict rule. This preserves independent progress in either completion order.

   New helpers retain task-specific ownership. A worker cannot claim another task's namespace or expand its authorized scope. Shared writes and exclusive operations retain their existing exclusions.

   Reviewers should use available slots for useful compatible closure attempts, including speculative inexpensive attempts. A dependency antichain is sufficient but no longer necessary. Phase objectives, pending exclusive operations, model limits and genuine lack of useful work can still justify idle slots.

5. **Represent global changes explicitly.**

   Some changes do not fit a node antichain: shared preamble edits, source or model changes, toolchain changes, target coverage changes, and global restructuring can affect otherwise unrelated nodes. They need explicit reservations and an exclusive boundary for incompatible work.

   Agreed on 4 October 2026: when such an operation is required, the supervisor stops launching conflicting tasks, lets affected workers finish, then applies the change at an exclusive checkpoint through the existing Trellis transition and approval mechanisms. This is the normal draining policy; ordinary task timeouts and operational failure handling still apply. A global operation must not quietly invalidate work that is still running against its old assumptions.

   Existing mandatory audit and repair ordering must remain in force. Human approvals and protected semantic decisions remain human gates. Parallel execution must not create another route around them.

   Agreed on 4 October 2026: parallel dispatch replaces the separate sidecar for this design. When enabling parallel dispatch, finish the legacy sidecar's drain and result handling at the migration checkpoint, preserve its evidence and closure provenance, and disable its dispatch path. Cheap Lean-closure attempts then use the ordinary worker ledger and reviewer assignments. Reuse useful isolation, acceptance, and recovery mechanisms without running a second scheduler. This is a migration requirement for the parallel system, not an instruction to stop the current live sidecar before its replacement is deployed. Historical sidecar results remain subject to their original trust checks.

6. **Give every task isolated state and integrate results serially.**

   Each task needs its own workspace, base snapshot, conversation or session identity, checker capability, retry context, and result artifacts. Several current mechanisms assume one worker: the kernel has one in-flight request, worker sessions are not keyed by task identity, and failure recovery uses shared worker-base and `last_invalid` state. Those assumptions must be removed together.

   A worker result should first be sealed as an immutable candidate. Integration should then validate its authorized changes and base assumptions, stage those changes against the current canonical state, run the normal authoritative checks with their existing cache identities, and publish the accepted files and corresponding events as one recoverable transition.

   Canonical integration should be serialized. Expensive independent preparation can still use the central checker, but publication must never expose a mixture of two partially applied results. Recovery must make an interrupted publication unambiguous and avoid applying a result twice.

   Acceptance of a worker's candidate can still leave verification blockers. It must not imply that all required verifier lanes have approved the new state.

   A failed or stuck worker must never restore the shared canonical worktree to that worker's starting snapshot. Today, singleton recovery can restore the active worker base; under parallelism that could erase another task's accepted work. Failure should preserve the failed task's own artifacts and leave canonical state alone.

   Results whose assumptions have become stale should receive an explicit stale result with a reason. The supervisor should not resolve conflicts in mathematical text by making an automatic semantic choice.

7. **Make every worker attempt finish with a clear outcome.**

   Preserve the existing mathematical outcomes, including Valid, Stuck, NeedsRestructure, and the specialized model or certification outcomes. Distinguish them from execution outcomes such as timeout, launch failure, cancellation, and a stale result.

   A timeout must stop the task and its managed compiler work, preserve useful artifacts and diagnostics, and reap the relevant processes before releasing occupied resources. A late callback cannot revive a timed-out attempt or publish its candidate.

   Agreed with qualification on 4 October 2026: completed attempts and their outcomes should be visible to the reviewer, while preserving the existing bounded automatic retry policies where they apply. An automatic retry need not wait for reviewer authorization. When the existing policy escalates, the reviewer can retry, change the task, choose another model, or request restructuring. The earlier proposal to replace all automatic worker retries is withdrawn.

   The current kernel's ordinary theorem-stating, revision-stating, and proof-formalization paths use a threshold of two attempts for consecutive Invalid or Stuck outcomes. Cleanup has specific exceptions. Transport failures have a separate configurable retry budget and circuit breaker. Port these rules into task-local state, retaining their existing limits and escalation behavior. A retry and a reviewer assignment must not accidentally dispatch the same work twice; the reviewer must see whether an automatic retry is already queued or running. Transport recovery within an attempt remains distinct from starting another worker attempt.

   Agreed on 4 October 2026: an attempt's wall-clock timeout starts when the worker launches. Time waiting for a worker slot is excluded. Once launched, the clock includes checker queue waits as well as reasoning, editing, and compilation. Report checker queue wait separately so the reviewer can identify congestion when deciding what to do after a timeout. Apply the profile-specific limits above, including the reviewer's ability to shorten Luna tasks.

8. **Use separate verifier queues and enforce compilation capabilities.**

   Verifier lanes have their own queues. Agreed on 4 October 2026: paper-faithfulness, substantiveness, and natural-language soundness verifiers receive the required text and evidence and cannot run Lean. Correspondence and audits that need compilation retain managed Lean access. Specialized witness, model, and audit tasks must be explicitly classified according to their required capabilities.

   A verifier designated as unable to compile receives no capability for submitting compilation jobs. Ordinary compiler entrypoints explain the restriction and instruct it to report the need for Lean to the reviewer. This is a cooperative resource policy, not a claim that arbitrary native compilation is impossible.

   Apply the same distinction to workers, per assignment. A task limited to NL proof or text work can run without Lean access when its authorized edits permit that scope; it consumes a worker slot but no compiler slot or compiler-memory reservation. Enforce the task's write restrictions as well as its lack of compiler access. If it discovers that Lean work is needed, it reports that requirement for reviewer reassignment instead of silently acquiring compilation capability. NL-only status does not relax the ordinary writer dependency restrictions.

   A worker's lack of Lean access does not exempt its result from authoritative acceptance or downstream verifiers. Any host preparation or later correspondence check that uses Lean is separately admitted to the shared compiler budget. The capability describes the worker's own execution, not a promise that the entire lifecycle of a text change is compilation-free.

   Such verifiers should not consume worker slots or reserved compiler memory. They can run with much greater concurrency, subject to provider limits, spending policy, and ordinary host capacity. “Unconstrained” here means independent of the compiler budget, not literally unlimited process creation. Any host preparation that does compile must still pass through central admission.

   Each verification should run against an immutable input snapshot and carry its lane identity and relevant fingerprints. Acceptance must preserve the existing prerequisites, panel rules, and freshness checks. A timeout, missing panel response, or stale result cannot count as approval.

   We should preserve lane-specific freshness rather than making every unrelated change invalidate every verifier. At the same time, the implementation audit must verify that each fingerprint covers the evidence that its lane actually relies on. Substantiveness deserves particular attention because its rubric can involve consumers while its current fingerprint is deliberately independent of dependencies. Any change to that trust contract needs an explicit decision.

   Agreed on 4 October 2026: the supervisor automatically dispatches eligible required verification, and the reviewer retains its existing ability to explicitly assign verifier lanes when it wants to. Automatic dispatch supplements reviewer requests. Both routes preserve the existing verifier request semantics, prerequisites, panel rules, freshness checks, and human gates. This avoids waiting for a reviewer merely to start a mandatory check while preserving reviewer-directed verification.

9. **Share resource accounting and reuse existing recovery mechanisms.**

   Agreed on 4 October 2026: launch defaults are four worker slots shared across model profiles and two compiler slots, managed independently. Verifier capacity is separate. These limits are configurable at launch. NL-only workers share the four worker slots and do not reserve compiler resources. A larger number of reasoning agents does not imply the same number of simultaneous Lean workloads. All supported compilation, including incremental checks and the cheap closure tasks replacing sidecar work, passes through central admission. The 5 October resource-policy agreement above governs command guards.

   The present checker already supplies useful building blocks: managed jobs, process ownership, queueing, captured inputs, private workspaces, and reconnectable results. The wrapper and legacy sidecar also have recovery mechanisms worth reusing in the unified dispatcher. Reusing those mechanisms does not retain a separate sidecar service in the new workflow.

   Record task intent durably before launch. On restart, identify and adopt a surviving attempt where possible; otherwise resolve it explicitly. Completion and integration must be idempotent. Cleanup must preserve live work and the evidence required to explain or recover a failure.

   The viewer should expose the task, selected profile, authorized scope, current status, elapsed execution, checker wait, outcome, and transcript. This lets us inspect actual agent behavior as well as aggregate timing. Performance comparisons should retain workload type, cache or warm-session status, and timing phases, because raw check durations have high variance.

10. **Specify and test the complete system together.**

    The implementation should cover the kernel task ledger and transitions, response schema, launch configuration, task isolation, result integration, verifier queues, restart recovery, viewer support, and migration of an existing run. These are coupled parts of the same process contract.

    The core acceptance cases should establish that independent work overlaps, conflicting scopes and dependencies are refused, one worker's failure cannot undo another's success, stale verification cannot approve changed inputs, and timeout or restart cannot cause a result to integrate twice. Tests must also establish that workers and verifiers without compilation capability cannot obtain it, that their downstream compilation remains centrally admitted, and that existing human gates remain unchanged. Migration tests must establish that the retired sidecar cannot dispatch duplicate work and that its historical evidence and accepted closures remain intact.

    Phase tests must cover closure attempts on existing SKETCH nodes, retirement of their soundness obligation only after authoritative closure, and preservation of all other applicable verification requirements. A theorem-stating run that satisfies its existing advance conditions must remain able to advance with Lean-open nodes; pending speculative opportunities must not create new advance blockers. Failed cheap closure attempts must not manufacture verifier approval or weaken task restrictions.

    Model the important completion, cancellation, integration, and restart interleavings, then exercise the real confined runtime. Formal model checks should complement real process tests. Any TLC smoke should remain bounded to one or two minutes; longer exploration can run in the background on one or two cores.

    Migration should happen at a recoverable checkpoint. An existing single request can be represented as one task in the new ledger where compatible, preserving its evidence and identity. Deployment should retain a clear recovery path and then be observed in the live run using actual transcripts and checker telemetry.

    We can build this complete architecture together and still validate deployment progressively. Progressive testing does not require defining a weaker process model that we immediately need to replace.

11. **Implement a durable task ledger and explicit transitions.**

    Replace the singleton worker request and worker recovery context with task records in `kernel/src/model.rs`, transitions in `kernel/src/engine.rs`, and task-specific contracts in `kernel/src/request_contracts.rs`. Preserve a single reviewer invocation. Each worker task records its originating review, phase, profile binding, authorized scope, compiler capability, base snapshot, timeout, retry lineage, process identity, artifacts, and outcome. A retry receives a new attempt identity while retaining the task's history.

    Represent launch, execution, candidate checking, and integration separately. A submitted candidate is not accepted work. Terminal records distinguish accepted candidates, mathematical outcomes such as Stuck or NeedsRestructure, deterministic rejection, timeout, cancellation, launch failure, and stale inputs. Existing mathematical outcome handling remains authoritative; these lifecycle fields make concurrent execution and recovery explicit.

    Move retry context, failed-work snapshots, and counters to the appropriate task or node identity. Preserve the existing retry thresholds, phase exceptions, and transport circuit breaker; failures on an unrelated concurrent task must not consume another task's allowance. A reviewer response gets a durable dispatch receipt identifying accepted assignments and concrete reasons for rejected ones. Replaying that response cannot launch duplicate workers.

12. **Hold worker slots through checker completion.**

    Agreed on 4 October 2026: the four worker slots count unresolved worker assignments, including execution, waiting for checker resources, and final acceptance checking. A worker slot remains occupied after the agent exits while required checker work is outstanding. Release it only when the attempt is accepted, rejected, or otherwise resolved and its processes have been reaped. Retain its node reservations through the same resolution boundary. The reviewer sees the checking status and cannot assign another worker into that slot merely because the agent process has finished.

    Checker jobs still compete for centrally admitted compiler and memory resources. Holding a worker slot does not bypass that admission or reserve a compiler for the entire burst. Keeping the slot occupied through checking bounds outstanding worker candidates by worker concurrency. Runnable automatic retries participate in admission before fresh assignments, preserving their existing policy without allowing duplicate ownership of the same scope.

    Reserve scope and execution capacity atomically before launch. Validate a batch against the current canonical state and against other accepted assignments in the batch. Return a specific rejection for each assignment that fails, preserving valid independent assignments. Revalidate scope and dependency assumptions when a result is integrated. The reservation must cover new-helper ownership and shared resources as well as existing node edits.

    Completion, failure, timeout, and newly available capacity are durable scheduler events. Coalesce reviewer wakeups, record which events a review has seen, and retain later events for follow-up. Automatic verifier dispatch does not require a reviewer turn. Suppress repeated review turns when nothing actionable has changed. Phase advancement and mandatory repair routing use the existing kernel predicates, with the agreed drain boundary for incompatible changes.

13. **Isolate execution and publish checked candidates recoverably.**

    Refactor `trellis/runtime/bridge.py` so worker conversation identity includes the task and attempt. Give each attempt its own checkout, working directory, writable build artifacts, result paths, and compiler capability. Reuse immutable package inputs where safe; do not share writable Lean artifacts between workers. Storage cleanup must consult live ownership and preserve the evidence needed for recovery.

    In `kernel/src/runtime.rs`, replace whole-run rollback on an individual worker failure with retention of that task's work and a task-specific outcome. A retry may receive its predecessor's preserved work through the existing recovery semantics, scoped to that task. It must not overwrite canonical work accepted from another worker.

    Seal a returned candidate, check its write set and base assumptions, and construct the proposed canonical update in isolation. Run independent host acceptances concurrently in private workspaces, with compilation subject to central resource admission. Worker slots bound outstanding acceptances; there is no separate serial acceptance queue. Reserve semantic inputs through publication so a conflicting publication waits. Before publication, revalidate the checked inputs against current canonical state; retain completed evidence for reconciliation if the reservation invariant fails, rather than repeating the full check. Publish files and the corresponding event through a serialized, recoverable transaction so a crash cannot leave an unexplained partial merge or cause double application. Resume either completes that recorded transaction or restores its own prior state; it never rolls back an unrelated accepted task. Stale candidates remain preserved with an explanation.

14. **Route compilation through the managed checker.**

    Extend the existing sandbox and checker registration mechanisms in `trellis/sandbox.py`, `trellis/checker/jobs.py`, and `trellis/checker/admission.py`. The worker profile chooses model strength; the task capability separately determines whether it may compile. Broker authorization enforces managed job capabilities. PATH commands route to the service, and ordinary absolute elan launchers return a clear instruction to use supplied checker tools and not work around the guard. Toolchain bytes remain readable for identity checks; copied or deliberately bypassed compilers are outside this cooperative resource guarantee.

    Before enabling parallel workers, audit worker and verifier launchers, incremental and scratch checks, authoritative acceptance, host preparation, prewarming, and recovery paths. Every supported Lean workload must have an owner and enter central admission. Tests must try the known direct invocation paths from real confined agents, not merely inspect their PATH. Retain clear ordinary Lean diagnostics and distinguish them from denied capabilities, stale inputs, resource exhaustion, and infrastructure errors.

    Keep compiler queue priority, memory admission, process-tree cleanup, and resident-session accounting centralized. Account for the checked-out input bytes and the toolchain identity. Preserve the separation between advisory worker checks and authoritative acceptance; an advisory success is never an approval or closure certificate.

15. **Schedule verifier jobs against immutable evidence.**

    Refactor verifier dispatch around job identities containing the lane or panel, node or global scope, input fingerprints, and configured verifier binding. Automatic and reviewer-requested dispatch should converge on the same job when their required inputs and policy are identical. A changed fingerprint produces a new verification obligation. Keep verifier panel composition, independence requirements, and reconciliation rules intact.

    Launch text-only lanes without compiler capability. Prepare their required evidence through trusted host paths first; any compilation during that preparation uses central admission. Compilation-capable verifiers submit managed jobs in the same way as workers. Verifier agent limits remain separate from compiler slots and reflect launch or provider limits.

    Accept a verifier result only if its fingerprint and panel identity remain current at integration. Preserve stale results as evidence without letting them approve changed content. Superseded queued work may be removed; running work may be cancelled where supported after preserving its outcome state. Audit each lane's fingerprint against the evidence it actually reads, especially substantiveness's use of consumers. If that audit exposes a trust-policy ambiguity, present it for decision instead of silently narrowing evidence or reusing an insufficient fingerprint.

16. **Update configuration, prompts, and the viewer together.**

    Add launch-defined worker profiles, the four-worker/two-compiler defaults, separate verifier limits, task-specific compiler capability, timeout selection, and optional cost metadata to the existing configuration and request-schema machinery. Validate these settings at launch and give clear errors for unavailable profiles or out-of-policy assignments. Do not infer model substitutions from a difficulty label.

    Update `trellis/runtime/bridge_prompts.py` and the relevant prompt fragments with the smallest changes needed to explain multiple assignments, available capacity, profile costs, task capabilities, and outcomes. Replace the sidecar instructions in parallel mode. Keep the theorem-stating phase objective prominent; cheap closure guidance must not override it. Show previous attempts and their outcomes without flooding the reviewer with full transcripts.

    The viewer should expose concurrent tasks and verifier jobs, their profiles and scopes, execution versus checker wait, candidate-checking status, and final outcomes. Preserve links to transcripts and historical sidecar provenance. Distinguish a requested checkpoint stop from an already stopped run and a worker's completion from acceptance of its candidate.

17. **Validate and migrate the full design before enabling it.**

    Implement the work packages in dependency order: protocol and ledger; scheduler and reservations; isolated execution and integration; compiler capability enforcement and verifier queues; prompts, viewer, migration, and end-to-end validation. These are internal implementation steps for one complete design, not separate weaker production semantics.

    Extend the existing `spec/SupervisorCore.tla` and `spec/SupervisorProtocol.tla` model coverage with bounded concurrent-task scenarios. Check reservation exclusivity, phase safety, at-most-once publication, stale-result rejection, timeout cleanup, and recovery around every publication boundary. Use the bounded TLC smoke and background exploration described above. Add Rust transition/replay tests and Python tests that exercise the actual wrapper, sandbox, and checker together.

    Required end-to-end cases include two independent workers completing in either order; a failed or timed-out worker alongside a successful one; helper-scope conflicts; a verifier finishing after its input changes; an NL-only worker denied compilation; a cheap SKETCH closure retiring only its eligible soundness blocker; automatic retry concurrent with fresh review; restart during launch or integration; and theorem-stating advancing without demanding all Lean proofs be closed. Exercise the full profile menu and the accepted timeout rules.

    Migrate the live run only at a recoverable checkpoint after sidecar drain and result reconciliation. Preserve all canonical mathematical files, event history, approvals, closure records, and existing failure evidence, with an explicit old-to-new state mapping. Retain compatibility for historical replay. Disable legacy sidecar dispatch before new worker dispatch starts, and verify that old callbacks cannot publish a second result.

    Preserve the old deployment and checkpoint as recovery evidence. After new events have been committed, recovery must understand or replay the new ledger; switching an old binary onto new state is not an adequate rollback procedure. On rollout, inspect actual worker and verifier transcripts, managed compiler ancestry, acceptance outcomes, queueing, memory, and disk growth. Compare timings within workload/backend cohorts and exclude deployment smoke probes. Do not claim a speedup from unrelated proof workloads.

**Agreed decisions**

- Agreed, updated 6 October: keep write scopes disjoint; allow an upstream closure-only writer to overlap with dependent tasks, with current-source acceptance. Ordinary upstream writers remain excluded. Closure eligibility is independent of the ordinary coarse focus.
- Agreed: make incompatible global operations exclusive, stop conflicting dispatches, and let affected workers finish before applying the change through existing checkpoint mechanisms. Preserve repair ordering and human gates.
- Agreed with qualification: expose completed attempts and retries to the reviewer, preserve existing bounded automatic retry policies, and retain their escalation to reviewer decisions. Do not replace all automatic retries.
- Agreed: dispatch eligible required verifiers automatically while retaining the reviewer's existing ability to explicitly assign verifier lanes.
- Agreed: paper-faithfulness, substantiveness, and natural-language soundness verifier lanes cannot compile. Correspondence and compilation-dependent audits retain managed Lean access.
- Worker timeouts start at launch and end at candidate submission, including checks performed during worker execution. Submitted candidates do not expire during host acceptance, its queue, or publication. Slots remain occupied through acceptance and cleanup. Default Sol to four hours with longer limits configurable at launch; cap the default Luna profile at one hour, with shorter limits selectable by the reviewer.
- Agreed: the reviewer fills currently available worker slots, with a fresh review when capacity opens, rather than maintaining a backlog of future worker assignments. Compiler and verifier queues remain separate.
- Agreed: default to four worker slots shared across profiles and two centrally managed compiler slots, with separate verifier capacity and launch-configurable limits.
- User direction: replace the sidecar when parallel dispatch is enabled. Cheap closure attempts become ordinary reviewer assignments; retain historical evidence and the existing trust checks.
- User direction: give reviewers launch-specific relative API cost guidance and encourage cheap early Lean-closure attempts that can save NL proof writing and review.
- Worker capability refinement: NL-only assignments use a worker slot without reserving Lean resources; enforce this capability and still admit any downstream compilation centrally.
- Agreed: the reviewer may dispatch closure attempts on existing SKETCH nodes, subject to the applicable statement and authoritative closure checks.
- Agreed: preserve Trellis's phase structure. Theorem-stating aims to eliminate verifier blockers and advance; Lean closure is an optional means to that end and introduces no requirement to close all Lean proofs before advancing.
- Agreed: a worker slot remains occupied through execution, checker resource waits, and required acceptance checking. Release it and its node reservations only when the attempt is resolved; compiler and memory admission remain centrally managed.

Capacity and timeout settings must be visible, with adjustments governed by the launch profiles and without changing the process's trust boundaries. The complete design is approved for implementation. Production enablement remains gated on the validation and migration requirements above.

Implementation authorized by the user’s “get started” instruction on 4 October 2026. Keep the live run on its existing deployment until the complete system passes the stated checks and reaches a recoverable migration checkpoint.

Implementation and operation — 5 October 2026

The production entry point is `python3 -m trellis.dispatch`. `scripts/trellis.sh run` selects it when the durable dispatch ledger is enabled. The native kernel owns assignment legality, reservations, retries, verification, acceptance, phase changes, and publication receipts. The Python coordinator owns durable subprocesses and compiler capabilities. Neither private agent state nor an advisory check can directly replace canonical state.

`parallel_dispatch.enabled` opts in through the run configuration; the shipped `examples/trellis.config.json` carries the block disabled. Defaults are a pool of three workers with at most one heavy worker (`heavy_worker_slots`, counting the profiles designated `lightweight: false`) when the worker profiles mix a heavy and a lightweight class, as the shipped profiles do: `sol-xhigh` (heavy; ordinary work or Lean closure, four hours) and `luna-high` (lightweight; Lean closure only, one hour). A configuration whose worker profiles are all one class, for example luna only, defaults to a single worker and no heavy cap, since a cap on a single-class pool would only shrink it. Six compiler slots and eight verifier slots apply either way. The default heavy cap never exceeds `worker_slots`, and an explicit `heavy_worker_slots: null` means no cap. The older `parallel_dispatch.sol_worker_slots` model-name cap (a hyphen-separated `sol` component counted toward it, regardless of profile name) is still accepted for configurations written against it: naming it, or supplying a profile without a `lightweight` designation, keeps that legacy capacity model and no default heavy cap is added. Checking and cleanup retain the slot; reviewer and verifier lanes do not count. Older runs keep their recorded capacity on resume until explicitly migrated at a drained checkpoint. Profiles accept provider, model, reasoning effort, timeout, maximum timeout, allowed tasks, and optional cost guidance. Longer strong-model runs are configured by raising both timeout fields. Verifier membership and model policy remain governed by the ordinary verifier configuration.

The viewer's **New run** wizard seeds its parallel dispatch controls from the selected template's block and lets each worker profile be included or left out (at least one stays). A preset whose worker lane is lightweight (all luna, sol-guided luna) keeps only the lightweight profiles, so the pool defaults to one worker without a heavy cap; other presets and the custom selection keep the template's profiles and values. Numbers the operator typed are never overwritten, the launch summary names the heavy cap and the profiles, and `trellis_create_run.sh --parallel-dispatch` validates the collected block with the kernel parser at claim time.

At a stopped Start or Reviewer checkpoint, after the legacy sidecar has drained:

```sh
python3 -m trellis.dispatch enable /path/to/runtime
scripts/trellis.sh run /path/to/runtime
```

Use an optimized prebuilt kernel for production. The coordinator loads native state frequently, and an unoptimized build is costly on a long event history. This build retains debug assertions and overflow checks:

```sh
CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true \
  cargo build --release --manifest-path kernel/Cargo.toml --bin trellis_runtime_cli
```

Set `TRELLIS_TRELLIS_KERNEL_CMD` to the resulting binary and record it with the deployment's acceptance identity, as for other prebuilt kernels.

Enablement checks controller ownership, the sidecar lock and journals, existing closure coverage, the ordinary checkpoint hook, managed checker setup, and a central checker with matching capacity. It uses the existing stock bubblewrap installation; no additional helper, FUSE mount, or administrator setup is required. Set `TRELLIS_CHECK_JOBS=1` and `TRELLIS_CHECK_JOBS_PARALLELISM=2` for the default menu. A checker already serving different capacity must be drained and restarted; launch never silently replaces it. This entry point currently supports Lean runs.

If completed sidecar results remain, stop at Start with no in-flight request and keep its legacy configuration enabled for `enable`. Under stopped controller and daemon ownership, enablement runs the existing bounded sidecar boundary before creating the dispatch ledger. It never issues a serial agent request. Active sidecar slots or unreconciled/deferred results still refuse enablement; retain them and retry after resolving the cause. At Reviewer, the spool must already be reconciled. Disable the sidecar configuration after enablement; the native ledger and daemon also reject legacy dispatch once the migration is committed.

New attempts use private workspaces, read-only shared package inputs, managed checker commands, and ordinary compiler-entrypoint guards. A blocked command explains the intended tools and asks the agent not to work around the guard. This manages cooperative agents without disabling native execution or JIT memory. NL-only attempts receive no agent compilation capability. Trusted host preparation, acceptance, correspondence, incremental checks and optional witness execution use centrally admitted operations. Compiler processes and capabilities are drained before the native ledger releases a worker slot. Previously recorded FUSE or native-noexec attempts retain their exact launch recipes for adoption and cleanup; legacy views cannot be unmounted while acknowledged or unreconciled users remain.

Every attempt receives frozen ordinary request bindings and a private repository/runtime. Writable project outputs are copied; immutable Git/package cache objects may be hardlinked. After native resolution and acknowledged process/compiler cleanup, private project build caches are removed; sources, sealed candidates, transcripts, artifact stores, publication journals and deployment binaries remain. A finished worker is reaped before its candidate is sealed. Acceptance reconstructs its originally authorized task against current canonical mathematics, then uses the ordinary normalizer and closure issuer. Generated indexes and unrelated scheduling IDs do not invalidate independent work. Relevant source changes, verifier evidence, graph conflicts, or changed task contracts still do. Source publication and native acceptance use a recoverable journal. Slots remain occupied until publication and compiler cleanup finish.

Once a numbered task's resolved outcome is recorded in the trusted Git checkpoint,
background cleanup also deletes its redundant private protocol snapshots, replay
environment manifests, staged contexts and staged diagnostic results. Generated
framework `src` and `skills` copies are also removed, one bounded tree per
background pass; framework binaries and staging diagnostics remain. Historical
execution re-authentication is not a retention requirement. This does not change
validation or recovery for unfinished tasks. Project sources, sealed candidates, responses,
handoff diagnostics and transcripts remain; `retain-builds.json` pins prevent
cleanup, and Git-indexed files are excluded. Each removal has a small inventory
linked to the checkpoint, and a small display record preserves task transcript
selection without retaining a full private protocol snapshot.

To preview historical cleanup, run `python -m trellis.dispatch.terminal_retention
--runtime /path/to/runtime --task 123`; add `--apply` to remove eligible files.
Omit `--task` to scan all resolved tasks. Busy coordinator checks defer cleanup
instead of blocking protocol work. Existing lossless archive readers remain for
compatibility; deleting unnecessary data takes precedence over archiving it.
To preview generated framework cleanup, use `python -m
trellis.dispatch.framework_retention --runtime /path/to/runtime --repo
/path/to/project --task 123`; add `--apply` to remove eligible trees.

Shutdown parks durable attempts for adoption. `python3 -m trellis.dispatch run /path/to/runtime` adopts them without repeating agent execution. Explicit `resume` additionally lifts an operational pause after its cause is repaired. Provider-budget pauses retain their process and attempt identity. Failed host acceptance can be retried in a new preserved acceptance round; a failed agent process must be resolved through native dispatch rather than silently rerun under its old ID. Ordinary malformed-output and mathematical retry policies retain their task-local budgets. Human gates are unchanged and never cleared by operational resume.

Worker profiles can explicitly designate `lightweight: true` or `false`; the kernel never infers this designation from a model name. New defaults mark `luna-high` lightweight and `sol-xhigh` non-lightweight. In menus containing both lightweight and non-lightweight profiles, `parallel_dispatch.lightweight_attempts_per_node` defaults to 2 (a positive integer, or `null` to disable). The budget is shared across every lightweight profile and counts each launched attempt against each node in its assigned edit scope, including Stuck, Invalid/rejected, timeout and launched cancellation outcomes. Read-only dependencies do not spend budget. Reservations hold a place; cancellation before launch frees it. Source edits, phase changes and ordinary LastClean rewinds do not reset retained launch history. Reviewer assignments and automatic retries both enforce the limit; exhaustion returns control to the reviewer without silently changing model.

Historical missing designation and budget fields retain their exact serialized shape. Enable the policy on an existing drained run through the existing `model_profiles_changed` event and a `lightweight_budget_changed` event (`expected_limit`, `lightweight_attempts_per_node`), then match the launch configuration before resuming. Attempts without a frozen designation use the explicitly configured designation of their recorded profile ID. Such a lightweight profile ID must remain designated while that legacy history exists; adding aliases cannot reset the shared budget. Attempts with a frozen designation retain it even if the launch menu later changes. Restoring a historical checkpoint retains that checkpoint's policy and history; no separate counter outside the recorded run is introduced.

The viewer Tasks tab shows workers, scopes, selected profiles, verifier queues, checking status, outcomes, elapsed time, checker measurements and per-attempt transcripts. Timings retain missing measurements as unknown. Compiler queue wait and execution are reported separately; synthetic integration timings should not be mixed with live proof-check cohorts.

Validation includes native scheduling/publication/retry/freshness regressions, Python coordinator and recovery tests, real Linux sandbox and command-guard tests, viewer tests, and the bounded `ParallelDispatchLifecycle.tla` simulation. The opt-in integration fixture uses deterministic provider answers with the actual bridge, kernel, checker, sandboxed worker processes and publication machinery:

```sh
PYTHONPATH=. TRELLIS_TRELLIS_KERNEL_CMD=/path/to/matching/trellis_runtime_cli \
  python3 tests/fixtures/parallel_dispatch/run.py .work/dispatch-fixture --restart --retry --migrate-sidecar
```

Use a fresh, short directory outside `/tmp`; worker sandboxes intentionally replace `/tmp`. The fixture retains evidence and services for inspection. Reap its attempt launchers and drain its checker before removing disposable build caches. Legacy fixture views require their recorded lease cleanup before unmounting. The lifecycle simulation checks scheduling interleavings, not mathematical acceptance, and does not replace the ordinary Trellis process invariants.

For live migration, preserve the stopped checkpoint, source hashes, event count, launch environment, sidecar journals and old deployment. Deploy matching native/Python acceptance identities, reconcile existing closure identities through the normal migration mechanism, and enable only at the supported boundary. Disable legacy sidecar dispatch; retain its historical evidence. Once parallel events exist, recovery must use a version that understands the ledger. An old binary must never be started on new state. Observe actual reviewer dispatch, managed checks, publication and viewer data after launch.

Multiple coarse goals — 6 October 2026

New parallel Lean runs default to `parallel_dispatch.coarse_goal_slots: 2`.
Use a positive integer to change the capacity, or `null` to retain the historical
single-goal process. Existing enabled runs without this field retain their old
semantics until explicitly migrated. Other backends require `null` for now.

In proof formalization, each ordinary assignment names the coarse goal it serves
with `next_active_coarse`. A successful reservation opens that commitment if a
place is available. Shared support and comparable coarse goals are allowed when
the actual frozen task inputs and authorized writes are compatible. A closure
attempt needs no coarse-goal place. Unknown prerequisites remain verification
gates; an explicitly assigned repair may address its own failed judgment.

An unfinished goal cannot be parked or replaced. Shallow completion, relevant
verification and finished workers release its place; a regression restores the
commitment without evicting another goal. Only an authorized structural repair
that actually removes an obsolete coarse node can eliminate an unfinished goal.

For advisory investigation, a reviewer can submit a non-acting `audit_request`
with `coarse_nodes` naming one or more active goals. It shares the verifier queue,
waits for conflicting evidence writers and uses the central compiler service.
Independent work can continue. Omit `coarse_nodes` for the existing run-wide audit
and any action requiring global process authority. Scoped reports cannot reset
nodes, change statements or polarity, or approve human gates. Theorem-stating's
blocker and phase rules remain unchanged.

Each goal retains its current plan and a verified history head. Reports are
stored once under `.trellis-history/coarse-audits/<sha256>.json`, with immutable
predecessor links. Normal reload verifies current heads; import, export, trusted
rewind and replay verify referenced history. Workspaces and the viewer load a
bounded recent history, with older reports available from Git. Migrating accepted
serial advice records its predecessor state as provenance, not a fresh audit.

To migrate an already enabled run, install the matching implementation and stop
at a drained Start or Reviewer checkpoint using the ordinary deployment process.
Then send the native CLI `dispatch_enable_coarse_goals` action with `root` and
`limit`, using the normal checkpoint hook and implementation/prose bindings.
The command checks controller ownership, outstanding attempts and publication,
and records a replayable predecessor-bound event. Retain the old deployment and
checkpoint for recovery; never run an older implementation on the new state.

The integration fixture accepts `--coarse --scoped-audit` to check a scoped audit
alongside work on another goal, verified report publication and subsequent proof
acceptance. `--coarse --restart --retry` covers simultaneous ordinary goals and
existing attempt recovery. `MultipleCoarseGoals.tla` adds bounded scheduler
simulation of commitments, regressions and shared evidence reservations.

Audit task ordering is advisory. Necessary mathematical repairs remain priorities,
but a plan does not create new dependencies or forbid compatible work elsewhere.
The reviewer chooses assignments; the kernel does not require idle-slot reasons
or automatically fill unused slots.

Acceptance receipts and artifact publication — 7 October 2026

Host acceptance rounds retain an immutable
`dispatch/<task>/acceptance/<round>/completed.json` receipt. A completed round can
be reused only when its full acceptance digest, verifier input map, candidate,
task/profile binding and implementation are still exact, and its preparation,
launch, response, cleanup and checked workspace remain intact. Ordinary native
publication gates still run. A rejected publication preflight records the actual
mismatch in `rejections/<sha256>.json` within that round: bounded field/source
paths and hashes, captured/current digests and source-observation cache metadata.
Private field values are not copied into those rejection records.

New version 2 publication journals retain independent copies of eligible exact
host supervisor artifacts under `dispatch/publication/<sha256>/artifacts`.
The canonical checker installs them under its existing build/probe locks and
replays their imported-module boundary; native source, certificate, dependency,
toolchain and policy checks remain authoritative. Missing or stale private cache
artifacts leave owners to ordinary rebuilding; modified retained journal bytes
refuse recovery. This does not change compiler working-directory or
closure policy semantics, and does not normalize artifact hashes. Old rounds
without the new completion evidence can finish normally but are never reused or
promoted as artifact inputs.

Deploy matching Python coordinator, checker and native binaries, with the normal
acceptance identity verification and native closure identity migration checks.
Version 1 source-only pending journals remain recoverable. Version 2 recovery
requires the new `dispatch_publication_receipt` native action, the matching
checker and the retained publication journal; the original acceptance workspace
is no longer needed after durable preparation. Recovery queries the exact native
receipt before restoring artifacts so a completed native rebuild is preserved.
Retain the new matching deployment while any version 2 journal is pending: an
older runtime cannot safely recover it, so rolling back binaries must wait for
that transaction to finish. Never edit protocol state to bypass these checks.


Worker submit/check/repair rollout — 8 October 2026

This lifecycle is opt-in while the staged canaries are being validated. Set
`parallel_dispatch.worker_submission_policy` explicitly to
`{"version":1,"max_submissions":3,"max_infrastructure_retries":2}` for new runs.
Only Codex worker profiles currently support its exact-conversation continuation.
For an existing run, drain and stop the coordinator, install the matching
implementation, set the launch configuration, and run
`python3 -m trellis.dispatch configure-submissions <runtime>` with the ordinary
checkpoint hook. Native migration checks coordinator ownership, the captured
checkpoint and absence of occupied worker/verifier slots or pending exclusive
retries. Retained reviewer requests, routing queues and resolved nonexclusive
retry contexts are preserved; future assignments use the new policy. It records
a replayable policy event without rewriting historical requests or ratifying human gates. Explicit `null`
disables the policy for future tasks at the same drained boundary.

Worker time starts immediately before provider execution, after preparation and
launch permission. Corrections keep the original conversation, remaining allowance
and task reservation. Host checking, cleanup and publication spend none of that
allowance. Each completed turn has a separate immutable candidate; a repairable
host rejection authorizes a bounded correction. Exhausted turns with captured
JSON still receive host validation, without another worker turn. Declared Stuck or
Invalid results retain the ordinary terminal behavior. Historical resolved tasks
are never revived. The two-try lightweight dispatch limit remains independent of
the per-attempt submission limit.

Provider-budget pauses retain the same task, files and conversation. An explicit
operator resume starts another protected execution segment with only the original
allowance remaining; time spent paused is not charged. Transport retries use the
configured infrastructure retry cap. Missing conversation history or ambiguous
completion stops for recovery rather than silently starting a fresh conversation.

The supplied submit command performs cheap delivery checks. A real completed
provider turn and drained descendants precede source sealing. The host performs
the authoritative check; successful work needs no acknowledgment turn. Failed
checks return actual diagnostics to the same conversation. Prior resolved attempts'
retained sources and available diagnostics are offered to fresh same-node tasks
as unaccepted findings, never as accepted proof evidence.

Serial runs use the same lifecycle through the top-level
`worker_submission_policy` setting, without `parallel_dispatch`. They require
explicit Codex worker bindings, the enforced sandbox and the managed compiler
service. Historical requests retain their original contract. The serial
configuration and drained migration are described below.

Publication follow-up, sequenced after the submit/check/repair rollout

1. Reuse checked replay evidence across workspace locations with content binding
   to the source, dependencies, toolchain and checker inputs it actually checked.
   Preserve boundaries against workers or reviewers forging outputs within their
   writable scope; do not add defenses solely for outside filesystem tampering.
2. Record native acceptance and verifier read sets, compare them in shadow mode
   against current full validation, then replace whole-state invalidation. Re-run
   the cheap transition on current state and reuse expensive evidence only while
   its recorded reads hold. Packer task 36 and verifier 10 are regression cases.
   Do not add the separate proof-irrelevance relaxation.
3. Keep one synchronous checkpoint per accepted event. Prepare bytes outside the
   lock and move archive pushing off the critical path where ordering permits.
4. Defer immutable generations. These changes must not interrupt completion of
   the worker lifecycle and its canaries.

### Current-context publication refresh

Successful math ProofFormalization checks can record native observation receipts
for managed Lean closure tasks and ordinary Restructure tasks with no new
obligations or protected semantic changes. The ordinary path permits mechanically
closed helper additions with explicit empty paper claims and optional `easy`
difficulty, and authorized deletion of unclaimed, non-active ordinary helpers
outside the protected target scope. Other
phases, outcomes and structural changes continue through ordinary acceptance.

When only the acceptance context changes, the host authenticates the immutable
completed round and captures current artifact environments outside the publication
lock. The kernel then reconstructs the current request and reruns normalization,
legality and certificate composition using exact recorded observation operands,
source/config identities and imported artifact bytes. Final consumers are prepared
before recording the environments their fingerprint observations read. Earlier
validation environments must still match. The locked refresh cannot compile;
missing or changed evidence causes ordinary rechecking. Old completions without
these observations remain valid historical evidence but cannot supply a refresh.

The original completion remains intact. Fresh decisions are recorded under
`acceptance/<round>/refreshes`; context-only Valid attempts that cannot refresh
record a reason under
`refresh-misses`, alongside the existing publication rejection differences.
The real integration fixture's `--ordinary-refresh-helpers` and
`--ordinary-refresh-delete` variants exercise source-changing rechecks followed
by context-only refresh, including publication of fresh helper certificates and
removal of deleted helpers. They assert that refresh issues no checker request.

### Serial worker submission lifecycle

Serial mathematical and PV workers use the same opt-in submission policy at the
config root: `worker_submission_policy`. It requires the enforced sandbox,
managed compiler service, and explicit Codex model/effort for every configured
worker binding. Verifier and reviewer behavior stays unchanged.

For an existing serial run, stop the supervisor at an idle Start or Reviewer
boundary, set the policy (or explicit `null` to disable), and run
`python3 -m trellis.agent_wrapper.serial_worker configure <runtime>` with the
ordinary checkpoint hook configured. Native migration refuses active worker
lifecycles, verifier stages and human gates. It preserves mathematical state,
reviewer requests and rollback checkpoints; only future worker requests change.

Each serial request retains its provider conversation and original allowance in
`worker-attempts/<request>-<nonce>`. Provider execution is charged; launch waits,
manual quota pauses and host checks are not. Host correction does not consume a
new mathematical attempt. Exhausted submissions still receive host checking,
with no further correction turn. Ordinary terminal responses retain the existing
rollback and checkpoint behavior.

Native operational events use the normal event log with a small recovery journal;
they do not capture a new worker baseline or create source checkpoints. Repeated
launch/check delivery adopts owned receipts. A saved terminal result is reused
after checkpoint failure only while its checked sources remain current; a
rejected result also permits the native-restored baseline. See REWINDING.md before
abandoning a retained attempt.

### Compiler concurrency (8 October 2026)

The compiler-slot limit is a ceiling shared by authoritative and incremental
checks. Admission also uses current `MemAvailable`, a retained host floor, and
reservations for each running job's remaining growth. Only anonymous/shared
resident PSS earns credit; reclaimable file pages and duplicate process IDs do
not. A blocked workspace no longer blocks an eligible different workspace.
An aged large request reserves headroom after 30 seconds or eight bypasses.

Host-prepared semantic collectors can run in a bounded pool after the complete
artifact replay and collector preparation barriers. Warm local-closure requests
may also share a workspace. Actual source synchronization, materialization,
Lake builds and replay preparation remain exclusive there. Every subprocess
retains its exact workspace capability, global admission and cancellation.

`TRELLIS_CHECK_JOBS_PARALLELISM` must match the durable compiler capacity.
`TRELLIS_CHECK_JOBS_FLOOR_GIB` and `TRELLIS_CHECK_JOBS_RESERVE_GIB` specify host
headroom and the cold/unknown-job reservation. Prepared consumers use
`TRELLIS_CHECK_READONLY_PARALLELISM` (default 3 per workspace, bounded by global
capacity) and `TRELLIS_CHECK_READONLY_RESERVE_GIB` (default 14 GiB). After three
successful sampled probes in the same workspace and operation, admission uses
the observed peak plus 25% and 2 GiB, with a 6 GiB minimum. Interrupted/failed
samples only raise the estimate; source-only scans do not train full probes.
History is bounded. These are estimates and pressure control, not hard limits.

In the `available` memory policy, ordinary jobs use the configured hard cap
(default 48 GiB) and host floor (default 8 GiB), without reservation accounting.
A confirmed own-cap OOM or host-pressure stop requests exclusive recovery in
this checker's scheduler. Pending recovery blocks new compiler admissions,
waits for active work to drain, and evicts its own idle sessions. Other checkers
and runtimes are neither stopped nor modified; their use of RAM remains part
of measured host availability.

Recovery defaults are `TRELLIS_CHECK_SERIAL_MAX_JOB_GIB=56`,
`TRELLIS_CHECK_SERIAL_FLOOR_GIB=4`, and
`TRELLIS_CHECK_SERIAL_WAIT_SECONDS=300`. The target cap is bounded by physical
RAM minus the serial floor. After draining, admission also subtracts that floor
from current `MemAvailable` and from any finite enclosing cgroup's remaining
allowance. It waits for the full target until the admission deadline; only
then may it use a smaller cap, at least 2 GiB above the shared cap. The record
distinguishes `full_cap` from `deadline_fallback`. No 24 GiB reservation or
second host floor is added. Thus a 62.66 GiB host with 60 GiB available admits
56 GiB; with 58 GiB available it can admit 54 GiB after the wait expires.

The exclusive command uses `LEAN_NUM_THREADS=1`, a fixed per-job cgroup cap,
and the 4 GiB runtime pressure floor. Availability is checked again before
spawn. A serial own-cap failure ends recovery. A serial pressure stop can
relaunch at the same cap, up to three serial launches in total. Execution time
from all launches shares the original command allowance. Admission waits and
recovery share a persisted absolute outer deadline, initially two admission
waits plus the command allowance. Cancellation and incomplete cleanup still
prevent admission. Cold interrupted builds discard incomplete output and
re-enter normal preparation; replay retains the same target/artifact set.

Retry records are keyed by checked inputs and command, independent of private
workspace and acceptance-round names. An unknown launch outcome after restart
is infrastructure failure, not permission for an extra launch. Resource
diagnostics retain each executed cap, thread count, peaks, local compiler
overlap, cgroup events, and full target inventory. Success retains all ordinary
acceptance and attestation requirements.

Candidate serial own-cap exhaustion enters the existing same-conversation
worker correction path with measured costs and guidance about smaller proof
terms, checked helper lemmas, and permitted restructuring. It does not claim
proof invalidity. Baseline failure, unavailable admission, timeout, and
exhausted pressure relaunches use the existing bounded infrastructure retries
without consuming worker corrections. At that bound, the existing
`parallel_infrastructure` failure path retains the candidate and diagnostics;
it does not automatically requeue exhausted work. Non-restartable host helpers
likewise unwind to bounded infrastructure recovery; optional heartbeat
measurement remains unavailable on resource interruption.

On host pressure, one reporting compiler is stopped and reaped before new
admission. Authoritative resource/transport failures leave a host-owned record
for that acceptance round and use the existing bounded infrastructure retries;
they do not become a mathematical failure or spend a worker submission. Partial
output from unsuccessful compiler processes cannot certify a result. Diagnostic
logs include sampled tree RSS and reservation provenance; wrapper `wait4` RSS
is not a Lean peak measurement. The data connection limit is 128, with four
separate control slots so cleanup remains reachable under data saturation.

To resize compiler capacity or use the legacy worker-capacity events, drain workers, reviewer and verifiers and record
`worker_capacity_changed`, `sol_worker_capacity_changed` and
`compiler_capacity_changed` through the native dispatch control API, each with
its inspected expected value. Update the matching launch configuration and
checker environment before resuming. Omitted settings retain an existing run's
durable capacities; new launch defaults do not silently resize it.

The run viewer's **Parallel agents** and **Heavy agents** sliders change future worker admission through a native `agent_capacity_changed` event. Current use includes reserved tasks, provider work, checking and cleanup. Reducing a limit below current use preserves those tasks and blocks new reservations until capacity is available. The event checkpoints normally without changing LastClean or the mathematical state; its limits survive coordinator restart even when launch-time values differ.

Worker profiles explicitly declare `lightweight: true` (light) or `lightweight: false` (heavy). `heavy_worker_slots` uses this designation and supersedes the older `sol_worker_slots` model-name cap. All worker profiles must be designated before live controls can be used. Reviewer and global audit execution are separate; scoped audits and other verifier lanes use the verifier capacity. These worker controls do not change compiler admission limits.

### Bounded provider/checking overlap

`parallel_dispatch.max_in_flight_tasks` is an optional total unresolved-task
bound. Its absence keeps the historical policy above. When explicitly enabled,
`worker_slots` and the heavy/Sol cap limit provider reservations while the new
bound also includes tasks waiting for host acceptance. For example, five worker
slots and `max_in_flight_tasks: 8` allow at most eight unresolved tasks and at
most five provider reservations. Compiler limits and disk preparation admission
remain separate and unchanged.

Only a sealed submission with an owned provider completion releases provider
capacity. Its task scope, check reservation and total in-flight position remain
owned. Legacy tasks retain their provider reservation through cleanup. An
invalid submission may return to its original task for repair; it queues until
its model has capacity, then atomically reacquires a provider reservation before
permission is granted. Completed checks do not reacquire provider capacity while
waiting for cleanup. Global transitions and human gates still require all tasks
to resolve. Ordinary exclusive checks still block reviewer batch delivery via
the finalization gate, so this setting does not eliminate every idle interval.

To enable or disable on an existing run, first drain and stop its coordinator.
Set an explicit `max_in_flight_tasks` integer (at least `worker_slots`), or `null`
to disable, in the run configuration. Using the matching deployed framework and
kernel environment, run:

```
python -m trellis.dispatch configure-pipeline /path/to/runtime
```

This command owns the coordinator, sidecar and native locks and records a
compare-and-swap `execution_pipeline_changed` event through the normal checkpoint
hook. It refuses live tasks, reviewers and verifiers. Resume normally afterwards.
Rollback requires disabling through this event using a binary that understands
it; changing old event history or downgrading to a binary without the event is
not supported. In pipeline mode, live worker/heavy reductions below current
provider reservations are refused until those reservations reach checking or
finish, rather than grandfathering a temporarily over-capacity pool.

Read-only scheduling polls share an unchanged native projection for at most
30 seconds. State/configuration/metadata changes and source/evidence/referent
changes invalidate it immediately. Every native event and authority check still
loads current state; pause, halt, publication recovery and human controls run
on every poll. `dispatch/telemetry.json` reports `scheduling.cache_hits`,
`native_refreshes` and `max_age_seconds` to measure actual saved reads.

### Acceptance freshness (9 October 2026)

Worker concurrency and host-check concurrency have separate conflict rules.
The host records a semantic input reservation before preparing a check and
retains it through publication. Independent checks may share immutable reads;
a source writer waits for checks that read or write that source. Waiting work
recaptures its inputs after admission. Restart restores surviving reservations
before admitting new checks. Unknown or structural check scopes are exclusive.

For supported math proof tasks, the first stage records checked owner observations,
not an acceptance verdict. Its reservation covers owned sources and their actual
imports; two proofs with a common downstream consumer can be checked together.
The driver retains that reservation while a separate finalization gate selects
the latest canonical sources, applies the candidate delta, and checks affected
consumers, target observations and guards. This gate also covers legacy publication.
No owner reservation is released or upgraded while waiting for finalization.
Import additions and authorized helpers use the current native scope rules.
Unsupported operations retain the conservative complete footprint or an exclusive
check. Every observed import must fit the admitted footprint.
`INDEX.md` and `README.md` are generated support, not semantic freshness inputs.
Registered node files outside the native read set may change independently;
headers, configuration, reference inputs and host checker carriers remain bound.
Host carriers are recorded separately from worker-writable candidate paths.

An authenticated completed result can be reconstructed on current state only
through the native typed delta and ordinary transition checks. This preserves
current sibling observations and rejects conflicting changes. Publication
applies the checked source delta to current sources, regenerates shared support,
and uses the existing source-backed certificate issuer and durable journal.
It does not replace the canonical tablet with an old private checkout. Typed owner
facts bind exact source and imported artifact inputs, including definitions and
secondary declarations. This design does not assume that a file containing a
theorem can change freely when its principal statement is unchanged. True import
conflicts are sequenced at host checking, while worker execution remains parallel.

Owned-stage completion and each current finalization have separate immutable
receipts. A finalizer resumes after a coordinator restart using the same completed
owner facts. Administrative changes reuse the completed finalization after a
native state comparison and transition check. Verifier verdict publication shares
the finalization gate, so a new approval cannot change a guard while it is being
checked; verifier execution remains concurrent. Ordinary failed proof
checks return their actual diagnostics through the existing submit/check/repair
lifecycle. Infrastructure failures consume its existing bounded retry allowance.

The scheduler does not silently run another full host round after a freshness
conflict. It retains the completed evidence and records an operational pause
for reconciliation. Bounded infrastructure retries are unchanged. A closure
worker's scope-valid Stuck result publishes no source changes: partial edits
remain sealed as unaccepted findings and do not require full proof checking.

The real-Lean canaries `--semantic-reservations` and `--dependent-acceptance`
exercise overlapping independent checks and sequenced dependent checks. Both
require one host round per candidate and two native publications. Deployment
validation must additionally measure real run workloads; safe serialization
alone does not establish the requested performance improvement. The
`--staged-shared-consumer` and `--staged-helpers` fixtures check overlapping owner
stages, one finalization per candidate, and preservation of both publications
when downstream consumers are shared, including newly introduced helpers.

### Checker delegation preflight for a rollout

Before stopping a healthy checker, run the following from the intended frozen
source directory with the same interpreter and memory-policy environment as
its replacement:

```sh
TRELLIS_CHECK_CGROUP=delegated python3 -m trellis.checker.delegation
```

This command enters a fresh delegated user scope, initializes the actual
compiler-group controls, creates and closes an empty bounded compiler group,
and exits successfully only after those operations succeed. It does not open
or modify a runtime, its sockets, or its durable state. Its JSON result records
`ready`, the PID, and the delegated cgroup. Save the exit status and result as
rollout evidence; a failed preflight must leave the old checker running.
This checks cgroup admission, not source correctness, socket health, or the full
memory policy. The replacement still needs its normal health and policy checks.

A writable delegated cgroup is not sufficient: it must contain only the
checker and no populated descendants. `Popen(start_new_session=True)` isolates
sessions, not cgroups. The checker bootstrap enters a separate scope when its
launcher's delegated service is shared; the final compiler-group guard remains
in force. An inherited bootstrap marker belongs to its original PID and does
not suppress a child checker's own delegation attempt.

For a supervised deployment, give the checker and coordinator independent
units rather than placing detached children in a short-lived rollout unit.
Use a checker service with `Delegate=memory`, `OOMPolicy=continue`, and
`KillMode=control-group`. The coordinator's lifecycle must preserve its workers
on coordinator exit (the existing parallel runtime uses `KillMode=process`).
A successful scope preflight does not validate those service lifecycle rules:
exercise the intended dedicated checker unit before replacing a healthy
service, and verify its own cgroup, health, and identity after launch. Keep the
rollout failure visible until a healthy checker and coordinator are confirmed;
logging a failed rollout does not itself provide automatic recovery.

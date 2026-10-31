## Conditional theorem proposal

For an unrestricted Decide target that remains open, you may emit one target-bound
`conditional_theorem_proposal`. Its fields:

* `target_id` — the Decide primary target id, copied exactly from
  `configured_challenge_targets`.
* `condition_lean` — a complete Lean `Prop` under the target's registered binders
  that excludes the input class you are ruling out and nothing else; the kernel
  rejects trivially true or false conditions.
* `condition_informal` and `rationale` — the condition's meaning, and why the
  conditional theorem is the strongest established result (for a physically
  unrealizable class: the hardware property the excluded inputs demand).
* `trigger` — `unconditional_not_established` when a verified refutation exists
  outside the condition; `model_counterexample_mismatch` or
  `assumptions_model_gap` for a model/source or assumption-set discrepancy.
* `evidence.disproof_sha256` — whenever a refutation witness motivates the
  condition: the SHA-256 hex digest of the `__Refutation` target's exact `lean`
  string as stored in the request file's `configured_challenge_targets`, e.g.
  `python3 -c 'import json,hashlib,sys; d=json.load(open(sys.argv[1])); print(hashlib.sha256(d["configured_challenge_targets"][sys.argv[2]]["lean"].encode()).hexdigest())' <request.json> <refutation target id>`.
* `concrete_counterexample_arguments` — Lean argument terms for that witness under
  the target's binders; the kernel seals `¬ C(witness)` as a Lean obligation from
  them, so the condition provably excludes the recorded counterexample.

A response that carries a proposal leaves `set_live_polarity` empty: the proposal
keeps both unconditional sides open. The kernel stamps the proposal, routes it
through correspondence, seals `C → T`, the inhabitation of `C`, and the witness
exclusion, and presents the packet for human ratification at the gate.

During theorem stating, sealed proposals for different targets accumulate for
one ordinary Advance approval of the complete batch. Their original sides stay
unresolved. Approval opens the generated proof obligations; it establishes no
conditional result until those obligations close.

Proposal submission and proof completion are distinct stages. Do not require a
prior Sound Pass merely to submit a conditional statement for its dedicated
correspondence review. Inspect `sound_dispatch_readiness`: an unresolved
unconditional statement failure can block ordinary Sound on the support package.
During theorem stating, an exact sealed CorrespondencePass conditional candidate
moves its original sides out of ordinary routing while retaining their failures
and unresolved truth status. Use this existing staging order when the evidence
justifies a proposal; never describe unverified supporting lemmas as established.
If the evidence is insufficient, identify the missing mathematical work instead
of prescribing a verifier transition that the retained failures prevent.

Inspect `request_summary.conditional_ratification_packets` (also on the request)
for existing sealed, correspondence-passed candidates awaiting human ratification.
Continue their remaining proof work and gate workflow; do not submit the same
proposal merely because an older audit requested staging it. Submit a replacement
only when your diagnosis calls for a revised candidate, and explain the change:
a replacement supersedes the old generation and requires fresh correspondence.
These packets establish neither proof completion nor human approval.

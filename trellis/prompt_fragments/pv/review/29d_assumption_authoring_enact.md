# PV under-model assumption authoring is ENACTABLE this turn

The most recent audit named a candidate under-model assumption `C` and its plan
recommends admitting it through the PV assumption-authoring lane. The exact
candidate text is in your structured contract at
`assumption_authoring_contract.candidate_invariant`.

To act on that recommendation, emit a **non-acting Continue** with:

```json
{"decision": "continue", "reset": "none", "assumption_authoring_request": true}
```

with every action field empty (no `next_active`, no `authorized_node_ids`, no
blocker action lists, no `audit_request`). In parallel dispatch, put this
response in one `ordinary` assignment's `review` with `compiler: "managed"`,
not in `action`. Wait for current tasks to finish and submit it alone: the
kernel reserves an exclusive assumption-authoring worker and determines its
scope. In serial runs, return the ordinary response directly. The worker
stages `C` as an axiom in `Tablet/Assumptions.{lean,tex}`, after which the
ordinary correspondence lane checks it. Environment premises also require
Soundness approval of their preservation argument. The assumptions lane judges
eligibility, followed by human ratification. Additions during formalization
reach that human gate before further proof work.

Rules that keep the trust chain intact:

- **You cannot restate, narrow, or widen `C`.** The candidate is
  auditor-owned; your only move is to enact it (or not).
- **Never route `next_active` to the `Assumptions` node.** That is rejected in
  every mode — the node is not ordinary work; a worker can only reach it
  through this authoring dispatch. Attempting it re-issues your review with a
  rejection reason.
- Enacting is a routing decision like any other: if you judge the audit's
  recommendation premature (e.g. an ordinary proof route remains untried),
  you may instead continue normal work or dismiss the audit task with a
  reason. The candidate stays enactable while its audit plan is live.

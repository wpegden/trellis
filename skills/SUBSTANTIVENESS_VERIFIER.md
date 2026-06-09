# Substantiveness Verifier Skill

This skill applies only when handling `RequestKind::Paper` requests in the per-node scenario — i.e. the request carries a non-empty `substantiveness_verify_nodes` and an empty `paper_verify_targets`. The lane runs in TheoremStating only.

The emitted prompt is authoritative. Treat the kernel-authored contract, rubric, and `node_paper_basis_inputs_json` as the full workflow contract.

## What This Lane Verifies

For each node on the frontier, render Pass / Fail / NotDoneYet. The node passes iff:

1. The node's NL content genuinely states or defines something the paper actually uses (explicitly or implicitly), at the full strength necessary for that purpose [so: we expect proofs from and of this node to be feasible].

2. The node's NL content is not essentially the same as or subsumed by the meaning of any other node [so: we expect no proofs from or of this node to be vacuous].

You are verifying that the node will represent a valid but also meaningful decomposition of the paper's proof.

**Note on clause 2 (case distinction).** Clause 2 implies that no node's content should just repackage or trivially imply the content of another single node. However, it is acceptable for one node's content to trivially follow from *several* others when those others correspond to meaningfully different cases of the aggregator's claim. The aggregator is meaningful even though its proof from the cases is trivial, and each case is meaningful even though it covers only part of the aggregator's content.

Strengthening or correcting a paper claim is allowed and counts as Pass.

## Triage With Explicit Verdicts

The kernel sends you the entire outstanding Unknown set in a single request, plus the full paper. You triage by emitting one explicit verdict per node:

- `Pass` — you read the node carefully and it satisfies both clauses. Comment optional.
- `Fail` — you read the node carefully and it fails clause 1 or clause 2. **Must include `comment` with a concrete next-step recommendation.**
- `NotDoneYet` — you didn't have time to evaluate carefully. Comment optional.

**Silence is treated as NotDoneYet.** Every node listed in `request_summary.nodes` should appear in `verdicts[]`. A node omitted from `verdicts[]` is treated as `NotDoneYet` by default — do not rely on omission to mark Pass.

The kernel re-issues another Paper request covering exactly the `NotDoneYet` residual (including missing-from-response nodes that defaulted to NotDoneYet) until empty, subject to a `substantiveness_max_consecutive_no_progress` safety bound (default 5).

Do **not** Pass or Fail a node you have not read carefully. NotDoneYet is the right answer.

## Reading the Paper

The verifier prompt has `read_files` permission. The configured paper file is documented in `node_paper_basis_inputs_json`. Read it directly; the kernel does not pre-extract paper content for you.

The `node_paper_basis_inputs_json` block gives you, per node:
- `tex_path`: path to the node's `.tex` file (the candidate statement).
- `lean_path`: path to the Lean file (only consult for context if you need it).
- `imported_by`: nodes that import this one — useful for judging downstream usability.
- `node_kind`: `preamble` / `definition` / `proof`.

## Output Discipline

- Use the deterministic checker command named in the prompt (`substantiveness-result`).
- Render `decision: PASS` iff every verdict is Pass-or-NotDoneYet; otherwise `FAIL`. NotDoneYet alone does not Fail the lane.
- Every node from `request_summary.nodes` should appear in `verdicts[]` with an explicit verdict. Missing nodes are treated as NotDoneYet.
- Every Fail must carry a non-empty `comment`. Comments on Pass and NotDoneYet are optional.
- Do not append `[NotDoneYet]` to node ids — that suffix hack is retired. The verdict goes in its own field.

## Boundaries

- Not corr (Lean-vs-TeX). Not soundness (proof rigor). This lane is `.tex`-vs-paper at the node level.
- Do not Pass a node "because I think it's harmless" without a paper basis.

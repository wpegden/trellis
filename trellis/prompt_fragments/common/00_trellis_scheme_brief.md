## Trellis formalization scheme

Recall that Trellis formalizes a natural-language math paper by maintaining a proof-tablet DAG in `Tablet/`. Each node is a paired Lean/NL result (`.lean` + `.tex`), and imports encode the support structure.

The system therefore cares about structure, not only local success. A good tablet exposes the paper's real subarguments, uses intermediate nodes to make support explicit, and makes later formalization easier.

The process is supervised and multi-cycle. In theorem-stating, workers act in global or targeted mode to build or repair a paper-faithful DAG, proof-bearing nodes typically still contain Lean `sorry`s, paper-faithfulness verifiers first check whether the covering node statements collectively capture each paper target, substantiveness verifiers then check that each per-node statement is a meaningful decomposition step, correspondence verifiers check Lean-vs-TeX statement alignment on individual nodes, soundness verifiers check the active NL proof target, and the reviewer chooses the next legal move under the current contract. In proof-formalization, the system works node by node to replace those provisional Lean proofs with complete Lean proofs without losing the vetted mathematical structure established earlier. In cleanup, the system performs constrained cleanup/hygiene work without reopening the semantic structure arbitrarily.

Several invariants are always in force. Every present node should have both Lean and NL artifacts. Lean and NL statements must genuinely correspond. Proof-bearing nodes must end either in a complete Lean proof or in a rigorous, line-by-line checkable NL proof supported by their dependencies; the detail level of the original-paper proofs are a floor for the detail of these tablet NL proofs. Definitions should have real bodies, imports should reflect real support, and project structure should make genuine progress on formalizing the paper rather than merely repackaging targets.

Role boundaries matter. Workers edit repository content only. Paper-faithfulness verifiers judge target-level NL coverage, substantiveness verifiers judge per-node meaningful-decomposition, correspondence verifiers judge node-level Lean-vs-TeX alignment, and soundness verifiers judge NL proof rigor; verifiers do not choose the next task. Reviewers choose the next step and guidance using the contract's legal decision vocabulary; they do not directly rewrite the repository. Human or expert review happens only at explicit gates. Deterministic kernel-authored contracts, not free-form prompt improvisation, decide what is accepted.

For the fuller end-to-end scheme, read `{{trellis_scheme_reference_path}}`.

Canonical rubric definitions for the four lanes — referenced by workers, reviewers, and verifiers alike — live at the project root:

- `FAITHFULNESS.md` — paper-target faithfulness via the covering set.
- `SUBSTANTIVENESS.md` — per-node paper-substantive decomposition.
- `CORRESPONDENCE.md` — per-node Lean-vs-TeX statement alignment.
- `SOUNDNESS.md` — NL-proof rigor with paper-detail-as-floor.

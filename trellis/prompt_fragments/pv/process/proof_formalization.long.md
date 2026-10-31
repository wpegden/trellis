## Proof formalization

Proof formalization replaces provisional Lean proofs with complete proofs over the vetted DAG. Work centers on an active node, with the reviewer selecting the permitted edit scope and closure conditions.

Workers translate the checked arguments into Lean and resolve the details needed for kernel acceptance. Supporting lemmas and broader restructuring may be introduced through authorized scopes. Changes to statements or dependencies trigger the relevant verification lanes, keeping the development aligned with its NL account and the pursued goals.

The audit may change the selected direction when the evidence warrants it. The live statement and its NL account receive the applicable semantic and argument checks.

The system enters cleanup when every pursued goal is addressed by a proof or disproof and every supporting proof is `sorry`-free with a successful closure check valid for its current statement, proof, dependencies, and permitted axioms. The complete development compiles, goal-faithfulness, substantiveness, and correspondence approvals are current, and no unresolved verification findings remain.

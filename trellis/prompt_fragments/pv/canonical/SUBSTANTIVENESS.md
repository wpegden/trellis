# Substantiveness

A node *N* is **substantive** iff both of the following hold:

**Clause 1**
The node's NL content genuinely states or defines something the verification actually requires — an obligation in `GOAL.md` or a genuinely-needed supporting fact about the pinned model — at the strength `GOAL.md` and the verified Rust require: strong enough to support the target contract, and not stronger in any way `GOAL.md`/the Rust do not justify. Record `FalseAsStated` when such a candidate is refuted under the pinned extraction model. Record `Fail` for unsuitable statements, including malformed claims and unjustified changes of strength or domain.

**Clause 2**
The node's NL content is not essentially the same as, or subsumed by the meaning of, any other single node. Thus: Proofs from or of this node should not be vacuous or trivial.

Note that Clause 2 is a defense against procrastination-through-wrapping: closing a node by introducing a new node packaging the necessary work and from which the now-closed node can get a very short/trivial proof. This kind of enrichment of the DAG is not productive and forbidden by Substantiveness. In particular, verifying that Clause 2 is not violated requires checking against the content of every node importing the node *N*.

## Note on cases

Clause 2 implies that no single node should repackage or trivially imply another single node's content. The aggregator carve-out (an aggregating node following trivially from *several* meaningfully-different-case nodes) is the math multi-case pattern and generally does not arise for program verification: the goal theorems are per-function `GOAL.md` obligations, not case splits of a single claim. If a genuine multi-case aggregation does arise, the same rule applies — the aggregator and each case all count as substantive even though the aggregator's proof from the cases is trivial.

A node that states a **declared verification-target obligation** — a Spec/Safety/Correctness/Invariant node carrying a `GOAL.md` obligation in prose-goal mode, or a pinned goal statement in Lean mode — is substantive by virtue of being a declared target, and is NOT a Clause-2 wrapper, even when its postcondition is `True` (a Safety/totality triple) or it sits above another goal node. Safety and Correctness over the same function are distinct obligations: a Safety triple `⦃ ret => True ⦄` asserts `ok`-ness (no panic/overflow, every step returns), while the Correctness triple asserts the output value; neither subsumes the other. Clause 2's procrastination defense targets worker-INVENTED intermediate nodes that trivially repackage another node, not the per-function obligations the run is commissioned to prove. In Lean mode the goal statements are pinned targets, not authored wrappers, so the wrapper question only ever arises for worker-introduced helper nodes.

A statement that invokes another theorem-like node's hypotheses by citation fails substantiveness (see FILESPEC).

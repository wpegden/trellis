# Substantiveness

A node *N* is **substantive** iff both of the following hold:

**Clause 1**
The node's NL content genuinely states or defines something the paper actually uses (explicitly or implicitly), at a paper-justified strength appropriate for that purpose: strong enough to support the paper's use, and not stronger in any way the paper does not justify. Thus: Proofs *from* and *of* this node should be feasible, and a statement that is technically false as written (e.g., because of insufficient hypotheses) should fail substantiveness.

**Clause 2**
The node's NL content is not essentially the same as, or subsumed by the meaning of, any other single node. Thus: Proofs from or of this node should not be vacuous or trivial.

Note that Clause 2 is a defense against procrastination-through-wrapping: closing a node by introducing a new node packaging the necessary work and from which the now-closed node can get a very short/trivial proof. This kind of enrichment of the DAG is not productive and forbidden by Substantiveness. In particular, verifying that Clause 2 is not violated requires checking against the content of every node importing the node *N*.

## Note on cases

Clause 2 implies that no single node should repackage or trivially imply another single node's content. However, it is acceptable for an aggregating node to follow trivially from *several* others when those others correspond to meaningfully different cases of the aggregator's claim. The example to keep in mind: a theorem covering multiple cases, whose individual cases are packaged as multiple, meaningfully different nodes. The aggregator is meaningful even though its proof from the cases is trivial; each case is meaningful even though it covers only part of the aggregator's content.

A statement that invokes another theorem-like node's hypotheses by citation fails substantiveness (see FILESPEC).

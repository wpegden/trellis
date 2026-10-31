# Extraction model

An `ExtractionModel` node (the `extraction_model` role) is the machine-extracted Lean model
of a Rust declaration, produced by the extractor (Charon then Aeneas) and pinned by the kernel
byte-for-byte. It is the trusted given the whole verification rests on: every spec, safety,
correctness, and invariant obligation is judged true or false relative to these nodes.

**Identity.** A model node's identity is its byte-exact declaration slice (the marker-to-EOF
region) **together with** its namespace context (the extractor-emitted `namespace <crate>`).
Both are immutable. The slice fixes the declaration's value; the namespace fixes its
fully-qualified name (`<crate>.<Name>`). Altering either changes what is being verified and breaks
the trust chain — stripping the namespace silently re-identifies a trusted declaration as a
different, top-level declaration. The kernel rejects any edit that changes either part.

**What is mutable.** Only the `import` and `set_option` lines in a model node's preamble. Nothing
that changes the declaration's value or its fully-qualified name. The `namespace` line is part of
the pinned identity, not free preamble.

**Idiom.** To reference a model declaration by its short name from a spec or proof node, write
`open <crate>` in **your** node's preamble. Never strip, rename, or re-namespace the model node to
make a name resolve.

**Role obligations.**

- **Worker:** never edit a model node's declaration or its namespace. When a name does not resolve,
  the fix is `open <crate>` in the consuming node, never an edit to the model.
- **Reviewer:** never direct, authorize, or bless an edit to a model node. Do not place model nodes
  in `authorized_node_ids` or `protected_semantic_change_node_ids`, and do not instruct the worker
  to change a model node's declaration or namespace. If a proof cannot resolve a model name, the
  repair you direct is `open <crate>` in the consuming node.
- **Verifier:** judge every node against the model as inviolate ground truth; the model's pinned
  semantics are the ground facts, never to be second-guessed or treated as adjustable.

The kernel deterministically rejects any identity-altering edit to a model node at acceptance. This
policy is stated as well as enforced because a reviewer's instruction fires before that commit-time
gate: the gate would bounce the worker's edit, but only this rule stops the wrong instruction from
being issued in the first place.

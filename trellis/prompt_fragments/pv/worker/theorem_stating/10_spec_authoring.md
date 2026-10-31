# Authoring a Spec node

A `Spec` node states the property the verified function is meant to have, as a
precondition/postcondition contract over the pinned model. It is the protected statement
the whole verification certifies, so author it at the full strength of the intended
property.

Write the spec as a Hoare triple `f args ⦃ ret => P ret ⦄` whose subject `f` is the
pinned `ExtractionModel` declaration imported into this node:
- The postcondition `P ret` names the returned value and constrains it to the intended
  property at full strength — an equality where the property is exact, the specific
  congruence or value where that is the intent.
- The precondition hypotheses in the signature carry exactly the input conditions the
  function needs, including the bounds that keep arithmetic in range, and no narrower.
  A precondition that needs `p > 0` to keep a modulus well-defined is a real obligation;
  a precondition that narrows the domain further restricts what is certified.

Pair the node with a natural-language `.tex` gloss stating the same contract in prose:
the input domain, the result, and the property. The gloss is the correspondence and
critic referent, so it must say what the Lean statement says.

A change to an approved Spec statement reopens the bound theorems and raises the
protected re-approval gate; state the spec correctly the first time.

## Deciding a Decide target

A Decide target (`resolution: decide` in `configured_challenge_targets`) is
resolved by proving `T` or by disproving it through its kernel-pinned exact
negation `<name>__Refutation`. For a Decide target you are the sole authority
that decides the truth and steers the worker between the two directions.

When your investigation concludes the goal statement is false against the
byte-pinned model, disprove it once no live node imports or hypothesizes `T`:
set `set_live_polarity: "disprove"`, making `<name>__Refutation` the live node,
and carry the offending input class and failure branch in your `report`. To
return the positive `<name>` node to live, set `set_live_polarity: "prove"`.

While a live node still imports or hypothesizes `T`, the disprove is one
precondition away. Leave `set_live_polarity` empty and give focused recovery
tasks that re-found each such importer off `T` — a success-conditioned residue
lemma (what remains true of `T`'s subject on its `ok` paths), or a flip of an
importer that is itself false. Disprove `T` in a later audit once nothing live
imports it. Vacuous closure of a pinned implication needs explicit adjudication
in your report.

`set_live_polarity` binds to the active/held pair; when the pair to flip is
not the active/held node, name it in `set_live_polarity_target` using its
primary challenge target id. Only these structured fields are enactable. For
an ordinary fixable obstruction, leave `set_live_polarity` empty and respond
with the usual report plus focused recovery tasks.

Before proposing a change, inspect
`request_summary.closed_conditional_polarity_targets`. Current policy pins the
polarity of a target whose ratified conditional result has closed. Leave
`set_live_polarity` empty for those targets. Preserve the conditional result and
the unresolved status of both unrestricted sides; conditional closure is not an
unconditional proof or disproof. Recommend remaining work for other targets.

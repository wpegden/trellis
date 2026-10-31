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

For every explicit `set_live_polarity` selection, set
`set_live_polarity_target` to the primary challenge target id, including when
it is the active/held pair. Only these structured fields are enactable. For
an ordinary fixable obstruction, leave `set_live_polarity` empty and respond
with the usual report plus focused recovery tasks.

A conditional companion is independent of the original direction. Adding or
closing it neither clears an existing disproof selection nor forbids a later
selection. An explicit selection, even of the current direction, resumes the
unrestricted work and requires checked closure before completion. Without such
a selection, conditional-only completion may disclose the original unresolved.
Selection preserves approved goal meanings and conditional seals; it does not
request another human gate. Actual edits to approved meanings still need review.

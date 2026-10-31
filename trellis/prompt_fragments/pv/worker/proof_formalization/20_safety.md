# Authoring a Safety node

A `Safety` node certifies panic-freedom: under its preconditions, the model function
returns `ok`. The generated model encodes each panic path — an
arithmetic overflow, a narrowing cast, a slice index, a division — as a `Result.fail`
branch, so a Safety theorem is a Hoare triple `f args ⦃ ret => True ⦄` (or carrying the
functional postcondition alongside) whose force is that the triple holds at all: `f args`
steps to `ok`.

State the Safety node over the pinned model declaration, with preconditions covering
exactly the failure paths the model carries. Step through the model with `step`; each
`Result`-producing operation raises a side goal that its failure branch is not taken
(`step` discharges the routine ones through `scalar_tac`).

When `step` cannot discharge a side goal on its own, it is a problem-specific bound — a
product that must stay below the integer maximum, a narrowing cast whose source is in
range, an index below the slice length. Land that bound as a `have` first so the
arithmetic automation picks it up, then take the step. A widening cast (`U32 → U64`) is a
`lift`ed operation carrying an in-bounds obligation; supply its cast spec explicitly
(`step with UScalar.cast_inBounds_spec`). A narrowing cast back (`U64 → U32`) is total,
so its obligation is value-preservation under the `< 2^width` bound.

The kernel surfaces a Safety node when a model carries a `%`, a cast, an index, or an
arithmetic op whose `Result.fail` branch is reachable; close the no-fail obligation for
each such operation the node covers.

# Program-verification authoring

This tablet verifies a Rust function through its byte-pinned generated Lean model. The
extractor (Charon then Aeneas) produces a Lean model of the function; the kernel pins
each generated declaration byte-for-byte. You author the surrounding proof obligations
against that pinned model.

The model nodes (`ExtractionModel`) are inviolate — their pinned identity and the
`open <crate>` idiom for referencing them are stated in the Extraction-model canonical doc.
Direct all authoring to the spec and proof nodes that import a model node — `Spec`, `Safety`,
`Invariant`, and `Correctness`.

The generated model uses the Aeneas `Result` monad: a model function returns `ok v` on
success, `Result.fail` on a panic (overflow, out-of-bounds index, division by zero), and
`Result.div` for non-termination. A loop is extracted as a `loop`-combinator trio
(`f_loop.body`, `f_loop`, and a `@[reducible] def f`). You
state properties as Hoare triples `f args ⦃ ret => P ret ⦄`: under the preconditions in
the signature, `f args` returns `ok` with a result satisfying `P`.

Every accepted proof must report `#print axioms` as exactly
`[propext, Classical.choice, Quot.sound]`. Keep proofs within that surface: the closure
probe fails closed on any other axiom, including the `sorry`-backed support specs (for
example `Slice.get_unchecked`), so reach for the bounds-checked indexer
`Slice.index_usize`.

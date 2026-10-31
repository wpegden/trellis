# PV correspondence

The NL statement is about the Rust program. The Lean statement is about the
extracted model. Correspondence asks whether the Lean, read over model
artifacts, genuinely means precisely what the NL says about the Rust.

The Rust source is available. Use it to establish what a Rust entity means —
the domain of a type, the behavior of a standard-library primitive the model
leaves opaque. Body extraction is pinned by provenance. When present,
`reachable_opaque_inventory` discloses the reachable primitive signatures,
call sites, and reachability edges for this judgment.

That gap between vocabularies is where correspondence failures live. Some ways
it opens:

- The theorem's subject is a Lean function resembling the extracted declaration
  rather than the pinned declaration itself.
- A model type admits values its Rust type cannot; where the NL means Rust
  entities, the Lean restricts to them, and a validity definition names that
  restriction. The definition carries the claim with the statement: it admits
  the values Rust produces and excludes the rest.
- The model encodes a panic as `Result.fail`, so panic-freedom is an `ok` claim
  over the pinned declaration.
- The subject is an opaque extraction stub, so the property is about nothing.

Judge the gap itself; these are instances of it.

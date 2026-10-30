## HOL library usage

Formalize consistently with the Isabelle/HOL library conventions for definitions and types. Build on `Main` and the `HOL` session (`HOL.List`, `HOL.Real`, and the rest), and on `Complex_Main`, which reaches `HOL.Binomial`, `HOL.Transcendental`, and `HOL.Series`, rather than re-deriving standard material; a proof that leans heavily on the library is just as good as one that does not.

Discover the facts you need from inside the prover session:

- `find_theorems` locates existing library and tablet facts by the shape of their conclusion or by a constant they mention, for example `find_theorems "_ + _ = _ + _"` or `find_theorems name: "comm" "_ * _"`. Search one shape at a time.
- `find_consts` locates a constant by its type when you know the type better than the name.
- `sledgehammer` searches the library for a proof of the current goal and reports the facts and method that close it; run it on a leaf goal and ship the method it reports.

Match the existing namespace, fact naming, and notation conventions of the library and the surrounding tablet. Keep the `imports` clause to the session roots and the sibling-node imports the node actually needs. Preserve manuscript provenance in a comment, for example `(* Paper Lemma 2.3 *)`.

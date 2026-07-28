Node file format reference: `{{filespec_path}}`

If you are reading or editing `Tablet/*.lean`, `Tablet/*.tex`, `Tablet/Preamble.*`, or `Tablet/header.tex`, consult that file before making changes. Deterministic checks enforce the structural parts of the file spec, so do not improvise around it.

Front-load these failure-prone rules:

- Every ordinary `Tablet/<Node>.lean` file must contain **exactly one** line whose trimmed content is `-- BODY`. That comment line is the statement/body boundary marker. It must sit directly after the principal declaration's body delimiter (`:= by`, `:= <term>`, or `where` for a `structure`/`inductive`/`class` data declaration) and before the start of the body. The kernel rejects worker output whose changed nodes don't satisfy this. Canonical shapes (tactic mode, term mode, where-form data declaration, instance):
  ```lean
  theorem Foo : Statement := by
  -- BODY
    tactic1
    tactic2
  ```
  ```lean
  def Foo : T :=
  -- BODY
    some_term
  ```
  ```lean
  structure Foo extends Base where
  -- BODY
    field1 : T1
    field2 : T2
  ```
  ```lean
  inductive Foo : Type where
  -- BODY
    | c1 : Foo
    | c2 : T → Foo
  ```
  ```lean
  instance Foo : ClassName Carrier where
  -- BODY
    law1 := by sorry
  ```
- An ordinary node `.tex` file must be exactly one top-level `definition` block, or exactly one theorem-like block immediately followed by exactly one `proof` block.
- Do not put free top-level prose, `\section` commands, or extra theorem environments in an ordinary node `.tex` file.
- If a file wants multiple top-level claims, split it into multiple nodes instead of stacking them into one file.
- Lean declaration families matter: definitions use `def`, `abbrev`, `noncomputable def`, or the data declarations `structure`/`inductive`/`class`; proof-bearing nodes use `theorem`, `lemma`, or `instance`. An `inductive` must use the `where` form, and an `instance` must be named and pairs to a `lemma`-style `.tex` claim.
- Definitions must not wrap or alias theorem statements. `def Foo : Prop := <statement>` is forbidden whether it stands alone as a definition node or is referenced by `theorem Bar : Foo := sorry` in a sibling node. Theorem signatures must contain the actual mathematical statement inline, using data definitions and predicates as building blocks.
- `.tex` categories like `corollary` and `helper` are statement-environment categories, not separate Lean declaration keywords.
- After cycle 1, new proof-bearing nodes may not use `SKETCH:` as the first nonblank line of their `.tex` proof block. If a worker cannot write a complete NL proof for a new theorem, lemma, corollary, or helper node that it believes will pass strict soundness verification, it should not create that node. The worker checker rejects this deterministically.
- `Tablet/*.lean` files use spaces: a literal tab character anywhere makes Lean reject the file, so convert any tabs your patch tool inserts to spaces before building.
- Set a node's `maxHeartbeats` with a standalone `set_option maxHeartbeats N` in the preamble, above the `-- [TABLET NODE: ...]` marker, so the budget stays outside the hashed signature and can be retuned without a statement edit. If the budget is needed mainly because the node has grown large, prefer splitting it into helper nodes when that is a suitable fix.

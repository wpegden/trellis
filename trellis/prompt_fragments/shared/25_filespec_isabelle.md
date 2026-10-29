Node file format reference: `{{filespec_path}}`

If you are reading or editing `Tablet/*.thy`, `Tablet/*.tex`, `Tablet/Preamble.*`, or `Tablet/header.tex`, consult that file before making changes. Deterministic checks enforce the structural parts of the file spec, so do not improvise around it.

Front-load these failure-prone rules:

- Every ordinary `Tablet/<Node>.thy` is one Isabelle theory named `Tablet_<Node>` that `imports Tablet_Preamble` (directly or via another node) and contains **exactly one** principal command introducing the name `<Node>`. The statement/proof boundary is the **command boundary**: the statement runs up to the first proof-part command (`proof`/`by`/`apply`/`using`/`unfolding`/`have`/`show`/…), and the proof is everything from there on. There is no `-- BODY` marker. A leading `using`/`unfolding` is part of the proof, so the statement ends before it. Canonical shapes (structured Isar, terminal proof, definition):
  ```isabelle
  theory Tablet_Foo
    imports Tablet_Preamble
  begin

  theorem Foo:
    fixes x :: nat
    shows "P x"
  proof -
    show "P x" by simp
  qed

  end
  ```
  ```isabelle
  theory Tablet_Foo
    imports Tablet_Preamble
  begin

  lemma Foo: "P x"
    using assms
    by blast

  end
  ```
  ```isabelle
  theory Tablet_Foo
    imports Main
  begin

  definition Foo :: "nat \<Rightarrow> nat" where
    "Foo x = x + 1"

  end
  ```
- The principal command is a theory-goal command (`theorem`/`lemma`/`corollary`/`proposition`) for a proof-bearing node, or a theory-declaration command (`definition`/`abbreviation`/`type_synonym`/`datatype`/`fun`/`primrec`/`record`) for a definition node.
- An ordinary node `.tex` file must be exactly one top-level `definition` block, or exactly one theorem-like block immediately followed by exactly one `proof` block.
- Do not put free top-level prose, `\section` commands, or extra theorem environments in an ordinary node `.tex` file.
- If a file wants multiple top-level claims, split it into multiple nodes instead of stacking them into one file. Every helper is a separate `.thy` node imported into the active node's support cone.
- The Isabelle declaration family matches the `.tex` statement family: theory-declaration commands pair with `definition`, theory-goal commands pair with `theorem`/`lemma`/`corollary`/`helper`. `.tex` categories like `corollary` and `helper` are statement-environment categories, not separate Isabelle command keywords.
- Definitions must not wrap or alias theorem statements. A definition that merely names a proposition and is then discharged by a sibling theorem is forbidden. Theorem statements must contain the actual mathematical claim inline, using data definitions and predicates as building blocks.
- After cycle 1, new proof-bearing nodes may not use `SKETCH:` as the first nonblank line of their `.tex` proof block. If you cannot write a complete NL proof for a new theorem, lemma, corollary, or helper node that you believe will pass strict soundness verification, do not create that node.
- The `imports` clause stays within the import allowlist: the session roots `Main`, `HOL` (and its dotted descendants such as `HOL.List`, `HOL.Real`), and `Complex_Main`, plus the sibling-node imports `Tablet_<Dep>`. The warm base already carries the analysis and binomial library through its parent chain, so most nodes need only `imports Tablet_Preamble`.
- `∀` and `\<forall>`, `‹›` and `\<open>\<close>` are the same symbol to the checker, so either spelling is fine and a spelling change is not a statement change.

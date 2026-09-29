# Trellis file specification (Isabelle/HOL backend)

This file defines the intended on-disk shape of the node files in `Tablet/`
for the Isabelle/HOL backend. Workers should read it before editing node
files. Deterministic checks enforce the structural parts of this spec, and
verifier/reviewer prompts assume the same conventions.

The per-node file is an Isabelle theory, so the statement/proof boundary is a
**command boundary**.

## Scope

This spec governs:

- `Tablet/Preamble.thy`
- `Tablet/Preamble.tex`
- `Tablet/header.tex`
- ordinary node files `Tablet/<Node>.thy`
- ordinary node files `Tablet/<Node>.tex`

The generated root import surface for the tablet is kernel-owned, auto-generated
support state, and read-only to workers. This spec does not treat it as a
worker-editable file.

## General pairing rules

- Every ordinary node must have both `Tablet/<Node>.thy` and `Tablet/<Node>.tex`.
- The principal Isabelle command for a node must introduce the name `<Node>` (the
  same name as the file stem).
- Node names match `[A-Za-z][A-Za-z0-9_]*`. No dots, no leading underscore, no
  path separators.
- A node name ending in `__Cert` is reserved: the checker writes a server-owned
  probe theory `Tablet_<Node>__Cert.thy` per node, so a node so named would
  collide with that reserved path.
- The `.thy` and `.tex` files for a node must describe the same mathematical object.
- `Tablet/header.tex` is for shared TeX macros only. Do not put node statements or
  proofs there.

## `Preamble.thy`

`Tablet/Preamble.thy` is the shared import root.

- It collects the shared session imports the rest of the tablet builds on.
- It must not contain project definitions, theorem statements, helper declarations,
  or proofs.

## `Preamble.tex`

`Tablet/Preamble.tex` is a structured list of imported background items. Every
node imports `Preamble.thy`, so nodes use these items directly, without a
`\noderef{Preamble}` citation.

- It may contain zero or more top-level blocks.
- Every top-level block must be exactly one of:
  - `definition`
  - `proposition`
- There must be no free text, section headers, macro definitions, or proof blocks
  outside those top-level blocks.
- Macros belong in `Tablet/header.tex`, not in `Preamble.tex`.

## Ordinary `.tex` node files

For an ordinary non-preamble node, the top-level file shape must be one of the
following exact forms.

### Definition node

```tex
\begin{definition}
...
\end{definition}
```

- Exactly one top-level `definition` block.
- No top-level `proof` block.
- No extra top-level prose before, after, or between blocks.
- Must not wrap or alias theorem statements. A definition that merely names a
  proposition and is then discharged by a sibling theorem is forbidden. Theorem
  statements must contain the actual mathematical claim, using data definitions and
  predicates as building blocks.

### Proof-bearing node

```tex
\begin{theoremlike}
...
\end{theoremlike}
\begin{proof}
...
\end{proof}
```

where `theoremlike` is exactly one of:

- `theorem`
- `corollary`
- `lemma`
- `helper`

Rules:

- Exactly one top-level theorem-like statement block.
- Exactly one top-level `proof` block, immediately after the statement block.
- No extra top-level prose before, after, or between the two blocks.
- If the proof is intentionally incomplete or has been found incomplete, the first
  nonblank line of the `proof` block should be exactly `SKETCH:`. The supervisor
  treats this as an automatic soundness failure until the marker is removed.
- After cycle 1, a worker must not create a new proof-bearing node whose `.tex`
  proof starts with `SKETCH:`. New theorem, lemma, corollary, and helper nodes
  created after the initial scaffold must contain complete NL proofs that the
  worker believes will pass strict soundness verification; if the worker cannot
  write that proof, it should not create the node.

Notes:

- Optional titles like `\begin{lemma}[Name]` are fine.
- Nested LaTeX structure inside a block is allowed when mathematically useful.
- Use `\noderef{ChildNode}` to cite tablet dependencies by node name.
- A citation of a theorem-like node is a citation of the implication, and should
  not be used just to refer to its hypotheses; restate needed conditions locally or
  via a Definition node.

## Ordinary `.thy` node files

For an ordinary non-preamble node, `Tablet/<Node>.thy` is a single Isabelle theory
with exactly one principal command.

### Theory header

The file begins with a theory header:

```isabelle
theory Tablet_<Node>
  imports Tablet_Preamble
begin
  ...
end
```

- The theory name is `Tablet_<Node>` (the `Tablet_` prefix plus the node name).
- The `imports` clause lists the session-root imports (`Main`, `HOL` and its
  dotted descendants such as `HOL.List`/`HOL.Real`, and `Complex_Main`) and the
  sibling-node imports `Tablet_<Dep>`. An import of a sibling node is the
  dependency edge to that node. The warm base already carries the analysis and
  binomial library, so most nodes need only `imports Tablet_Preamble`.
- Every node theory transitively imports `Tablet_Preamble`, either directly or via
  another node's `imports`. This keeps a single shared root for the import DAG.
- The body sits between `begin` and `end`.

### The single principal command

A node theory contains **exactly one** principal command, and it introduces the
name `<Node>`. The principal command is one of two kinds:

- A **theory-goal** command — `theorem`, `lemma`, `corollary`, or `proposition` —
  which states a claim and opens an Isar proof. This is a proof-bearing node and
  pairs to a theorem-like `.tex` block.
- A **theory-declaration** command — `definition`, `abbreviation`, `type_synonym`,
  `datatype`, `fun`, `primrec`, or `record` — which introduces a definitional
  object. This is a definition node and pairs to a `definition` `.tex` block. It is
  never a proof target.

Auxiliary facts and definitions belong in their own registered nodes. A second
principal command alongside the node's own is rejected; factor the auxiliary into a
new node and import it.

### Statement/proof boundary = the command boundary

The statement and the proof are separate outer-syntax commands, so the boundary
is the command boundary:

- For a theory-goal node, the **statement** runs from the goal command
  (`theorem <Node>: …`, including any `fixes`/`assumes`/`shows` and the goal
  proposition) up to the first proof-part command. The **proof** is everything from
  that first proof-part command onward. `using` and `unfolding` are part of the
  proof, so a leading `using`/`unfolding` begins the body — the statement ends
  before it. The checker locates this boundary with the Isabelle outer-syntax
  tokenizer, which skips `(* … *)` comments, `"…"` strings, `‹…›` /
  `\<open>…\<close>` cartouches, and `\<name>` symbols, so a `by`/`proof`/`using`
  word inside a string, comment, or cartouche never moves the boundary.
- For a definition node, the principal command span is the statement and there is no
  Isar proof.

Canonical shapes:

```isabelle
(* theory-goal node: structured Isar proof *)
theory Tablet_<Node>
  imports Tablet_Preamble
begin

theorem <Node>:
  fixes x :: nat
  shows "P x"
proof -
  show "P x" by simp
qed

end
```

```isabelle
(* theory-goal node: terminal proof *)
theory Tablet_<Node>
  imports Tablet_Preamble
begin

lemma <Node>: "P x"
  using assms
  by blast

end
```

```isabelle
(* definition node *)
theory Tablet_<Node>
  imports Main
begin

definition <Node> :: "nat \<Rightarrow> nat" where
  "<Node> x = x + 1"

end
```

### Symbol spellings

Isabelle accepts both the Unicode spelling and the ASCII `\<name>` spelling of a
symbol (`∀` and `\<forall>`, `‹›` and `\<open>\<close>`). The checker treats the two
spellings as identical, so either is fine and a spelling change is not a statement
change.

Deterministic checks enforce the structural parts of this spec:

- the principal command must be named `<Node>` and there must be exactly one
- the `imports` clause must stay within the import allowlist (session roots plus
  `Tablet_<Dep>` siblings)
- `sorry` and the `\<proof>` placeholder mark a node as still open
- a node theory may contain only the statement and its proof; it may not author a
  command-defining, ML, oracle, or axiom command, nor the certificate-inspection
  commands the checker injects into its own probe theory
- `oops` abandons the goal (it produces no theorem), so it is rejected; use `sorry`
  to leave a proof open

## Isabelle source layout under `Tablet/`

All Isabelle source under `Tablet/` must live in either `Tablet/Preamble.thy` or
`Tablet/<Node>.thy` for some registered tablet node. Subdirectories are not allowed,
and a top-level `Tablet/<X>.thy` whose stem `X` is not a registered node is also not
allowed. Shared declarations belong in `Tablet/Preamble.thy`; anything larger should
be factored into a real registered tablet node.

## Practical guidance

- Do not put free exposition, section headers, or manuscript-style narrative at top
  level in a node `.tex` file.
- If a node really contains two separate mathematical claims, split it into two nodes
  instead of placing multiple top-level statement blocks in one `.tex` file.
- If background notation or macros are needed globally, add them to
  `Tablet/header.tex`, not `Preamble.tex`.
- If the file shape you want does not fit this spec, that is usually a sign the DAG
  needs restructuring rather than a sign that the spec should be ignored.

## Protected correspondence (paper-target preservation)

After a human expert approves a paper target at the advance-gate, the kernel
snapshots the set of covering nodes and baselines their correspondence fingerprint.
From that point on, any subsequent worker commit that would cause a covering node's
correspondence to reopen is rejected at commit time (outside `coarse_restructure`
mode). The fingerprint captures the node's own `.tex` statement, the elaborated
meaning of its principal command's signature and the declarations it references
(proof bodies are not in this closure), the `.tex` statements of its
definition-kind descendants, and the preamble's structured `.tex` content. You may
freely modify the proof machinery, but you must preserve the covering node's
declared mathematical content and the definitions it was baselined against.

# Trellis file specification

This file defines the intended on-disk shape of the node files in `Tablet/`.
Workers should read it before editing node files. Deterministic checks enforce
the structural parts of this spec, and verifier/reviewer prompts assume the
same conventions.

## Scope

This spec governs:

- `Tablet/Preamble.lean`
- `Tablet/Preamble.tex`
- `Tablet/header.tex`
- `Tablet/model-support.json`
- ordinary node files `Tablet/<Node>.lean`
- ordinary node files `Tablet/<Node>.tex`

`Tablet.lean` is the generated root import surface for the tablet. It is
kernel-owned, auto-generated support state, and read-only to workers. This spec
does not treat it as a worker-editable file.

## General pairing rules

- Every authored node must have both `Tablet/<Node>.lean` and `Tablet/<Node>.tex`. Registered `extraction_model` nodes use pinned Lean and source provenance; they need no authored TeX counterpart.
- The principal Lean declaration for a node must have the same name as the file stem.
- Node names match `[A-Za-z][A-Za-z0-9_]*`. No dots, no leading underscore, no path separators.
- The `.lean` and `.tex` files for a node must describe the same mathematical object.
- `Tablet/header.tex` is for shared TeX macros only. Do not put node statements or proofs there.

## `Preamble.lean`

`Tablet/Preamble.lean` supplies the extracted model's shared environment:
imports, extraction settings, generated types, and disclosed external
declarations. Its structure is exempt from ordinary node markers; existing
trust records govern its extraction axioms.

In native Modeling, composition materializes this file from the frozen
extraction and `Tablet/model-support.json`. Workers change its support through
that manifest and ordinary paired definition nodes. Extracted types are pinned
nodes that bindings can import directly. Composition arranges their dependencies
without changing the extracted declarations.

Native composition also materializes every registered extracted module in full,
including its imports and elaboration context. Direct edits to these generated
files fail composition checks even when the pinned declaration text is unchanged.
Use paired support definitions and the manifest for missing model meanings;
translation defects require returning to preparation for an audited repair.

## `model-support.json`

The support manifest has this shape:

```json
{"schema":"trellis-model-support/v1","imports":[],"bindings":[]}
```

`imports` lists authored foundational definitions used by the extraction.
Dependencies may be paired nodes, extracted type nodes, or installed libraries,
with an acyclic import graph. The frozen native manifest's
`library_resolution.approved_imports` lists available library modules;
Mathlib imports must name specific submodules.

Each binding copies `name`, `extracted_signature`, `rust_symbol`, and
`raw_declaration_sha256` unchanged from the full `inspect-native` inventory,
then adds `node` and its fully qualified principal Lean `declaration`.
Composition matches the exact external declaration and replaces it with a
typed reference to that definition. Binding nodes are imported automatically.
The Lean code and NL meaning live in the paired node.

Run `python3 .trellis/scripts/pv_modeling.py compose-native <repo>` after
editing support. `--check` checks all composed files without writing them.
Composition retains identity and dependency evidence; ordinary worker
acceptance and semantic verification govern the authored content.

## Preservation sections

In runs with `modeling.preservation`, append preservation sections to every
authored definition's `.tex` and to `Preamble.tex`. Preamble covers setup and
source adaptations; each definition covers its authored meaning. Extracted
declarations need no authored sections. Each section has this shape:

```tex
\begin{preservation}
\begin{claim}
The precise source/model relationship being asserted, with its domain.
\end{claim}
\begin{proof}
The preservation argument.
\end{proof}
\begin{goalproof}{supplied_goal_id}
Why this claim preserves the meaning of this GOAL under the modeling choice.
\end{goalproof}
\end{preservation}
```

Include a separate `goalproof` for every pursued GOAL in each section. An
unaffected goal needs a short noninterference argument. Reuse shared proofs;
do not repeat them in each application. Claim statements identify relevant
observations, assumptions, and source subjects, including the coverage of any
representations or witnesses required by a GOAL. Code locations refer to the
frozen input paths. Missing proofs or proofs beginning with `SKETCH:` remain
soundness blockers.

`\noderef{NodeName}` cites an ordinary paired NL statement. Preservation
citations need no additional Lean import: a preservation theorem may itself
import the definition being justified. The cited support still needs its
ordinary verification and a place in the goal-support DAG. The owner may
cite its own paired definition. These citations do not refer to another
preservation section. Relations already expressible in Lean can use paired
theorem nodes with ordinary proofs.

Use `\evidenceref{reference/model-evidence/file}` in preservation arguments
to cite primary evidence files. Store source excerpts or reproducible probes
with their commands, tool identities, build settings, and raw results there.
Cite each supporting file directly; paths use letters, digits, `_`, `-`, `.`,
and `/`, without symlinks or `.`/`..` segments. Cited contents are checked with
the owner's Soundness evidence and included in the human-review package.

Preservation arguments establish their conclusions independently of the
supplied GOALs; those statements are the subjects being modeled.

Preservation sections have no additional Lean counterpart. Correspondence
compares the ordinary paired statement; soundness checks the preservation
arguments. GOAL statements remain theorem nodes with their own proofs.

## `Preamble.tex`

`Tablet/Preamble.tex` is a structured list of imported background items. Every node imports `Tablet.Preamble`, so nodes use these items directly, without a `\noderef{Preamble}` citation.

- It may contain zero or more top-level blocks.
- Imported background blocks must be one of:
  - `definition`
  - `proposition`
- Preservation sections may follow these background blocks. Other free text, section headers, macro definitions, or proof blocks are forbidden.
- Macros belong in `Tablet/header.tex`, not in `Preamble.tex`.

Informally, each `definition` block should correspond to a definition imported through `Preamble.lean`, and each `proposition` block should correspond to a theorem/proposition imported through `Preamble.lean`.

## Ordinary `.tex` node files

For an ordinary non-preamble node, the paired content has one of the following forms. Definition files append their required preservation sections.

### Definition node

```tex
\begin{definition}
...
\end{definition}
```

- Exactly one top-level `definition` block.
- No top-level `proof` block.
- No extra top-level prose before, after, or between blocks.
- Must not wrap or alias theorem statements. `def Foo : Prop := <statement>` is forbidden whether it stands alone as a definition node or is referenced by `theorem Bar : Foo := sorry` in a sibling node. Theorem signatures must contain the actual mathematical statement inline, using data definitions and predicates as building blocks.

An `instance` node is proof-bearing and uses the proof-bearing `.tex`
shape below, with the statement block phrased as a `lemma`-style claim
that the carrier type has the class structure (for example, "$T$ forms a
group").

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
- If the proof is intentionally incomplete or has been found incomplete, the
  first nonblank line of the `proof` block should be exactly `SKETCH:`. The
  supervisor treats this as an automatic soundness failure until the marker is
  removed.
- In argument development and proof formalization, after cycle 1, a worker must not create a new proof-bearing node whose
  `.tex` proof starts with `SKETCH:`. New theorem, lemma, corollary, and
  helper nodes created after the initial scaffold must contain complete NL
  proofs that the worker believes will pass strict soundness verification; if
  the worker cannot write that proof, it should not create the node. The
  worker checker rejects post-cycle-1 new proof-bearing `SKETCH:` nodes.

Notes:

- Optional titles like `\begin{lemma}[Name]` are fine.
- Nested LaTeX structure inside a block is allowed when mathematically useful.
- Use `\noderef{ChildNode}` to cite tablet dependencies by node name.
- A citation of a theorem-like node is a citation of the implication, and should not be used just to refer to its hypotheses; restate needed conditions locally or via a Definition node.

## Ordinary `.lean` node files

For an ordinary non-preamble node:

- **Every `import` line must come before the `-- [TABLET NODE: NodeName]` marker.** Imports live in the unhashed file preamble; placing one after the tablet marker would pull it into the kernel's protected declaration-signature hash region. The checker rejects any post-marker import.
- The file must contain the tablet marker comment:
  - `-- [TABLET NODE: NodeName]`
- The file must contain a principal top-level declaration named exactly `NodeName`.
- The file should not define additional top-level node declarations unrelated to that node.
- Auxiliary declarations in a node file (helper lemmas under the node's
  namespace, `protected theorem`s, and the like) are **private to that
  node**. Another node's Lean must depend only on registered nodes'
  principal declarations, never on `SomeNode.SomeAux`; the local-closure
  check rejects cross-node auxiliary dependencies. When another node needs
  the auxiliary fact, factor it out: move its statement and proof into a
  new Lean-closed helper node and point the original node's proof at it
  (the auxiliary sits below `-- BODY`, so the move is a proof-body edit).
  (Definition-generated names of a `def`/`structure` node — field
  accessors and the like — are part of its principal declaration and
  remain usable.)
- The legal shape for an in-file auxiliary: declarations inside
  `namespace NodeName ... end` (or `NodeName.`-prefixed). A bare extra
  top-level `theorem`/`lemma` alongside the principal declaration is
  rejected by the file-shape check.
- The `local` forms of `macro`, `macro_rules`, `elab`, `elab_rules`,
  `syntax`, and `binder_predicate` are allowed.
- Non-`local` `macro`, `macro_rules`, `elab`, `elab_rules`, `syntax`, and
  `binder_predicate` commands are not allowed. `declare_syntax_cat` is not
  allowed.
- `run_cmd`, `run_elab`, `run_meta`, `by_elab`, `initialize`, and
  `builtin_initialize` are not allowed.
- `#eval` and `#eval!` are allowed.
- Explicit `partial` declarations are not allowed.
- `notation`, `infix`, `infixl`, `infixr`, `prefix`, and `postfix` are
  allowed.
- **The file must contain exactly one line whose trimmed content is `-- BODY`.** This comment line is the statement/proof boundary marker. It sits between the principal declaration's body delimiter (`:= by`, `:= <term>`, `:=` alone, or `where` for a data declaration) and the start of the body. Lean treats `--` as a line comment, so the marker is invisible to the parser. The kernel's text-based splitter relies on this marker — no Lean-parser fallback exists.
- **The line immediately above the `-- BODY` marker (with trailing whitespace ignored) must end with `:=`, `by`, or `where`.** This is how the checker confirms the marker sits where the FILESPEC says it does (right after the principal declaration's body delimiter, not inside the type or inside the proof). For a multi-line `:= by` split where `by` sits on its own line, that `by` line is what immediately precedes the marker. For a `structure`/`inductive`/`class` data declaration, that delimiter is `where` and the fields/constructors sit below the marker.
- The Lean declaration family must match the `.tex` statement family:
  - definition-like Lean declarations (`def`, `abbrev`, `noncomputable def`, and the data declarations `structure`, `inductive`, `class`) go with `definition`
  - proof-bearing Lean declarations should be written with `theorem`, `lemma`, or `instance`
  - theorem-like `.tex` environments (`theorem`, `lemma`, `corollary`, `helper`) are statement-environment categories, not separate Lean declaration keywords
- `structure`, `inductive`, and `class` are definition nodes (siblings of `def`/`abbrev`); they never enter soundness or proof-formalization as proof targets. The declaration head (up through `where`) is the hashed, correspondence-checked signature; the fields/constructors below `-- BODY` are the body. An `inductive` must use the `where` form (`inductive Foo : Type where`), so its body delimiter is uniformly `where`.
- `instance` is a proof-bearing node: it discharges the class's law fields, which can carry `sorry`s in theorem-stating and close in proof-formalization, so it routes through soundness and pairs with a `lemma`-style `.tex` claim. An instance must be named (`instance NodeName : ClassName … := …`) so the kernel can identify the node's principal declaration; its body delimiter may be `:=`, `:= by`, or structure-instance `where`.

Canonical shapes:

```lean
-- tactic mode
theorem NodeName : Statement := by
-- BODY
  tactic1
  tactic2
```

```lean
-- term mode
def NodeName : T :=
-- BODY
  some_term
```

```lean
-- When the current task permits preamble-option edits, place a declaration-wide
-- option above the node marker. Closure-only tasks preserve existing options.
import Tablet.Dep
set_option maxHeartbeats 800000
-- [TABLET NODE: NodeName]
theorem NodeName : Statement := by
-- BODY
  tactic1
```

```lean
-- structure / class (where-form data declaration; fields are the body)
structure NodeName extends Base where
-- BODY
  field1 : T1
  field2 : T2
```

```lean
-- inductive (where-form mandatory; constructors are the body)
inductive NodeName : Type where
-- BODY
  | c1 : NodeName
  | c2 : T → NodeName
```

```lean
-- instance (proof-bearing; pairs to a lemma-style .tex claim)
instance NodeName : ClassName Carrier where
-- BODY
  law1 := by sorry
  law2 := by sorry
```

### Heartbeat options (`set_option maxHeartbeats`)

Apply performance advice within the current task's edit scope. For a
closure-only task, preserve the existing preamble options and theorem
statement; develop intermediate facts as local `have`/`let` steps inside
the proof body. Apply Lean's diagnostic suggestions within those same
boundaries, using scratch files for declaration-wide experiments.

When the task permits new helper nodes, splitting a large proof into closed
helpers lets each helper build and cache independently. When the task permits
an option change, placement depends on what the option must cover:

- **Declaration-wide budget, with preamble-option edit permission:** a standalone `set_option maxHeartbeats N`
  in the file preamble, **above the `-- [TABLET NODE: ...]` marker**
  (alongside the imports). This keeps the option outside the hashed
  declaration signature; task-specific preamble guards still apply.
- **Spot increase inside a proof:** tactic-position
  `set_option maxHeartbeats N in <tactic>` below `-- BODY`.

The command-wrapper form (`set_option … in theorem`) remains valid, but it places the
option inside the hashed signature region: on an approved node, adding or
retuning it is a statement edit and is rejected by the drift guards. Use
the above-marker form when the current task permits preamble-option edits.

The marker line may be at any indentation (`--` line comments are indentation-inert in Lean). Do not put the literal text `-- BODY` (whitespace-trimmed, on its own line) inside any `/-…-/` block comment in a tablet file — the splitter does not distinguish the two.

Existing deterministic checks also enforce related Lean hygiene:

- unauthorized imports are rejected
- node-marker mismatches are rejected
- `sorry` inside definitions is rejected
- declaration-name mismatches are rejected
- **missing or duplicate `-- BODY` marker is rejected**
- **missing or duplicate `-- [TABLET NODE: NodeName]` marker is rejected**
- **an `import` line at or after the `-- [TABLET NODE: ...]` marker is rejected**
- **the line immediately preceding `-- BODY` not ending with `:=`, `by`, or `where` is rejected**
- **an anonymous `instance` (no name) is rejected**
- **a bare-pipe `inductive` (not using the `where` form) is rejected**

### Preamble import convention (auto-applied)

Ordinary nodes import `Tablet.Preamble` directly or through their dependencies.
Preamble's own prerequisites import the libraries and definitions they need.
At worker acceptance, the supervisor adds a Preamble import to nodes with no
Tablet imports, except Preamble and its transitive prerequisites. Existing
import cycles remain errors.

## Lean source layout under `Tablet/`

All Lean source under `Tablet/` must live in either `Tablet/Preamble.lean`
or `Tablet/<NodeName>.lean` for some registered tablet node. Subdirectories
are not allowed (no `Tablet/Support/X.lean`), and a top-level
`Tablet/<X>.lean` whose stem `X` is not a registered node is also not
allowed. Authored shared declarations belong in registered paired nodes. This is
enforced at worker-acceptance time: any offending `.lean` file under
`Tablet/` produces a per-path rejection naming the file.

## Practical guidance

- Do not put free exposition, section headers, or manuscript-style narrative at top level in a node `.tex` file.
- If a node really contains two separate mathematical claims, split it into two nodes instead of placing multiple top-level statement blocks in one `.tex` file.
- If background notation or macros are needed globally, add them to `Tablet/header.tex`, not `Preamble.tex`.
- If the file shape you want does not fit this spec, that is usually a sign the DAG needs restructuring rather than a sign that the spec should be ignored.

## Protected correspondence (approved-target preservation)

After a human expert approves a verification target at the advance-gate, the kernel
snapshots the set of **covering nodes** (the tablet nodes whose statements
the target claims) and baselines their "correspondence fingerprint." From
that point on, any subsequent worker commit that would cause a covering
node's correspondence to "reopen" is rejected at commit time (outside
`coarse_restructure` mode).

### What the correspondence fingerprint captures

For a given node, its correspondence fingerprint is a hash of:

1. The node's own `.tex` statement block.
2. The Lean-semantic closure of the node's Lean declaration — the
   transitive serialization of every `const` it references (theorem types,
   definition values, inductive/constructor shapes, axiom types, ...).
   **Proof bodies are NOT in this closure.** Changing a proof body of a
   dependency does not change this hash; changing the declared type of a
   referenced theorem, or the value of a referenced definition, DOES.
3. The `.tex` statements of every `definition`-kind descendant in the
   node's Lean-import closure (via `import Tablet.X` lines). Only
   descendants that were already present at the moment the current
   baseline was captured count. A worker adding a new definition helper
   post-baseline does not by itself trigger a reopen; the new helper
   enters the baseline at the next correspondence verification.
4. The preamble's structured `.tex` content.

### What "reopen" means

A covering node's correspondence is "reopened" by a commit if the
prospective post-commit fingerprint differs from the approval baseline
along any of these axes:

- Own `.tex` statement changed.
- Lean-semantic closure changed (something referenced by the node's Lean
  declaration no longer has the same meaning).
- A baselined definition-descendant's `.tex` statement changed, or the
  descendant is no longer reachable from the node's Lean-import closure.
- The preamble's `.tex` structured content changed.

If any of those happen on a covering node, the commit is rejected and the
agent receives a prose error enumerating which axis differed.

### What is allowed (not a reopen)

- Proof-body changes — your own or dependencies'.
- Adding new helper nodes (proof-kind or definition-kind) below the
  covering node.
- Renaming local identifiers inside proofs.
- Any change on a non-covering node, as long as it does not cascade into
  a meaning-change of the covering node's Lean-semantic closure.

In general: you may freely modify the proof machinery, but you must
preserve the covering node's declared mathematical content and the
definitions it was baselined against.

### The `coarse_restructure` escape hatch

Genuine expert-approved changes to a covering node's statement or its
supporting definition package require `coarse_restructure` mode. Requests
for that mode must come from a reviewer — workers cannot enter
`coarse_restructure` on their own. The mode bypasses the reopen guard
entirely, on the assumption that the human-in-the-loop review step
re-approves the modified target.

### What workers see in each request

The worker request carries:

- `approved_target_nodes`: the covering-node set snapshotted at the last
  advance-gate approval.
- `approved_corr_fingerprints`: the JSON-encoded
  `CorrespondenceFingerprint` baseline for each covering node.

These are the exact nodes / fingerprints the commit-time guard will
compare against. Workers can precompute whether their planned delta
would trip the guard by reasoning about whether it affects any of the
four axes above on any `approved_target_nodes` entry.

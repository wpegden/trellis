import Lean

open Lean

/-!
# Tablet local-closure axiom collector (Patch A — observation only)

Given a Tablet node name (e.g. `"ProjectionSubsetCountBound"`), import its
module, locate the named declaration, and emit a JSON payload describing the
*local closure* of that declaration in the elaborated environment.

This is the Lean half of `LOCAL_CLOSURE_IMPL_PLAN.md` Patch A. It produces
observational data only — gating, persistence, and policy live in the
Rust kernel and Python checker server. See plan §4 (traversal) and §5
(Patch A scope).

## What the closure is

The closure is the set of constants reachable from the active declaration
under a *mode-aware* traversal:

* `ProofMayAssumeTheorems` mode is used **only** when visiting an active
  theorem's `value`. In this mode, when we hit another Tablet **theorem**
  via `thmInfo`, we record it as a *boundary helper* and walk only its
  `type` (the statement) — not its proof. This mirrors the meta-level
  "assume the helper as stated" semantics of plan §2.2: we ride on top of
  Lean's honest elaboration but stop at the boundary we define.
* `Strict` mode is used for everything else — types, statements,
  definition values, inductive constructors, recursors. In `Strict`,
  hitting a Tablet theorem walks both `type` and `value`; hitting a
  Tablet definition walks both `type` and `value`; etc. No boundary cut.

Non-Tablet constants on the closure boundary are handed to
`Lean.collectAxioms` (the public stdlib API at
`Lean/Util/CollectAxioms.lean:149`). Their transitive axioms are merged
into `kernel_axioms`. We rely on the per-module axiom-export cache that
`exportedAxiomsExt` populates at olean-emit time, so non-Tablet sub-trees
do not require body walks.

## Memoization shape (per plan §4.4)

The memo is keyed by `(provider module, constant, mode)`, never by constant
alone. The same
constant visited under `Strict` produces different side effects than
under `ProofMayAssumeTheorems` (a Tablet theorem is recorded as a
boundary in the latter, as a strict-theorem-dep in the former). A
single-bit `seen` set is therefore **forbidden** here: it would silently
lose one mode's recording. On a memo hit, the cached *side-effect record*
is replayed into the current accumulator.

## Hash policy

`statement_hash` / `value_hash` / `semantic_hash` are content-hashes for
record invalidation, not security primitives. We mirror the precedent
walker from `lean_semantic_fingerprint.lean`:

* `mdata` is stripped (source-position drift must not change the hash);
* binder *names* in `lam` / `forallE` / `letE` are not mixed in (alpha
  equivalence under re-elaboration must not change the hash);
* binder *info* (default / implicit / strictImplicit / instImplicit)
  *is* included.

Hashes are emitted as 16-character lowercase hex of a `UInt64`.

## Output

JSON object on stdout per plan §5.1, plus the §4.6.1 cross-check sub-object:

```
{
  "node": "...",
  "status": "ok" | "elaboration_error" | "missing_declaration" | "policy_rejection" | "internal_error",
  "root_kind": "theorem" | "lemma" | "def" | "abbrev" | "axiom" | "opaque" | "other",
  "kernel_axioms": ["..."],
  "boundary_theorems": [{"name": "...", "statement_hash": "..."}],
  "strict_theorem_deps": [{"name": "...", "value_hash": "..."}],
  "strict_definition_deps": [{"name": "...", "semantic_hash": "..."}],
  "errors": ["..."],
  "axiomization_check": {
    "kernel_axioms": ["..."],
    "boundary_theorems": ["..."],
    "agreed": true,
    "skipped": false,
    "primary_only_axioms": [],
    "axcheck_only_axioms": [],
    "primary_only_boundaries": [],
    "axcheck_only_boundaries": []
  }
}
```

Disable the secondary collector via env var
`TRELLIS_LOCAL_CLOSURE_AXCHECK_DISABLE=1` or CLI flag `--no-axcheck`
(after the node name). When disabled the sub-object reports
`skipped: true` and empty diff sets; the Rust wrapper accepts that as
a trivial pass.

Lean does not classify `theorem` vs `lemma` distinctly — both elaborate
to `thmInfo`. We emit `"theorem"` for both. Likewise the script does not
distinguish `def` from `abbrev` at the *root* (the declaration kind on
disk is what the Rust caller already validates against the `.tex`); both
emit `"def"` here. The Rust wrapper performs NodeId normalization and
top-level kind interpretation.

## Project axiom policy

This script does NOT consult any approved-axioms list. The
`axiomInfo`/`opaqueInfo` arm just emits the constant name into
`kernel_axioms`; the Rust wrapper applies per-node `load_approved_axioms`
filtering. Trust boundary lives in the kernel, not here.
-/

/-! ## Mode and helpers -/

inductive VisitMode where
  | strict
  | proofMayAssumeTheorems
  deriving DecidableEq, BEq, Hashable, Inhabited

private def nameFromString (s : String) : Name :=
  s.splitOn "." |>.foldl Name.str Name.anonymous

private def moduleForNode (nodeName : String) : Name :=
  nameFromString s!"Tablet.{nodeName}"

/-! ## Worker authoring-policy scan

This parse-only scan is invoked exclusively through `--scan-only`.  The full
certificate probe never calls it.  It rejects source constructs that can
export or arbitrarily mutate persistent environment-extension state not
represented by the declaration manifest.  Locally scoped macro/syntax/
elaborator definitions and `#eval`/`#eval!` do not export such state and are
allowed. -/

/-- The command syntax kinds that DEFINE a command macro, term/command
syntax, or elaborator — i.e. the mechanisms that can synthesize a top-level
declaration whose name is absent from source. Verified against
v4.30.0-rc1 by parsing each command and reading `Syntax.getKind` (the
parser `def`s in `Lean/Parser/Syntax.lean` carry these node kinds via
`leading_parser`'s `decl_name%`). Still valid under the current pin
v4.33.0 (2026-08-27): the `` `` ``-quoted names below resolve at
COMPILE time, so a renamed or removed kind would fail this file to
elaborate, and the `--scan-only` cases in
`kernel/tests/local_closure_smoke.rs` run this gate green against a
v4.33.0-built fixture. `notation`/`mixfix` are intentionally
absent (term-level only; see the section comment). -/
private def policyCommandKinds : Array Name := #[
  ``Lean.Parser.Command.macro,
  ``Lean.Parser.Command.macro_rules,
  ``Lean.Parser.Command.elab,
  ``Lean.Parser.Command.elab_rules,
  ``Lean.Parser.Command.syntax,
  ``Lean.Parser.Command.syntaxAbbrev,
  ``Lean.Parser.Command.syntaxCat,        -- `declare_syntax_cat`
  ``Lean.Parser.Command.binderPredicate,
  `Lean.runCmd,
  `Lean.runElab,
  `Lean.runMeta,
  `Lean.byElab,                           -- term elaborator; recursively reached
  ``Lean.Parser.Command.initialize        -- `initialize` AND `builtin_initialize`
]

/-- A human-facing keyword for a banned command kind, for the diagnostic. -/
private def policyCommandKeyword (k : Name) : String :=
  if k == ``Lean.Parser.Command.macro then "macro"
  else if k == ``Lean.Parser.Command.macro_rules then "macro_rules"
  else if k == ``Lean.Parser.Command.elab then "elab"
  else if k == ``Lean.Parser.Command.elab_rules then "elab_rules"
  else if k == ``Lean.Parser.Command.syntax then "syntax"
  else if k == ``Lean.Parser.Command.syntaxAbbrev then "syntax (abbrev)"
  else if k == ``Lean.Parser.Command.syntaxCat then "declare_syntax_cat"
  else if k == ``Lean.Parser.Command.binderPredicate then "binder_predicate"
  else if k == `Lean.runCmd then "run_cmd"
  else if k == `Lean.runElab then "run_elab"
  else if k == `Lean.runMeta then "run_meta"
  else if k == `Lean.byElab then "by_elab"
  else if k == ``Lean.Parser.Command.initialize then "initialize / builtin_initialize"
  else k.toString

/-- Macro/syntax/elaborator definitions carrying Lean's `local` attribute do
not export their environment-extension entries and are allowed by policy. -/
private partial def syntaxContainsKind (stx : Syntax) (kind : Name) : Bool :=
  stx.getKind == kind || stx.getArgs.any (syntaxContainsKind · kind)

private def hasLocalAttrKind (stx : Syntax) : Bool :=
  stx.getArgs.any fun arg =>
    arg.getKind == ``Lean.Parser.Term.attrKind
      && syntaxContainsKind arg ``Lean.Parser.Term.local

private def isLocallyScopedPolicyDefinition (stx : Syntax) : Bool :=
  let k := stx.getKind
  (k == ``Lean.Parser.Command.macro
    || k == ``Lean.Parser.Command.macro_rules
    || k == ``Lean.Parser.Command.elab
    || k == ``Lean.Parser.Command.elab_rules
    || k == ``Lean.Parser.Command.syntax
    || k == ``Lean.Parser.Command.binderPredicate)
    && hasLocalAttrKind stx

/-- Recursively collect the keyword of every banned command kind that
appears anywhere in a parsed command's syntax tree. The walk is RECURSIVE,
not top-level-only: a banned command may be wrapped by a `… in` combinator
(`set_option x in macro …` has top kind `Lean.Parser.Command.in` with the
`macro` nested inside) or by leading attributes, so a top-level-kind check
would miss those forms. The command-kind branch is false-positive-free: an
ordinary declaration's type/body uses `Term`-category quotation kinds, never
these `Command.*` kinds. The separate `by_elab` identifier fallback covers a
reserved builtin term keyword that this parse-only environment may recover
before constructing its normal `Lean.byElab` node. -/
private partial def collectPolicyCommandKeywords
    (stx : Syntax) (acc : Array String) : Array String :=
  let acc :=
    if policyCommandKinds.contains stx.getKind && !isLocallyScopedPolicyDefinition stx then
      acc.push (policyCommandKeyword stx.getKind)
    else if stx.isIdent && stx.getId == `by_elab then
      -- `by_elab` has the real term kind `Lean.byElab` in Lean's normal
      -- frontend, but `Parser.parseCommand` against a freshly imported core
      -- environment error-recovers it as an `ident` before the do-sequence.
      -- It is nevertheless a reserved builtin keyword in a compiling source
      -- file, so this recursive token fallback is exact and keeps the
      -- authoring ban effective on the parse-only path.
      acc.push "by_elab"
    else acc
  stx.getArgs.foldl (fun a c => collectPolicyCommandKeywords c a) acc

private partial def collectAuthoredDeclarationNames
    (stx : Syntax) (acc : Array String) : Array String :=
  let acc := if stx.getKind == ``Lean.Parser.Command.declId then
    let id : Name := if stx.isIdent then stx.getId else stx[0].getId
    acc.push (toString id)
  else acc
  stx.getArgs.foldl (fun a c => collectAuthoredDeclarationNames c a) acc

/-- Whether a parsed source command carries Lean's explicit `partial`
declaration modifier. This is source-command provenance: compiler-generated
implementation declarations never occur in this syntax tree. -/
private partial def containsPartialModifier (stx : Syntax) : Bool :=
  stx.getKind == ``Lean.Parser.Command.partial
    || stx.getArgs.any containsPartialModifier

/-- Parse-only authoring-policy result. The parser uses Lean's core grammar
without importing the node module. Build and certificate checks separately
require the source to elaborate and the artifact to replay. -/
private structure OwnerPolicyScanResult where
  /-- Authored declaration-command names carrying an explicit `partial`
  modifier. Safe recursive compiler scaffolding is absent from source. -/
  partialNames    : Array String := #[]
  /-- Keywords of constructs forbidden by the authoring policy. -/
  policyCommands  : Array String := #[]
  /-- `some path` when the node source could not be read (fail closed). -/
  unreadable      : Option String := none
  deriving Inhabited

private def scanOwnerPolicySource (source : String) (fileName : String)
    : IO OwnerPolicyScanResult := do
  let inputCtx := Parser.mkInputContext source fileName
  -- Load Lean's core parser grammar, but no Tablet or other project module.
  let env ← importModules #[{ module := `Lean }] {}
  let (_, parserState, messages) ← Parser.parseHeader inputCtx
  let pmctx : Parser.ParserModuleContext := { env := env, options := {} }
  let mut state := parserState
  let mut msgs := messages
  let mut partials : Array String := #[]
  let mut policyCommands : Array String := #[]
  repeat
    let (cmd, state', msgs') := Parser.parseCommand inputCtx pmctx state msgs
    state := state'
    msgs := msgs'
    let cmdAuthored := collectAuthoredDeclarationNames cmd #[]
    if containsPartialModifier cmd then
      partials := partials ++ cmdAuthored
    policyCommands := collectPolicyCommandKeywords cmd policyCommands
    if Parser.isTerminalCommand cmd then break
  -- Deduplicate + sort for a deterministic diagnostic.
  let dedup (xs : Array String) : Array String :=
    (xs.foldl (init := (#[] : Array String)) fun acc s =>
      if acc.contains s then acc else acc.push s).qsort (· < ·)
  return { partialNames := dedup partials, policyCommands := dedup policyCommands }

/-- Read a node's own source file (`Tablet/<Node>.lean`, relative to the
probe's cwd = repo root) and return its authoring-policy parse result. The
path mirrors `moduleForNode`'s module→path convention. -/
private def scanOwnerPolicyFile (nodeName : String) : IO OwnerPolicyScanResult := do
  -- `Tablet.<A>.<B>` ⇒ `Tablet/<A>/<B>.lean`. nameFromString round-trips
  -- the dotted node name; build the path from its components.
  let modName := moduleForNode nodeName
  let segs := modName.components.map (·.toString)
  let relPath : System.FilePath :=
    (segs.foldl (init := (none : Option System.FilePath)) fun acc s =>
      match acc with
      | none => some (System.FilePath.mk s)
      | some p => some (p / s)).getD (System.FilePath.mk nodeName)
  let path := relPath.addExtension "lean"
  if (← path.pathExists) then
    let source ← IO.FS.readFile path
    scanOwnerPolicySource source path.toString
  else
    return { unreadable := some path.toString }

/-- Salt per `BinderInfo` so binder-info changes (e.g. `(x : T)` ↔ `{x : T}`)
register in the hash even though the binder *name* does not. -/
private def binderInfoTag : BinderInfo → UInt64
  | .default        => 0x10
  | .implicit       => 0x11
  | .strictImplicit => 0x12
  | .instImplicit   => 0x13

private def literalHash : Literal → UInt64
  | .natVal n => mixHash 0xaa (Hashable.hash n)
  | .strVal s => mixHash 0xab (Hashable.hash s)

/-- Pointer-memoized walk over an `Expr`, returning a `UInt64` digest that is

  * mdata-insensitive (`mdata _ body` → recurse into `body`),
  * binder-name-insensitive (`λ x => x` and `λ y => y` hash equally),
  * sensitive to constructor tag, structure, universes, binder info,
    const names, and literals.

Memoizes on `Expr` pointer addresses (`ptrAddrUnsafe`) so cost is
`O(unique-pointers)` regardless of the structural fanout of the
elaborated term. Marked `unsafe`; called only from
`hashExprs` below, which is wrapped via `@[implemented_by]` so the
rest of the script stays in safe code. -/
private unsafe def walkExprImpl
    (e : Expr) (cache : Std.HashMap USize UInt64)
    : UInt64 × Std.HashMap USize UInt64 :=
  let addr := ptrAddrUnsafe e
  match cache[addr]? with
  | some h => (h, cache)
  | none =>
    let (h, cache') : UInt64 × Std.HashMap USize UInt64 :=
      match e with
      | .bvar idx                =>
          (mixHash 0x01 idx.toUInt64, cache)
      | .fvar fvarId             =>
          (mixHash 0x02 (Hashable.hash fvarId.name), cache)
      | .mvar _                  =>
          (0x03, cache)
      | .sort lvl                =>
          (mixHash 0x04 lvl.hash, cache)
      | .const name lvls         =>
          let lvlH : UInt64 :=
            lvls.foldl (fun acc l => mixHash acc l.hash) 0
          (mixHash 0x05 (mixHash (Hashable.hash name) lvlH), cache)
      | .app f a                 =>
          let (hF, c1) := walkExprImpl f cache
          let (hA, c2) := walkExprImpl a c1
          (mixHash 0x06 (mixHash hF hA), c2)
      | .lam _ ty body bi        =>
          let (hTy,   c1) := walkExprImpl ty   cache
          let (hBody, c2) := walkExprImpl body c1
          (mixHash 0x07 (mixHash hTy (mixHash hBody (binderInfoTag bi))), c2)
      | .forallE _ ty body bi    =>
          let (hTy,   c1) := walkExprImpl ty   cache
          let (hBody, c2) := walkExprImpl body c1
          (mixHash 0x08 (mixHash hTy (mixHash hBody (binderInfoTag bi))), c2)
      | .letE _ ty val body nondep =>
          let (hTy,   c1) := walkExprImpl ty   cache
          let (hVal,  c2) := walkExprImpl val  c1
          let (hBody, c3) := walkExprImpl body c2
          let nondepBit : UInt64 := if nondep then 1 else 0
          (mixHash 0x09 (mixHash (mixHash hTy hVal) (mixHash hBody nondepBit)), c3)
      | .lit lit                 =>
          (mixHash 0x0a (literalHash lit), cache)
      | .mdata _ body            =>
          walkExprImpl body cache
      | .proj typeName idx struct =>
          let (hStruct, c1) := walkExprImpl struct cache
          (mixHash 0x0b
            (mixHash (Hashable.hash typeName) (mixHash idx.toUInt64 hStruct)),
           c1)
    (h, cache'.insert addr h)

private unsafe def hashExprsImpl (exprs : List Expr) : UInt64 :=
  let init : UInt64 × Std.HashMap USize UInt64 :=
    (0xfeed_face_cafe_beef, ∅)
  let (h, _) :=
    exprs.foldl (fun (acc, cache) e =>
        let (hE, cache') := walkExprImpl e cache
        (mixHash acc hE, cache'))
      init
  h

/-- Safe-callable wrapper. The body is a typechecking placeholder; runtime
goes through `hashExprsImpl` via `@[implemented_by]`. -/
@[implemented_by hashExprsImpl]
private def hashExprs (_exprs : List Expr) : UInt64 := 0

/-- Format a `UInt64` as 16-character lowercase hex (zero-padded). -/
private def uint64HashStr (h : UInt64) : String :=
  let n := h.toNat
  let hex := Nat.toDigits 16 n
  let padded := List.replicate (16 - hex.length) '0' ++ hex
  String.ofList padded

private def statementHash (typeExpr : Expr) : String :=
  uint64HashStr (hashExprs [typeExpr])

private def valueHash (typeExpr valueExpr : Expr) : String :=
  uint64HashStr (hashExprs [typeExpr, valueExpr])

/-- Same hashing strategy as `valueHash` but named separately to track
the field semantically (defs/abbrevs/inductives go here). -/
private def semanticHash (typeExpr : Expr) (extras : List Expr := []) : String :=
  uint64HashStr (hashExprs (typeExpr :: extras))

/-! ## Module classification -/

/- Historical classifier note (no longer executable): Lean's constructor injection theorems `Ctor.inj` / `Ctor.injEq`.

Lean generates these under the constructor namespace, for example
`InputDenotation.finite.injEq` and `InputDenotation.finite.inj`. They are not
authored Tablet nodes and have no kernel lifecycle hook, so they must be
transparent-walked. This is a provenance check, not a suffix-only filter: the
immediate parent must be an environment-recognized constructor. -/

/- Historical classifier note (no longer executable): Lean's constructor `sizeOf` specification theorems
`Ctor.sizeOf_spec`.

Lean's `SizeOf` machinery generates one simp lemma per constructor under the
constructor's namespace, for example `InputDenotation.finite.sizeOf_spec`.
Like `inj` / `injEq` these are not authored Tablet nodes and have no kernel
lifecycle hook, so they must be transparent-walked. This is a provenance
check, not a suffix-only filter: the immediate parent must be an
environment-recognized constructor AND the theorem must be declared in the
same module as that constructor (generation is eager, in the inductive's own
module — a cross-module authored `Ctor.sizeOf_spec` forgery stays a
recorded, fail-closed dep key). -/

/- Historical classifier note (no longer executable): final-name-component suffixes of the compiler-generated
recursor/eliminator family attached to an inductive's namespace. Grounded in
the toolchain source (`Lean.AuxRecursor` suffix constants;
`Lean.Elab.MutualInductive.mkAuxConstructions`; `Lean.Meta.IndPredBelow`).
`IndPredBelow` generates `below` (an `inductDecl`) and `brecOn` (a `thmDecl`)
for recursive inductive PREDICATES without ever calling `markAuxRecursor`,
so `Lean.isAuxRecursor` is false for them in EVERY environment. -/

/- Historical classifier note (no longer executable): recursor-family suffixes and numbered variants
`<suffix>_<digits>` (the `Name.appendIndexAfter` shape Lean uses for the
nested/mutual-inductive copies, e.g. `below_1`, `brecOn_2`). Digits-only
after the underscore: `below_lemma` does NOT match (stays fail-closed). -/

/- Historical classifier note (no longer executable): inductive generated
recursor-family members: the final component is a recursor-family suffix AND
the immediate parent name resolves in the env to an INDUCTIVE AND the member
is declared in the SAME module as that inductive (generation is eager, in
the inductive's own module). Provenance-based like `isCtorInjectionTheorem`:
a same-suffix name under a non-inductive parent, or declared in a different
module than its parent inductive, is NOT recognized (fail-closed: it stays a
recorded dep key). -/

/- Removed interim classifier rationale. The module manifest supersedes this
definition and its former classifier clause when the
declaration-provenance manifest lands; it is name-fitted by design and the
manifest supersedes it wholesale.

Recognizes the ONE compiler-generated theorem that blocked a real run:
`Owner.ofNat_ctorIdx`, emitted by `Lean/Elab/Deriving/DecEq.lean` for an
enum with `deriving DecidableEq`. It is the only member of the `ctorIdx`
family that is theorem-shaped; the siblings (`ctorIdx`, `toCtorIdx`,
`ofNat`, `ctorElimType`) are definitions, which `depDefKeyName` already
attributes to the owner node, and `ctorElim` carries the aux-recursor tag.

PROVENANCE, not name shape. The conjuncts that make this sound:

  * the owner is enum-shaped and its generated siblings exist — so the
    `deriving DecidableEq` machinery demonstrably ran on it;
  * theorem and owner share a module — a cross-module forgery fails here;
  * the OWNER carries a `declRangeExt` entry and the THEOREM does not.

That last one is the discriminator. Lean records source ranges for
declarations elaborated from a declaration command; `mkEnumOfNatThm` adds
its theorem through `addDecl` and no range is recorded. Measured on
v4.33.0: a genuine `Gen.ofNat_ctorIdx` has no range, while a hand-authored
`Forge.ofNat_ctorIdx` — enum owner, same module, exact name — has one. That
is precisely the own-module forgery the classifier comment above flags as
the case module co-location alone cannot catch.

KNOWN INCOMPLETE, deliberately: `Owner.brecOn.eq` (recursive inductives),
`Owner.ext`/`ext_iff` (`@[ext]`), and Prop-valued derived instances such as
`instNonemptyOwner` remain fail-closed. They are not name-matched here
because nothing is blocked on them and each would be more throwaway code.
Range absence is NOT a general rule — `@[ext]` theorems and Prop-valued
derived instances DO carry ranges — which is why this stays narrow and why
the general fix is a provenance manifest rather than a wider range test. -/

/- Removed generated-artifact classifier rationale. Tablet declarations are
rather than authored Tablet nodes.

The local-closure record maps Lean declarations onto kernel `NodeId`s. A
generated artifact must not become a record key, because it has no kernel
lifecycle hook. It must still be traversed transparently so any real Tablet
dependencies inside its type/value are recorded under their own NodeIds.

`Name.isInternalDetail` covers the older `_proof`, `_sunfold`, `eq_1`,
`match_1`, etc. families. `Lean.isReservedName` covers Lean's reserved
realization families such as `congr_simp`, `hcongr_N`, `eq_def`, and
`eq_unfold`; user code cannot declare those names directly. Do not add broad
suffix filters here unless Lean also prevents users from authoring them.

The remaining clauses recognize the auto-generated members of a
`structure` / `class` / `inductive` node, which `FILESPEC.md` declares to be
part of the node's principal declaration (and therefore usable across nodes,
not forbidden private auxiliaries). Their names are arbitrary — a field can
be named anything, a constructor anything — so a name-prefix heuristic is
structurally wrong; we ask the environment, which holds the authoritative
classification:

* `Environment.getProjectionFnInfo?` is populated for every `structure` /
  `class` field projection (class methods included via
  `ProjectionFunctionInfo.fromClass`).
* `Environment.getAuxParentProjectionInfo?` is populated for the
  `extends`-diamond non-subobject parent coercion `Child.toParent`.
* `Environment.isConstructor` recognizes inductive constructors regardless of
  their (arbitrary) name, e.g. an enum-style `MyColor.red`.
* `Ctor.inj` / `Ctor.injEq` are recognized by checking that their immediate
  parent is an environment-recognized constructor; `Ctor.sizeOf_spec` by the
  same parent check plus module co-location (`isCtorSizeOfSpecTheorem`).
* `Lean.isAuxRecursor` recognizes only declarations tagged via
  `markAuxRecursor` at generation time. That covers the eliminator family
  (`.casesOn` / `.recOn` / `.brecOn` / `.below`) built by
  `mkAuxConstructions` for ordinary inductives — but NOT the `below` /
  `brecOn` that `Lean.Meta.IndPredBelow` builds for recursive Prop-valued
  inductive PREDICATES: those are added without any `markAuxRecursor`
  tagging, so the tag does not exist in ANY environment (loading the
  environment extensions does not help — the tags were simply never
  written). `isRecursorFamilyRealization` therefore recognizes the family
  explicitly by name shape + provenance: recursor-family final component
  (`recursorFamilySuffixes`, incl. numbered `_N` variants), parent resolves
  to an inductive, and the member is declared in the SAME module as that
  inductive.
* `Lean.isNoConfusion` recognizes the `.noConfusion` *lemma*.

`Owner.noConfusionType` / `Owner.toCtorIdx` / `Owner.ctorIdx` are
deliberately NOT classified as generated. No authoritative predicate
reachable from a plain `import Lean` distinguishes a genuinely-generated one
from a hand-written auxiliary of the same name: `isNoConfusion` tags only the
`noConfusion` lemma, not the sibling type; `toCtorIdx` is only a deprecated
alias with no predicate; and `isCtorIdxCore?` is the same unsound
suffix-plus-`isInductiveCore?` heuristic, with no check that the decl is
actually compiler-generated. Lean does not always generate these members
(e.g. no `toCtorIdx` for a structure owner), so a name-collision guard is
insufficient: a hand-authored `theorem Owner.toCtorIdx` compiles unblocked,
and a suffix classifier would hide it. They therefore fail CLOSED — a proof
that genuinely pulls one of these helper members into its closure
over-rejects with `internal_error` rather than risk admitting an
unregistered auxiliary. This is the safe direction; revisit only if a real
run needs it, and only with a provenance-based check (the
`UsesOwnerToCtorIdx` smoke test pins this fail-closed direction).

The recursor-family and `sizeOf_spec` clauses ARE such provenance-based
checks. Lean generates those members EAGERLY when the owning inductive /
constructor is elaborated, so a genuine one is always declared in the same
module as its parent — that is what the module co-location conjunct
encodes. A hand-authored forgery of one of these names can only live in a
DIFFERENT module (in the parent's own module the name would collide with
the generated member wherever Lean generates it), so co-location keeps
every cross-module forgery a recorded, fail-closed dep key. Where Lean does
NOT generate the member (e.g. `brecOn` of a non-recursive inductive), an
own-module forgery does compile and is classified as generated — but the
transparent walk still traverses its type and value, so every axiom inside
(`sorryAx` included) surfaces in `kernel_axioms`; nothing hides.

A hand-written `theorem Owner.realAux` is none of these (it is a `thmInfo`),
so it is still recorded as a dependency key and still rejected — the
over-admission guard the check was built for is preserved. -/

/-! ## Root declaration resolution (namespaced-node fix)

The node name (e.g. `montgomery_reduce_Invariant`) is the BARE name a node
file is keyed by, but the on-disk declaration may be NAMESPACED. PV nodes are
extracted by Aeneas under a crate namespace: the node file opens
`namespace <crate>` in the free region above the `-- [TABLET NODE: …]` marker,
so the declaration lands at `<crate>.<nodeName>` even though the file/module
is still `Tablet.<nodeName>` and lake compiled it cleanly. A bare
`env.find? (nameFromString nodeName)` then returns `none` ⇒ a spurious
`missing_declaration`, blocking `formalization_complete` for every PV theorem.

Math nodes are unaffected: their declaration is top-level, so node name ==
decl name and the FAST PATH below resolves them exactly as before.

The supervisor now supplies the exact registered principal declaration.
`resolveRoot` accepts only that exact name when module metadata attributes it
to `Tablet.<nodeName>`. Declaration/file-stem resemblance is not consulted.
(This also closes the earlier bare-name fast-path provenance gap: resolution
reads only the node's own artifact `ModuleData.constants`, so an imported
root namesake can never be selected.) -/

/-- The module `Name` a constant is declared in, or `none` when the
environment has no module index for it (a seed / locally-elaborated decl). -/
private def declaringModule? (env : Environment) (name : Name) : Option Name :=
  match env.getModuleIdxFor? name with
  | some idx => env.allImportedModuleNames[idx]?
  | none     => none

private def isTabletModuleName (moduleName : Name) : Bool :=
  match moduleName.components.map (·.toString) with
  | "Tablet" :: _ :: _ => true
  | _ => false

private def tabletNodeIdFromModule? (moduleName : Name) : Option String :=
  match moduleName.components.map (·.toString) with
  | "Tablet" :: rest@(_ :: _) =>
      some (toString (rest.foldl Name.str Name.anonymous))
  | _ => none

/-- Artifact-local lookup.  Unlike `env.find?`, this cannot return another
provider's proof for a multiply-provided theorem name. -/
private def moduleConstantInfo? (env : Environment) (moduleName name : Name)
    : Option ConstantInfo := do
  let idx ← env.getModuleIdx? moduleName
  let data ← env.header.moduleData[idx.toNat]?
  data.constants.find? (·.name == name)

private def moduleData? (env : Environment) (moduleName : Name) : Option ModuleData := do
  let idx ← env.getModuleIdx? moduleName
  env.header.moduleData[idx.toNat]?

private structure ProviderInfo where
  moduleName : Name
  visibility : String
  info       : ConstantInfo

private structure ProviderIndex where
  providers : Std.HashMap Name (Array ProviderInfo) := {}
  extraNames : Std.HashMap Name (Array Name) := {}
  errors : Array String := #[]

/-- Build the complete Tablet provider multimap from the environment produced
with the consumer artifact's own import header and module-system level.  The
merged `const2ModIdx` table is deliberately not consulted. -/
private def buildProviderIndex (env : Environment) : ProviderIndex := Id.run do
  let mut result : ProviderIndex := {}
  for h : idx in [:env.header.moduleData.size] do
    let data := env.header.moduleData[idx]
    let effective := env.header.modules[idx]!
    let moduleName := effective.module
    if !isTabletModuleName moduleName then
      continue
    if data.constNames != data.constants.map (·.name) then
      result := { result with errors := result.errors.push s!"unsupported_toolchain: \
        {moduleName}: ModuleData.constNames != constants.map ConstantInfo.name" }
    let mut seen : Std.HashSet Name := {}
    let visibility := if data.isModule && effective.importAll then "private" else "exported"
    for info in data.constants do
      if seen.contains info.name then
        result := { result with errors := result.errors.push s!"unsupported_toolchain: \
          {moduleName}: duplicate logical constant {info.name} in one ownership manifest" }
      seen := seen.insert info.name
      let prior := result.providers[info.name]?.getD #[]
      let providerInfo : ProviderInfo := { moduleName, visibility, info }
      let providers := result.providers.insert info.name (prior.push providerInfo)
      result := { result with providers }
    for name in data.extraConstNames do
      let prior := result.extraNames[name]?.getD #[]
      let extraNames := result.extraNames.insert name (prior.push moduleName)
      result := { result with extraNames }
  result

private structure ProviderUse where
  moduleName : Name
  declaration : Name
  kind : String
  visibility : String
  deriving BEq, Hashable

/- Removed own-aux classifier rationale. Same-module declarations now share
active root's own module AND strictly namespaced under the active root name.
Never true for another node's decl or a genuine cross-node reference.

Fix A (own-node `let rec` aux): a user-named `let rec` binder inside the
active node's own proof is lifted by the elaborator to a real constant
`<activeRoot>.<binderName>` in the node's OWN module. It is an artifact of
the node's own elaboration (design intent: the §4.5 transparent-walk
filter), but the former name-shape classifier could not see
it, so it leaked into the dep records as a dotted boundary key that the
kernel's Patch C-K present-node validation fail-closes on. This predicate
is provenance-based: same declaring module as the active root (each node
compiles to its own module, so no other node's decl can satisfy it) AND
strictly below the active root's namespace. When either module index is
unavailable it returns `false` (fail-closed: the decl stays a recorded
dep). Used by BOTH collectors — the gates must stay mirrored or the dual
collectors disagree ⇒ `internal_error`. -/

/-! ## Legacy dependency-map owner attribution

The exact-use witness is authoritative. The three historical maps remain on
the wire for compatibility and diagnostics, and all attribute a declaration
to its declaring `Tablet.<owner>` module. In particular, declaration-name
shape and file-stem resemblance cannot affect a checking decision.

This also handles legitimate non-principal definitions such as:

* Preamble-shared structures (for example, `RecordType`) — a
  `structure` declared in module `Tablet.Preamble` (node id `Preamble`, a
  present node) but whose decl name is `<crate>.RecordType`, which does not
  sanitize to the stem `Preamble`.
* Aeneas-generated loop helpers (`<crate>.left_shift_loop0`,
  `…_loop0.body`, `parse_decimal_seq_loop0`) — extra defs Aeneas co-emits
  inside a model node's OWN module (e.g. `Tablet.left_shift`; node id
  `left_shift`, a present node), non-principal, and not name-shaped as a
  generated artifact (so the former classifier did not transparent-walk
  them).

The correct closure attribution for a DEFINITION dep is unconditional: a
definition dependency on ANY declaration living in module `Tablet.X` means
node `X` must be present and Lean-closed for the consumer to close —
regardless of whether the referenced decl is `X`'s principal declaration or a
co-generated helper. So the def-dep mapping uses the bare declaring-module
node id with no name-parity gate. This is sound BECAUSE it only ever maps onto
the declaring module's OWN node id (never onto a different node), and the
referenced decl genuinely lives in that present node's olean.

The exact name is retained independently in `exact_declaration_uses`, where
certificate issuance checks membership in that owner's replayed manifest. -/

/-- The NODE ID of any Tablet constant by its DECLARING MODULE alone: the
module `Name` under `Tablet.` with the leading `Tablet.` component stripped,
rendered as the node-id string. `none` when the constant has no module index
(seed / locally-elaborated decl) or its module is not under `Tablet.`. -/
private def tabletDefDepNodeId? (env : Environment) (name : Name) : Option String :=
  match (declaringModule? env name).map (·.components.map (·.toString)) with
  | some ("Tablet" :: rest@(_ :: _)) =>
      some (toString (rest.foldl Name.str Name.anonymous))
  | _ => none

/-- Canonicalize a Tablet DEFINITION-dependency's `Name` to its NODE ID for
emission. Maps any Tablet-module decl to its declaring-module node id
(`tabletDefDepNodeId?`, no principal gate); else preserves the legacy
`toString name` (non-Tablet / no module index). Applied at the
`strict_definition_deps` emission site only. -/
private def depDefKeyName (env : Environment) (name : Name) : String :=
  (tabletDefDepNodeId? env name).getD (toString name)

private def depKeyName (env : Environment) (name : Name) : String :=
  depDefKeyName env name

/-- Stable environment-constructor label used by declaration manifests and
exact-use observations.  This is classification by `ConstantInfo`, never by
declaration name. -/
private def declarationKind : ConstantInfo → String
  | .thmInfo _    => "theorem"
  | .axiomInfo _  => "axiom"
  | .opaqueInfo _ => "opaque"
  | .defnInfo _   => "definition"
  | .inductInfo _ => "inductive"
  | .ctorInfo _   => "constructor"
  | .recInfo _    => "recursor"
  | .quotInfo _   => "quotient"

/-- Exhaustive declaration set serialized by `Tablet.<nodeName>`'s
`ModuleData.constants`.  This is artifact membership, not merged attribution.
Certificate issuance compares this artifact-local read with the independently
stored replay-sidecar commitment only to detect races/plumbing mistakes. -/
private def moduleDeclarationManifest (env : Environment) (nodeName : String)
    : Array (Name × String) :=
  let entries := (moduleData? env (moduleForNode nodeName)).map (·.constants)
    |>.getD #[] |>.map fun info => (info.name, declarationKind info)
  entries.qsort (fun a b => toString a.1 < toString b.1)

/-- Outcome of resolving a node's root declaration. -/
private inductive RootResolution where
  /-- The exact registered root `Name`. -/
  | resolved (name : Name)
  /-- No declaration found ⇒ `missing_declaration` (unchanged). -/
  | missing
  /-- Legacy serialized variant retained to keep the result type stable. -/
  | ambiguous (candidates : Array Name)

/-- Resolve the exact registered declaration in the expected module. -/
private def resolveRoot (env : Environment) (nodeName registeredName : String)
    : RootResolution :=
  let exact := nameFromString registeredName
  let constants := (moduleData? env (moduleForNode nodeName)).map (·.constants) |>.getD #[]
  if constants.any (fun info => info.name == exact) then
    .resolved exact
  else
    -- In the module system declarations outside a `public section` are
    -- serialized with an artifact-specific private prefix.  The FILESPEC
    -- registration retains the user-facing name, so resolve through Lean's
    -- own reversible private-name view and require uniqueness.
    let candidates := constants.filterMap fun info =>
      if privateToUserName info.name == exact then some info.name else none
    if candidates.size == 1 then .resolved candidates[0]!
    else if candidates.isEmpty then .missing
    else .ambiguous candidates

/-! ## Side-effect record + visitor state -/

/-- The set of side effects produced by visiting a single `(provider, c, mode)`
pair. On a memo hit, the record is replayed into the current accumulator.
This is the per-plan-§4.4 alternative to a single-bit `seen` set, which
is **forbidden** because the same constant visited under different modes
produces different side effects. -/
private structure SideEffectRecord where
  axioms              : Array Name              := #[]
  boundaryTheorems    : Array (Name × String)   := #[]
  strictTheoremDeps   : Array (Name × String)   := #[]
  strictDefinitionDeps: Array (Name × String)   := #[]
  exactDeclarationUses: Array ProviderUse       := #[]
  errors              : Array String            := #[]
  deriving Inhabited

private structure VisitorState where
  /-- Accumulator: kernel-level axioms reached. Names only — Rust
  applies `load_approved_axioms` policy. -/
  kernelAxioms         : Std.HashSet Name                      := {}
  /-- Accumulator: Tablet boundary helpers (theorems referenced via
  `ProofMayAssumeTheorems` mode). Keyed by `Name`, value is the
  statement hash. Last-write-wins; for a fixed environment, the hash
  is deterministic, so multiple visits can only produce the same hash. -/
  boundaryTheorems     : Std.HashMap Name String               := {}
  /-- Accumulator: theorems reached in `Strict` mode. Hash is over
  type+value (the proof). -/
  strictTheoremDeps    : Std.HashMap Name String               := {}
  /-- Accumulator: defs/abbrevs/inductives reached in `Strict` mode.
  Hash is over type (and constructor names for inductives). -/
  strictDefinitionDeps : Std.HashMap Name String               := {}
  /-- Exact cross-module Tablet declarations reached.  Ownership is derived
  solely from declaring-module metadata. -/
  exactDeclarationUses : Std.HashSet ProviderUse                := {}
  /-- Errors raised during traversal (e.g. unsafe declarations,
  partial blocks, missing decls below the root). Surface to the JSON
  `errors` array; do NOT abort. -/
  errors               : Array String                          := #[]
  /-- The active root constant (cycle-guard target). -/
  active               : Name                                  := .anonymous
  activeModule         : Name                                  := .anonymous
  providers            : Std.HashMap Name (Array ProviderInfo) := {}
  extraNames           : Std.HashMap Name (Array Name)         := {}
  /-- Ownership-manifest entries explicitly used as primary collector seeds.
  Emitted for the falsifier-hunting coverage assertion. -/
  seededDeclarations   : Std.HashSet Name                      := {}
  /-- Per-`(provider module, c, mode)` memo.  Provider identity is
  load-bearing because duplicate theorem providers may carry different proof
  values. -/
  memo                 : Std.HashMap (Name × Name × VisitMode) SideEffectRecord := {}

private abbrev VisitorM := StateRefT VisitorState CoreM

/-- Add an axiom name to the accumulator and return its singleton record entry. -/
private def recordAxiom (rec : SideEffectRecord) (a : Name) : SideEffectRecord :=
  { rec with axioms := rec.axioms.push a }

private def recordBoundary (rec : SideEffectRecord) (n : Name) (h : String) : SideEffectRecord :=
  { rec with boundaryTheorems := rec.boundaryTheorems.push (n, h) }

private def recordStrictThm (rec : SideEffectRecord) (n : Name) (h : String) : SideEffectRecord :=
  { rec with strictTheoremDeps := rec.strictTheoremDeps.push (n, h) }

private def recordStrictDef (rec : SideEffectRecord) (n : Name) (h : String) : SideEffectRecord :=
  { rec with strictDefinitionDeps := rec.strictDefinitionDeps.push (n, h) }

private def recordExactUse (rec : SideEffectRecord) (use : ProviderUse) : SideEffectRecord :=
  { rec with exactDeclarationUses := rec.exactDeclarationUses.push use }

private def recordError (rec : SideEffectRecord) (msg : String) : SideEffectRecord :=
  { rec with errors := rec.errors.push msg }

/-- Replay a `SideEffectRecord` into the current `VisitorState`. -/
private def applyRecord (rec : SideEffectRecord) : VisitorM Unit := do
  modify fun s =>
    let kernelAxioms := rec.axioms.foldl (init := s.kernelAxioms) (·.insert ·)
    let boundaryTheorems := rec.boundaryTheorems.foldl
      (init := s.boundaryTheorems) (fun m (n, h) => m.insert n h)
    let strictTheoremDeps := rec.strictTheoremDeps.foldl
      (init := s.strictTheoremDeps) (fun m (n, h) => m.insert n h)
    let strictDefinitionDeps := rec.strictDefinitionDeps.foldl
      (init := s.strictDefinitionDeps) (fun m (n, h) => m.insert n h)
    let exactDeclarationUses := rec.exactDeclarationUses.foldl
      (init := s.exactDeclarationUses) (fun set use => set.insert use)
    let errors := s.errors ++ rec.errors
    { s with kernelAxioms, boundaryTheorems, strictTheoremDeps,
             strictDefinitionDeps, exactDeclarationUses, errors }

/-! ## Non-Tablet axiom collection -/

/-- For a non-Tablet constant, dispatch to Lean's public
`Lean.collectAxioms` and return the array of axioms. Errors collected
via `Lean.collectAxioms` are caught and surfaced as `errors`-array entries
(one error per failure), not propagated. -/
private def collectExternAxioms (c : Name) : VisitorM (Array Name) := do
  try
    -- `Lean.collectAxioms` requires `MonadEnv`; `VisitorM` has it via the
    -- `CoreM` base. The exported axioms ext is per-module-cached
    -- (Lean/Util/CollectAxioms.lean:96-146), so this stays cheap for
    -- imported sub-trees.
    let axs ← (Lean.collectAxioms c : CoreM (Array Name))
    return axs
  catch e =>
    let msg ← e.toMessageData.toString
    modify fun s => { s with
      errors := s.errors.push s!"collectAxioms({c}) failed: {msg}" }
    return #[]

/-! ## The 9-arm visitor (plan §4.3) -/

/-- Visit one constant under one mode. Cycle-guard short-circuits on the
active root. Memoized on `(provider, c, mode)` with side-effect record replay. -/
private partial def visitConst (provider c : Name) (mode : VisitMode) : VisitorM Unit := do
  -- Provider identity is part of the memo key.  A name-only cache can replay
  -- another artifact's theorem proof when Lean coalesces duplicate providers.
  if let some record := (← get).memo[(provider, c, mode)]? then
    applyRecord record
    return

  -- Capture pre-visit accumulator snapshots so the side-effect record we
  -- store reflects only THIS visit's contribution. We compute the record
  -- by running the visit against an empty accumulator, then merge the
  -- record into the live accumulator. To avoid threading two states, we
  -- instead build the record incrementally by stashing what we add and
  -- mutating both record and accumulator together.
  let env ← getEnv
  let mut record : SideEffectRecord := {}

  -- Place a sentinel in the memo to break recursion through cycles
  -- among non-active consts (e.g. mutual inductive↔ctor↔rec triangles).
  -- The sentinel is replaced by the real record once the visit finishes.
  modify fun s => { s with memo := s.memo.insert (provider, c, mode) {} }

  let activeModule := (← get).activeModule
  match moduleConstantInfo? env activeModule c with
  | some info =>
      -- The active artifact's own value wins for traversal even if a different
      -- provider won Lean's merged name slot.
      match info with
      | .thmInfo v | .defnInfo v | .opaqueInfo v =>
          for child in v.type.getUsedConstants do
            visitConst activeModule child .strict
          for child in v.value.getUsedConstants do
            visitConst activeModule child mode
      | .inductInfo v =>
          for child in v.type.getUsedConstants do
            visitConst activeModule child .strict
          for ctor in v.ctors do
            visitConst activeModule ctor .strict
      | .ctorInfo v | .recInfo v =>
          for child in v.type.getUsedConstants do
            visitConst activeModule child .strict
      | .axiomInfo v =>
          record := recordAxiom record c
          modify fun s => { s with kernelAxioms := s.kernelAxioms.insert c }
          for child in v.type.getUsedConstants do
            visitConst activeModule child .strict
      | .quotInfo _ => pure ()
      if info.isUnsafe then
        let msg := s!"unsafe declaration in closure: {c}"
        record := recordError record msg
        modify fun s => { s with errors := s.errors.push msg }
  | none =>
      let providers := (← get).providers[c]?.getD #[]
      if !providers.isEmpty then
        -- Expr.const does not retain a provider.  Record every visible Tablet
        -- artifact provider conservatively; issuance requires every root and
        -- exact visibility-level membership witness.
        for providerInfo in providers do
          let use : ProviderUse := {
            moduleName := providerInfo.moduleName
            declaration := c
            kind := declarationKind providerInfo.info
            visibility := providerInfo.visibility
          }
          record := recordExactUse record use
          modify fun s => { s with exactDeclarationUses := s.exactDeclarationUses.insert use }
          match providerInfo.info with
          | .thmInfo v =>
              let h := statementHash v.type
              if mode == .proofMayAssumeTheorems then
                record := recordBoundary record c h
                modify fun s => { s with boundaryTheorems := s.boundaryTheorems.insert c h }
              else
                record := recordStrictThm record c h
                modify fun s => { s with strictTheoremDeps := s.strictTheoremDeps.insert c h }
          | .defnInfo v =>
              let h := semanticHash v.type [v.value]
              record := recordStrictDef record c h
              modify fun s => { s with strictDefinitionDeps := s.strictDefinitionDeps.insert c h }
          | .inductInfo v =>
              let h := semanticHash v.type
              record := recordStrictDef record c h
              modify fun s => { s with strictDefinitionDeps := s.strictDefinitionDeps.insert c h }
          | .ctorInfo v | .recInfo v =>
              let h := semanticHash v.type
              record := recordStrictDef record c h
              modify fun s => { s with strictDefinitionDeps := s.strictDefinitionDeps.insert c h }
          | .axiomInfo _ | .opaqueInfo _ | .quotInfo _ => pure ()
          if providerInfo.info.isUnsafe then
            let msg := s!"unsafe declaration in closure: {providerInfo.moduleName}::{c}"
            record := recordError record msg
            modify fun s => { s with errors := s.errors.push msg }
      else if let some extraProviders := (← get).extraNames[c]? then
        let msg := s!"unsupported_toolchain: logical traversal reached IR-only \
          extraConstNames entry {c} from providers {extraProviders.map toString}"
        record := recordError record msg
        modify fun s => { s with errors := s.errors.push msg }
      else if (env.find? c).isSome then
        -- The provider index exhaustively enumerates every serialized Tablet
        -- provider visible at this artifact level. Absence from that index is
        -- the authority for taking the external-library path; merged
        -- const2ModIdx attribution is not consulted.
        let axs ← collectExternAxioms c
        for a in axs do record := recordAxiom record a
        modify fun s => { s with
          kernelAxioms := axs.foldl (init := s.kernelAxioms) (·.insert ·) }
      else
        let msg := s!"missing constant during traversal: {c}"
        record := recordError record msg
        modify fun s => { s with errors := s.errors.push msg }

  -- Replace the memo sentinel with the actual record for this provider key.
  modify fun s => { s with memo := s.memo.insert (provider, c, mode) record }

/-! ## Root dispatch (plan §4.2) -/

inductive RootKind where
  | theorem
  | lemma_
  | def_
  | abbrev_
  | axiom_
  | opaque_
  | other
  deriving Inhabited

private def RootKind.toString : RootKind → String
  | .theorem  => "theorem"
  | .lemma_   => "lemma"
  | .def_     => "def"
  | .abbrev_  => "abbrev"
  | .axiom_   => "axiom"
  | .opaque_  => "opaque"
  | .other    => "other"

private def classifyRoot (info : ConstantInfo) : RootKind :=
  match info with
  | .thmInfo _ => .theorem
  | .defnInfo v =>
      -- `abbrev X := ...` ⇒ defnInfo with hints .abbrev.
      if v.hints matches .abbrev then .abbrev_ else .def_
  | .axiomInfo _   => .axiom_
  | .opaqueInfo _  => .opaque_
  | _              => .other

/-- Visit the root declaration per plan §4.2. Returns the root kind for
the JSON envelope. Roots that are not theorems/lemmas/defs/abbrevs are
recorded as errors (plan §4.2: "→ reject"); the script still emits a
well-formed JSON so the Rust wrapper can read structured failure data. -/
private def visitRoot (rootName : Name) : VisitorM RootKind := do
  let env ← getEnv
  let activeModule := (← get).activeModule
  match moduleConstantInfo? env activeModule rootName with
  | none =>
      modify fun s => { s with
        errors := s.errors.push s!"missing root declaration: {rootName}" }
      return .other
  | some info =>
      let kind := classifyRoot info
      match info with
      | .thmInfo v =>
          let provider := activeModule
          for child in v.type.getUsedConstants do
            visitConst provider child .strict
          for child in v.value.getUsedConstants do
            visitConst provider child .proofMayAssumeTheorems
      | .defnInfo v =>
          let provider := activeModule
          -- Plan §4.2: definition root visits value Strict (no
          -- Prop-valued special case — that was dropped in this revision).
          for child in v.type.getUsedConstants do
            visitConst provider child .strict
          for child in v.value.getUsedConstants do
            visitConst provider child .strict
      | .axiomInfo _ | .opaqueInfo _ =>
          modify fun s => { s with
            errors := s.errors.push s!"root is axiom/opaque: {rootName}" }
      | _ =>
          modify fun s => { s with
            errors := s.errors.push
              s!"unsupported root kind for {rootName}; expected theorem/lemma/def/abbrev" }
      return kind

/-! ## Axiomization cross-check (plan §4.6.1)

The traversal above is the *primary* collector — a hand-rolled
per-`(provider, c, mode)` visitor with exact declaration-use ownership evidence. The
legacy `boundary_theorems` / `strict_theorem_deps` /
`strict_definition_deps` fields remain wire-format diagnostics only. The mode-aware dispatch
is subtle. To defend against bugs in the primary (or future Lean
elaboration changes that violate its assumptions), we run a *secondary*
collector side-by-side: an `Lean.CollectAxioms.collect`-shaped pass with
the same artifact-local provider boundary, but no primary-mode partition. It
emits only `{kernel_axioms, boundary_theorems}`
and the primary's JSON now carries a parallel `axiomization_check`
sub-object for the Rust wrapper to compare.

Comparison rule (plan §4.6.1): set equality on `kernel_axioms` AND on
the *set of all Tablet theorem names reached transitively*. The
primary partitions those reached theorems into two accumulators
(`boundaryTheorems` for theorems reached via a `proofMayAssumeTheorems`
path, `strictTheoremDeps` for theorems reached via a `.strict` path —
e.g. from inside a definition's body), so the comparison uses
`primary.boundaryTheorems ∪ primary.strictTheoremDeps` against
`axcheck.boundaryTheorems` (axcheck has no modes; every Tablet
theorem it reaches lands in one set). Comparing only
`primary.boundaryTheorems` is a bug: theorems reached strictly
through definitions look like `axcheck_only` false positives.
Disagreement is a runtime-invariant violation; the wrapper flips
`status` to `internal_error`. Default-on; disable via env var
`TRELLIS_LOCAL_CLOSURE_AXCHECK_DISABLE=1` or CLI flag `--no-axcheck`.

The two collectors run against the same already-loaded `Environment`,
sharing the env-load cost. Cost note in plan §4.6.1: doubles probe
Lean-time (~4-30s vs ~2-15s). -/

private inductive AxTraversalMode where
  | axiomization
  deriving BEq, Hashable

private structure AxCheckState where
  /-- Constants already visited (mode-independent: the axiomization
  customization makes every Tablet theorem a boundary regardless of how
  it was reached). -/
  seen             : Std.HashSet (Name × Name × AxTraversalMode) := {}
  /-- Non-Tablet axioms reached transitively. -/
  kernelAxioms     : Std.HashSet Name := {}
  /-- Tablet theorems reached transitively (axiomized: recorded here,
  not recursed into). -/
  boundaryTheorems : Std.HashSet Name := {}
  /-- Exact declarations reached across certified Tablet-module boundaries. -/
  exactDeclarationUses : Std.HashSet ProviderUse := {}
  /-- Errors raised during traversal. -/
  errors           : Array String     := #[]
  /-- Fix A: the active root constant, mirroring `VisitorState.active`.
  Used to identify the active declaring module. Both collectors traverse
  every declaration in that module and cut only at a different Tablet
  module, so they must share this exact owner identity. -/
  active           : Name             := .anonymous
  activeModule     : Name             := .anonymous
  providers        : Std.HashMap Name (Array ProviderInfo) := {}
  extraNames       : Std.HashMap Name (Array Name) := {}
  /-- Ownership-manifest entries explicitly used as secondary collector seeds. -/
  seededDeclarations : Std.HashSet Name := {}

private abbrev AxCheckM := StateRefT AxCheckState IO

/-- Visit all sub-consts of `e` under the axcheck collector. -/
private partial def axCheckExpr (env : Environment) (e : Expr) : AxCheckM Unit := do
  for c in e.getUsedConstants do
    axCheckCollect env c
where
  /-- Mirror of `Lean.CollectAxioms.collect`, with the Tablet-theorem
  axiomization customization and the §4.5 transparent-walk fix for
  internal-detail artifacts. We take constants from the kernel env (per
  the stdlib precedent at CollectAxioms.lean:55) so async errors do not
  divert this traversal. -/
  axCheckCollect (env : Environment) (c : Name) : AxCheckM Unit := do
    let activeModule := (← get).activeModule
    let key := (activeModule, c, AxTraversalMode.axiomization)
    if (← get).seen.contains key then return
    modify fun s => { s with seen := s.seen.insert key }
    match moduleConstantInfo? env activeModule c with
    | some info =>
        match info with
        | .thmInfo v   => axCheckExpr env v.type; axCheckExpr env v.value
        | .defnInfo v  => axCheckExpr env v.type; axCheckExpr env v.value
        | .opaqueInfo v => axCheckExpr env v.type; axCheckExpr env v.value
        | .inductInfo v =>
            axCheckExpr env v.type
            for ctor in v.ctors do axCheckCollect env ctor
        | .ctorInfo v  => axCheckExpr env v.type
        | .recInfo v   => axCheckExpr env v.type
        | .axiomInfo v =>
            modify fun s => { s with kernelAxioms := s.kernelAxioms.insert c }
            axCheckExpr env v.type
        | .quotInfo _ => pure ()
    | none =>
        let providers := (← get).providers[c]?.getD #[]
        if !providers.isEmpty then
          for providerInfo in providers do
            let use : ProviderUse := {
              moduleName := providerInfo.moduleName
              declaration := c
              kind := declarationKind providerInfo.info
              visibility := providerInfo.visibility
            }
            modify fun s => { s with
              exactDeclarationUses := s.exactDeclarationUses.insert use
              boundaryTheorems :=
                if providerInfo.info matches .thmInfo _ then s.boundaryTheorems.insert c
                else s.boundaryTheorems }
        else if let some extraProviders := (← get).extraNames[c]? then
          modify fun s => { s with errors := s.errors.push s!"unsupported_toolchain: \
            logical traversal reached IR-only extraConstNames entry {c} from \
            providers {extraProviders.map toString}" }
        else if (env.checked.get.find? c).isSome then
          -- Non-Tablet const: full recursive walk mirroring
          -- `Lean.CollectAxioms.collect`. This differs in shape from the
          -- primary (which delegates to `Lean.collectAxioms` at the
          -- non-Tablet boundary), but the *transitive axiom set* is the
          -- same, which is what the cross-check compares.
          match env.checked.get.find? c with
          | none => pure ()
          | some info => match info with
          | .axiomInfo v =>
              modify fun s => { s with
                kernelAxioms := s.kernelAxioms.insert c }
              axCheckExpr env v.type
          | .defnInfo v   => axCheckExpr env v.type; axCheckExpr env v.value
          | .thmInfo v    => axCheckExpr env v.type; axCheckExpr env v.value
          | .opaqueInfo v => axCheckExpr env v.type; axCheckExpr env v.value
          | .quotInfo _   => pure ()
          | .ctorInfo v   => axCheckExpr env v.type
          | .recInfo v    => axCheckExpr env v.type
          | .inductInfo v =>
              axCheckExpr env v.type
              for ctor in v.ctors do axCheckCollect env ctor
        else
          modify fun s => { s with
            errors := s.errors.push s!"missing constant during traversal: {c}" }

/-- Top-level axcheck traversal: dispatch on the root's kind, walk
type and value (for theorems/defs/abbrevs). Mirrors the primary
script's `visitRoot` (plan §4.2). Root itself is NOT recorded. -/
private def axCheckRoot (env : Environment) (rootName : Name) : AxCheckM Unit := do
  let activeModule := (← get).activeModule
  match moduleConstantInfo? env activeModule rootName with
  | none =>
      modify fun s => { s with
        errors := s.errors.push s!"missing root declaration: {rootName}" }
  | some info =>
      match info with
      | .thmInfo v =>
          axCheckExpr env v.type
          axCheckExpr env v.value
      | .defnInfo v =>
          axCheckExpr env v.type
          axCheckExpr env v.value
      | .axiomInfo _ | .opaqueInfo _ =>
          modify fun s => { s with
            errors := s.errors.push s!"root is axiom/opaque: {rootName}" }
      | _ =>
          modify fun s => { s with
            errors := s.errors.push
              s!"unsupported root kind for {rootName}; expected theorem/lemma/def/abbrev" }

/-! ## JSON emission -/

private def stableSort (xs : List String) : List String :=
  (xs.toArray.qsort (· < ·)).toList

/-- Emit the per-pair `[{"name": "...", "<hashField>": "..."}, ...]` array
sorted by name for determinism. The `name` field is the cross-node dep's
NODE ID, NOT the raw Lean `Name`: a namespaced (PV) dep's decl is
`<crate>.<node>` but its node id is its `Tablet.`-stripped module suffix.
`keyer` is retained for the stable wire shape, but every category attributes
by declaring module. Exact reached names live in `exact_declaration_uses`.
The hash field is emitted verbatim (content hash; unaffected by the name
canonicalization). Sorting is by the EMITTED node-id key so the array order
matches what the kernel sees. -/
private def pairArrayJson (env : Environment) (hashField : String)
    (keyer : Environment → Name → String)
    (pairs : List (Name × String)) : Json :=
  let keyed : Array (String × String) :=
    (pairs.toArray.map fun (n, h) => (keyer env n, h))
  let sorted := keyed.qsort (fun a b => a.1 < b.1)
  let items : Array Json := sorted.map fun (k, h) =>
    Json.mkObj [
      ("name", Json.str k),
      (hashField, Json.str h)
    ]
  Json.arr items

private def declarationManifestJson (entries : Array (Name × String)) : Json :=
  Json.arr (entries.map fun (name, kind) => Json.mkObj [
    ("name", Json.str (toString name)),
    ("kind", Json.str kind)
  ])

private def exactDeclarationUsesJson (uses : Std.HashSet ProviderUse) : Json :=
  let sorted := uses.toArray.qsort fun a b =>
    s!"{a.moduleName}::{a.declaration}@{a.visibility}:{a.kind}" <
      s!"{b.moduleName}::{b.declaration}@{b.visibility}:{b.kind}"
  Json.arr (sorted.map fun use =>
    let owner := (tabletNodeIdFromModule? use.moduleName).getD ""
    Json.mkObj [
      ("owner", Json.str owner),
      ("reached_declaration", Json.str (toString use.declaration)),
      ("declaration_kind", Json.str use.kind),
      ("visibility", Json.str use.visibility)
    ])

/-- Compute set differences between primary and axcheck for diagnostic
output. Returns sorted name lists. -/
private def setDiff (a b : Std.HashSet Name) : List String :=
  stableSort (a.toList.filter (fun n => !b.contains n) |>.map toString)

private def providerUseString (use : ProviderUse) : String :=
  s!"{use.moduleName}::{use.declaration}@{use.visibility}:{use.kind}"

private def providerUseDiff (a b : Std.HashSet ProviderUse) : List String :=
  stableSort (a.toList.filter (fun use => !b.contains use) |>.map providerUseString)

/-- Patch C-K Fix 3 (audit MEDIUM): build a JSON sub-object for the
axiomization_check field when the secondary collector throws. Distinct
from the operator-disabled skip path (`skipped: true`): a crash is an
implementation bug and must surface loudly. The sub-object carries
`agreed: false, skipped: false` so the existing Rust parser's
"disagreement" arm flips status to `internal_error`; the extra `error`
field plus the top-level `axiomization_check_crash:` prefix let the
Rust wrapper distinguish "crash" from "real disagreement" in the
diagnostic shown to operators.

Previously the script swallowed crashes by emitting `skipped: true`,
which the wrapper treated as a legitimate operator opt-out (trivial
pass). That silently disabled the safety cross-check whenever the
collector bugged out — audit MEDIUM finding. -/
private def axiomizationCheckCrashJson (msg : String) : Json :=
  Json.mkObj [
    ("kernel_axioms",            Json.arr #[]),
    ("boundary_theorems",        Json.arr #[]),
    ("agreed",                   Json.bool false),
    ("skipped",                  Json.bool false),
    ("primary_only_axioms",      Json.arr #[]),
    ("axcheck_only_axioms",      Json.arr #[]),
    ("primary_only_boundaries",  Json.arr #[]),
    ("axcheck_only_boundaries",  Json.arr #[]),
    ("error",                    Json.str msg)
  ]

/-- Build the `axiomization_check` JSON sub-object from primary + secondary
collector outputs. Comparison rule (plan §4.6.1):

* `kernel_axioms`: primary's `kernelAxioms` (the secondary mirrors).
* `boundary_theorems` (axcheck side): name-set of every reached Tablet
  theorem (axcheck is mode-less).
* Equality is set-equality on `kernel_axioms` AND on the set of
  reached Tablet theorem names. Because primary partitions reached
  theorems across `boundaryTheorems` (PMAT-reached) and
  `strictTheoremDeps` (strict-reached, e.g. via a def's body), the
  primary-side comparison set is the UNION of those two accumulators.
  Comparing only `primary.boundaryTheorems` was a test-design bug:
  see the comment inside the `else` branch.
* `primary_only_*` / `axcheck_only_*` carry set-differences for
  diagnostics on disagreement. -/
private def axiomizationCheckJson
    (primary : VisitorState)
    (axcheck : AxCheckState)
    (skipped : Bool)
    (ownershipManifest : Std.HashSet Name := {}) : Json :=
  if skipped then
    -- Skip-flag handling (plan §4.6.1 disable flag): emit an
    -- "agreed: true, skipped: true" sub-object. The Rust wrapper
    -- treats `skipped: true` as a trivial pass.
    Json.mkObj [
      ("kernel_axioms",            Json.arr #[]),
      ("boundary_theorems",        Json.arr #[]),
      ("agreed",                   Json.bool true),
      ("skipped",                  Json.bool true),
      ("primary_only_axioms",      Json.arr #[]),
      ("axcheck_only_axioms",      Json.arr #[]),
      ("primary_only_boundaries",  Json.arr #[]),
      ("axcheck_only_boundaries",  Json.arr #[]),
      ("primary_seeded_declarations", Json.arr #[]),
      ("axcheck_seeded_declarations", Json.arr #[]),
      ("seed_coverage_agreed",     Json.bool true)
    ]
  else
    -- Build the primary-side name set for comparison. The axcheck
    -- collector is mode-less: every Tablet theorem it reaches goes
    -- into `boundaryTheorems`, regardless of whether the path was a
    -- proof-body or a def-body. The primary collector splits those
    -- same theorems into TWO accumulators depending on the path:
    --   * `boundaryTheorems` — theorems reached via a
    --     `proofMayAssumeTheorems` path (the typical case: the active
    --     theorem's proof body references another theorem).
    --   * `strictTheoremDeps` — theorems reached via a `.strict` path
    --     (e.g. the active theorem's proof body references a
    --     definition, whose body in turn references a theorem — the
    --     def→child traversal at the .defnInfo arm always passes
    --     `.strict`, so theorems reached from inside a def's body land
    --     here even when the outer call was PMAT).
    -- For the cross-check's set-equality to hold against axcheck's
    -- merged set, primary's comparison set must be the union of the
    -- two reach buckets. Comparing only `boundaryTheorems` produced
    -- spurious `axcheck_only_boundaries` whenever the active proof
    -- referenced a Tablet theorem strictly through a def body — first
    -- observed at example-run cycle 44 (Worker#185), when the active
    -- theorem's proof used a Subtype constructor whose membership
    -- field was a Tablet theorem inside a `def`'s body. (See
    -- LOCAL_CLOSURE_IMPL_PLAN.md §4.6.1.)
    let primaryReachedNames := primary.exactDeclarationUses
    let axiomsAgree :=
      primary.kernelAxioms.toList.all axcheck.kernelAxioms.contains
        && axcheck.kernelAxioms.toList.all primary.kernelAxioms.contains
    let boundariesAgree :=
      primaryReachedNames.toList.all axcheck.exactDeclarationUses.contains
        && axcheck.exactDeclarationUses.toList.all primaryReachedNames.contains
    let seedCoverageAgree :=
      primary.seededDeclarations.toList.all ownershipManifest.contains
        && ownershipManifest.toList.all primary.seededDeclarations.contains
        && axcheck.seededDeclarations.toList.all ownershipManifest.contains
        && ownershipManifest.toList.all axcheck.seededDeclarations.contains
    let agreed := axiomsAgree && boundariesAgree && seedCoverageAgree
    let kernelAxList := stableSort (axcheck.kernelAxioms.toList.map toString)
    let boundaryList := stableSort (axcheck.exactDeclarationUses.toList.map providerUseString)
    let primaryOnlyAx     := setDiff primary.kernelAxioms axcheck.kernelAxioms
    let axcheckOnlyAx     := setDiff axcheck.kernelAxioms primary.kernelAxioms
    let primaryOnlyBnd    := providerUseDiff primaryReachedNames axcheck.exactDeclarationUses
    let axcheckOnlyBnd    := providerUseDiff axcheck.exactDeclarationUses primaryReachedNames
    let primarySeeded := stableSort (primary.seededDeclarations.toList.map toString)
    let axcheckSeeded := stableSort (axcheck.seededDeclarations.toList.map toString)
    Json.mkObj [
      ("kernel_axioms",            Json.arr (kernelAxList.toArray.map Json.str)),
      ("boundary_theorems",        Json.arr (boundaryList.toArray.map Json.str)),
      ("agreed",                   Json.bool agreed),
      ("skipped",                  Json.bool false),
      ("primary_only_axioms",      Json.arr (primaryOnlyAx.toArray.map Json.str)),
      ("axcheck_only_axioms",      Json.arr (axcheckOnlyAx.toArray.map Json.str)),
      ("primary_only_boundaries",  Json.arr (primaryOnlyBnd.toArray.map Json.str)),
      ("axcheck_only_boundaries",  Json.arr (axcheckOnlyBnd.toArray.map Json.str)),
      ("primary_seeded_declarations", Json.arr (primarySeeded.toArray.map Json.str)),
      ("axcheck_seeded_declarations", Json.arr (axcheckSeeded.toArray.map Json.str)),
      ("seed_coverage_agreed",     Json.bool seedCoverageAgree)
    ]

private def emitJson
    (env      : Environment)
    (nodeName : String)
    (principalName : String)
    (status   : String)
    (rootKind : String)
    (s        : VisitorState)
    (axCheck  : Json) : String :=
  -- `kernel_axioms` are BOUNDARY consts (mathlib/Aeneas axioms), NOT Tablet
  -- cross-node deps, so they keep their verbatim `toString` handling (the
  -- kernel applies `load_approved_axioms` to them, not present-node mapping).
  let kernelAxiomsList := stableSort (s.kernelAxioms.toList.map toString)
  let kernelAxiomsArr  : Array Json := kernelAxiomsList.toArray.map Json.str
  -- The three cross-node dep maps emit NODE IDs, so a namespaced (PV) dep
  -- maps to its bare node id and passes the kernel's Patch C-K present-node
  -- check. These maps are compatibility telemetry; exact-use evidence is the
  -- certificate authority.
  let boundaryArr      := pairArrayJson env "statement_hash" depKeyName
    (s.boundaryTheorems.toList)
  let strictThmArr     := pairArrayJson env "value_hash" depKeyName
    (s.strictTheoremDeps.toList)
  -- Every category attributes by declaring module. A declaration rename does
  -- not change its owner; the exact name is retained separately above.
  let strictDefArr     := pairArrayJson env "semantic_hash" depDefKeyName
    (s.strictDefinitionDeps.toList)
  let errorsArr        : Array Json := s.errors.map Json.str
  let declarationManifest := moduleDeclarationManifest env nodeName
  let payload : Json := Json.mkObj [
    ("node",                  Json.str nodeName),
    ("status",                Json.str status),
    ("root_kind",             Json.str rootKind),
    ("principal_declaration", Json.str principalName),
    ("declaration_manifest",  declarationManifestJson declarationManifest),
    ("exact_declaration_uses",exactDeclarationUsesJson s.exactDeclarationUses),
    ("kernel_axioms",         Json.arr kernelAxiomsArr),
    ("boundary_theorems",     boundaryArr),
    ("strict_theorem_deps",   strictThmArr),
    ("strict_definition_deps",strictDefArr),
    ("errors",                Json.arr errorsArr),
    ("axiomization_check",    axCheck)
  ]
  payload.compress

/-! ## Top-level entry -/

/-- Run the visitor over `rootName` against `env`, returning the final
state and root kind. -/
private def runClosure (env : Environment) (nodeName : String) (rootName : Name)
    (providerIndex : ProviderIndex)
    : IO (VisitorState × RootKind) := do
  let coreCtx : Core.Context := {
    fileName := "<lean_local_closure>",
    fileMap  := default
  }
  let coreState : Core.State := { env := env }
  let activeModule := moduleForNode nodeName
  let initialVisitor : VisitorState := {
    active := rootName
    activeModule
    providers := providerIndex.providers
    extraNames := providerIndex.extraNames
    errors := providerIndex.errors
  }
  let manifest := moduleDeclarationManifest env nodeName
  let visitor : VisitorM RootKind := do
    for (name, _) in manifest do
      modify fun s => { s with seededDeclarations := s.seededDeclarations.insert name }
      visitConst activeModule name .strict
    return (moduleConstantInfo? env activeModule rootName).map classifyRoot |>.getD .other
  let action : CoreM (RootKind × VisitorState) := do
    StateRefT'.run visitor initialVisitor
  let ((kind, finalSt), _) ← action.toIO coreCtx coreState
  return (finalSt, kind)

/-- Run the axcheck collector over `rootName` against `env`, returning
the final axcheck state. -/
private def runAxiomizationCheck (env : Environment) (nodeName : String) (rootName : Name)
    (providerIndex : ProviderIndex)
    : IO AxCheckState := do
  -- Seed the exact active owner so both collectors use identical
  -- declaring-module boundaries.
  let activeModule := moduleForNode nodeName
  let manifest := moduleDeclarationManifest env nodeName
  let action : AxCheckM Unit := do
    for (name, _) in manifest do
      modify fun s => { s with seededDeclarations := s.seededDeclarations.insert name }
      axCheckExpr env (.const name [])
  let (_, finalSt) ← action.run {
    active := rootName
    activeModule
    providers := providerIndex.providers
    extraNames := providerIndex.extraNames
    errors := providerIndex.errors
  }
  return finalSt

/-- JSON for an early-failure path (e.g. import failed). The status is
provided by the caller; only the node name and an `errors` list survive.

The `axiomization_check` field is emitted with `skipped: true` so the
shape stays stable; the Rust wrapper treats `skipped: true` as a
trivial pass independent of the top-level status. -/
private def emitFailureJson (nodeName : String) (status : String)
    (errors : Array String) : String :=
  let axCheck := axiomizationCheckJson {} {} (skipped := true)
  let payload : Json := Json.mkObj [
    ("node",                  Json.str nodeName),
    ("status",                Json.str status),
    ("root_kind",             Json.str "other"),
    ("kernel_axioms",         Json.arr #[]),
    ("boundary_theorems",     Json.arr #[]),
    ("strict_theorem_deps",   Json.arr #[]),
    ("strict_definition_deps",Json.arr #[]),
    ("errors",                Json.arr (errors.map Json.str)),
    ("axiomization_check",    axCheck)
  ]
  payload.compress

/-- Result of the source-policy operation. Policy violations are intentionally
distinct from checker failures on the wire. -/
private inductive OwnerPolicyScanOutcome where
  | ok
  | rejected (message : String)
  | internalError (message : String)

/-- Parse the node source for worker-authoring policy only. Certificate probes
never call this function. -/
private def ownerFilePolicyScan (nodeName : String) : IO OwnerPolicyScanOutcome := do
  let scan : IO OwnerPolicyScanOutcome := do
    let result ← scanOwnerPolicyFile nodeName
    match result.unreadable with
    | some path =>
        return .internalError s!"authoring-policy scan could not read node source {path}"
    | none =>
        if !result.policyCommands.isEmpty then
          return .rejected s!"FILESPEC authoring policy forbids construct(s) \
            {result.policyCommands.toList}: macro/syntax/elaborator definitions must \
            be `local`; `declare_syntax_cat`, `run_cmd`, `run_elab`, `run_meta`, \
            `by_elab`, `initialize`, and `builtin_initialize` are forbidden"
        if !result.partialNames.isEmpty then
          let named := String.intercalate ", " result.partialNames.toList
          return .rejected s!"FILESPEC authoring policy forbids explicit `partial` \
            declaration(s): {named}"
        return .ok
  match (← scan.toBaseIO) with
  | .ok r    => return r
  | .error e => return .internalError s!"authoring-policy scan failed: {e}"

/-- Run the traversal against a populated environment and emit JSON.
Wraps internal-error catching separately from import-error catching.

When `axCheckEnabled` is false, the secondary collector is skipped and
`axiomization_check` reports `skipped: true` so the Rust wrapper
accepts unconditionally. Default behavior: run both.

Patch C-K Fix 3 (audit MEDIUM): on secondary-collector crash, the
script now emits `axiomization_check { agreed: false, skipped: false,
error: <msg> }` plus a top-level `errors: [axiomization_check_crash:
<msg>]` so the Rust wrapper sees the crash as `internal_error` rather
than silently degrading to a trivial pass. The legitimate
operator-disabled-skip path still emits `skipped: true`. -/
private def runResolvedAndEmit (nodeName : String) (principalName : Option String)
    (rootName : Name) (env : Environment) (axCheckEnabled : Bool) : IO Unit := do
  try
    let ownData ← match moduleData? env (moduleForNode nodeName) with
      | some data => pure data
      | none => throw <| IO.userError s!"unsupported_artifact: no ModuleData for Tablet.{nodeName}"
    let visibilityLevel := if ownData.isModule then OLeanLevel.exported else OLeanLevel.private
    let visibilityEnv ← importModules (level := visibilityLevel) ownData.imports {}
    let mut providerIndex := buildProviderIndex visibilityEnv
    if ownData.constNames != ownData.constants.map (·.name) then
      providerIndex := { providerIndex with errors := providerIndex.errors.push s!"unsupported_toolchain: \
        Tablet.{nodeName}: ModuleData.constNames != constants.map ConstantInfo.name" }
    let mut ownSeen : Std.HashSet Name := {}
    for info in ownData.constants do
      if ownSeen.contains info.name then
        providerIndex := { providerIndex with errors := providerIndex.errors.push s!"unsupported_toolchain: \
          Tablet.{nodeName}: duplicate logical constant {info.name} in ownership manifest" }
      ownSeen := ownSeen.insert info.name
    for name in ownData.extraConstNames do
      let prior := providerIndex.extraNames[name]?.getD #[]
      let extras := providerIndex.extraNames.insert name (prior.push (moduleForNode nodeName))
      providerIndex := { providerIndex with extraNames := extras }
    let (finalSt, rootKind) ← runClosure env nodeName rootName providerIndex
        -- The skipped flag in this binding distinguishes legitimate
        -- operator opt-out (`skipped: true`, trivial pass) from a
        -- collector crash (`skipped: false` and an `error` field in
        -- the JSON; status flipped to internal_error by the wrapper).
    let (axCheckJson, crashMsg) ←
      if axCheckEnabled then do
            -- Run the secondary collector against the same loaded env
            -- (sharing the env-load cost). Catch errors so a bug in
            -- the secondary doesn't take out the primary's output;
            -- surface the error LOUDLY via the crash JSON shape AND a
            -- top-level `errors[]` entry the Rust wrapper detects via
            -- the `axiomization_check_crash:` prefix.
        try
          let axCheckSt ← runAxiomizationCheck env nodeName rootName providerIndex
          let ownershipManifest := ownData.constants.foldl
            (init := {}) (fun names info => names.insert info.name)
          pure (axiomizationCheckJson finalSt axCheckSt (skipped := false)
            (ownershipManifest := ownershipManifest), none)
        catch e =>
          let msg := e.toString
          pure (axiomizationCheckCrashJson msg, some msg)
      else
        pure (axiomizationCheckJson {} {} (skipped := true), none)
        -- Append the crash diagnostic to the visitor state's `errors`
        -- so `emitJson` surfaces it at the top level. The Rust parser's
        -- `axiomization_check_crash:` prefix detector keys off this.
    let finalSt :=
      match crashMsg with
      | some m =>
          { finalSt with errors := finalSt.errors.push s!"axiomization_check_crash: {m}" }
      | none => finalSt
        -- The status stays "ok" even when `errors` is non-empty: errors
        -- are observational data the Rust wrapper interprets per policy
        -- (plan §5.1 / §6.1). Transport-level failures (probe internal
        -- crash) raise via `catch` below and produce "internal_error".
    let certifiedPrincipal := if principalName.isSome then toString rootName else ""
    IO.println (emitJson env nodeName certifiedPrincipal "ok"
      rootKind.toString finalSt axCheckJson)
  catch e =>
    IO.println (emitFailureJson nodeName "internal_error"
      #[s!"traversal failed: {e.toString}"])

private def runAndEmit (nodeName principalName : String) (env : Environment)
    (axCheckEnabled : Bool) : IO Unit := do
  -- Resolve only the exact registered principal in the expected module. The
  -- resolved name then flows through the artifact closure walk and axcheck.
  match resolveRoot env nodeName principalName with
  | .missing =>
      IO.println (emitFailureJson nodeName "missing_declaration"
        #[s!"registered principal declaration {principalName} was not found \
            exactly in declaring module {moduleForNode nodeName}"])
  | .ambiguous candidates =>
      IO.println (emitFailureJson nodeName "ambiguous_declaration"
        #[s!"node {nodeName} has multiple candidate root declarations with final \
            component {nodeName} in module {moduleForNode nodeName}: \
            {candidates.toList}; refusing to pick one (resolve the namespace \
            ambiguity in the node source)"])
  | .resolved rootName =>
      runResolvedAndEmit nodeName (some principalName) rootName env axCheckEnabled

/-- A module owner has no ordinary FILESPEC principal. For a non-empty
manifest, choose one member only as the traversal's module-identity anchor;
the emitted principal remains empty and the closure still walks the complete
declaring-module manifest. For an empty manifest, emit an affirmative empty
certificate observation: the imported environment was enumerated and the
module-metadata declaration set is exactly empty. Replay set equality is
checked independently by the supervisor; import/enumeration failure never
reaches this accepting arm. -/
private def runModuleOwnerAndEmit (nodeName : String) (env : Environment)
    (axCheckEnabled : Bool) : IO Unit := do
  match (moduleDeclarationManifest env nodeName)[0]? with
  | none =>
      let axCheck := axiomizationCheckJson {} {} (skipped := !axCheckEnabled)
      IO.println (emitJson env nodeName "" "ok" "other" {} axCheck)
  | some (anchor, _) =>
      runResolvedAndEmit nodeName none anchor env axCheckEnabled

/-- Emit the source-policy operation envelope. Policy rejections use a stable
non-internal status; unreadable input or scanner failure remains an
`internal_error`. No closure traversal or axcheck runs in this mode. -/
private def emitScanOnlyJson (nodeName : String) (outcome : OwnerPolicyScanOutcome) : String :=
  let (status, errors) :=
    match outcome with
    | .ok                => ("ok", (#[] : Array String))
    | .rejected msg      => ("policy_rejection", #[msg])
    | .internalError msg => ("internal_error", #[msg])
  let axCheck := axiomizationCheckJson {} {} (skipped := true)
  let payload : Json := Json.mkObj [
    ("node",                  Json.str nodeName),
    ("status",                Json.str status),
    ("root_kind",             Json.str "other"),
    ("kernel_axioms",         Json.arr #[]),
    ("boundary_theorems",     Json.arr #[]),
    ("strict_theorem_deps",   Json.arr #[]),
    ("strict_definition_deps",Json.arr #[]),
    ("errors",                Json.arr (errors.map Json.str)),
    ("axiomization_check",    axCheck)
  ]
  payload.compress

/-- Run only the worker-authoring policy scan over the node's own source. -/
private def runScanOnly (nodeName : String) : IO Unit := do
  let outcome ← ownerFilePolicyScan nodeName
  IO.println (emitScanOnlyJson nodeName outcome)

/-- Parse the CLI args looking for `--no-axcheck` and `--scan-only`.
Returns the node name plus the `axCheckEnabled` and `scanOnly` flags.
Plan §4.6.1: `--no-axcheck` is an additive opt-out so the default remains
"run both collectors". `--scan-only` is an additive mode
selector so the default remains "run the full closure probe". -/
private def parseArgs (args : List String)
    : Option (String × Option String × Bool × Bool × Bool) :=
  match args with
  | [] => none
  | nodeName :: rest =>
      let hasNoAxcheck := rest.any (· == "--no-axcheck")
      let hasScanOnly := rest.any (· == "--scan-only")
      let hasModuleOwner := rest.any (· == "--module-owner")
      let principal := rest.findSome? fun arg =>
        if arg.startsWith "--principal=" then some (arg.drop "--principal=".length).toString
        else none
      some (nodeName, principal, !hasNoAxcheck, hasScanOnly, hasModuleOwner)

def main (args : List String) : IO UInt32 := do
  match parseArgs args with
  | none =>
      IO.eprintln "ERR\t<global>\tno node name provided"
      return 2
  | some (nodeName, principal, axCheckEnabledByArgs, scanOnly, moduleOwner) =>
      -- Both the policy scanner and the artifact probe import core modules.
      initSearchPath (← findSysroot)
      -- `--scan-only` is the cheap all-node-kind worker-policy operation.
      -- The full certificate probe never reads or parses node source.
      if scanOnly then
        runScanOnly nodeName
        return 0
      if moduleOwner && principal.isSome then
        IO.println (emitFailureJson nodeName "principal_registration_error"
          #["--module-owner takes no --principal"])
        return 0
      if !moduleOwner && principal.isNone then
        IO.println (emitFailureJson nodeName "principal_registration_error"
          #["full node-certificate probe requires an exact supervisor-registered --principal=<Lean.Name>"])
        return 0
      -- Env-var override per plan §4.6.1: setting
      -- TRELLIS_LOCAL_CLOSURE_AXCHECK_DISABLE=1 also disables the
      -- secondary collector. Either signal disables; both must
      -- consistently disable when set so operator can flip via env or
      -- CLI without restarting the kernel.
      let envDisable ← IO.getEnv "TRELLIS_LOCAL_CLOSURE_AXCHECK_DISABLE"
      let envSkip := match envDisable with
        | some v => v == "1" || v.toLower == "true"
        | none => false
      let axCheckEnabled := axCheckEnabledByArgs && !envSkip
      try
        let env ← importModules #[{ module := moduleForNode nodeName }] {}
        if moduleOwner then
          runModuleOwnerAndEmit nodeName env axCheckEnabled
        else
          -- Checked above: an ordinary owner always has an exact registered
          -- principal, while module-owner mode never fabricates one.
          runAndEmit nodeName principal.get! env axCheckEnabled
        return 0
      catch e =>
        IO.println (emitFailureJson nodeName "elaboration_error"
          #[s!"importModules failed: {e.toString}"])
        return 0

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

The memo is keyed by `(constant, mode)`, NOT by constant alone. The same
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
  "status": "ok" | "elaboration_error" | "missing_declaration" | "internal_error",
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

/-! ## FIX 2 — owner-file authored-name reserved-shape scan

`Name.isInternalDetail` is a textual name-shape test with zero provenance.
The closure walk uses it to transparent-walk *generated* artifacts
(`_sunfold`, `_proof_N`, equation lemmas, matchers) without recording them
as cross-node dependency keys. But a worker can AUTHOR a declaration whose
final component matches the same shapes — `protected theorem Owner.eq_1`,
`protected theorem Owner._helper`, `axiom Owner.eq_1 : …`, a `where`-bound
`eq_1`, or a `namespace Owner … theorem eq_1 … end`. Such a declaration is
hidden by the transparent-walk (it looks "generated") and, for several of
these shapes, also slips past the Rust first-token line-scanner because a
`protected`/`private` modifier or a namespace wrapper defeats it. The
authored auxiliary then crosses a node boundary with no registered NodeId,
no review, and no invalidation hook.

The distinguishing fact is **authorship**: genuine internals are
compiler-*generated* and never appear as a worker's declaration command;
the forge is always *authored*. We therefore parse the node's OWN source
through the Lean frontend parser, collect every AUTHORED declaration's
final name component, and reject the node if any matches the reserved
shapes. This runs on the node's own source at the node's own acceptance,
so every accepted owner is clean and consumers are transitively safe.

The check is on the FINAL component only: `protected` / `private` /
`namespace` / `export` change a declaration's *prefix*, never the final
written component, so no name-resolution / elaboration is needed — a pure
syntax-tree walk over `declId`s (and `where` / `let rec` binders) is
authoritative for this check. Genuine generated internals are never source
declaration commands, so a source-parse walk never sees them: no false
positives. -/

/-- Reserved name SHAPES on a single string component, mirroring exactly
the `.str` cases of `Lean.Name.isInternalDetail` (`Lean/Data/Name.lean`):
a leading `_`, or `eq_`/`match_`/`proof_`/`omega_` followed only by
digits/`_`. We deliberately operate on the FINAL component string, NOT on
the full `Name`: `isInternalDetail`'s ancestor (`p.isInternalOrNum`) and
`.num` cases describe generated/non-authorable shapes that do not apply to
an authored final component. `matchPrefix` reuses Lean's own semantics. -/
private def isReservedShapedComponent (s : String) : Bool :=
  s.startsWith "_"
    || matchPrefix s "eq_"
    || matchPrefix s "match_"
    || matchPrefix s "proof_"
    || matchPrefix s "omega_"
where
  /-- A string begins with `pre`, then is only digits/`_`. Verbatim copy of
  `Lean.Name.isInternalDetail.matchPrefix`. -/
  matchPrefix (s : String) (pre : String) : Bool :=
    s.startsWith pre && (s |>.drop pre.length |>.all fun c => c.isDigit || c == '_')

/-- The final (last) component of a dotted `Name`, as a string. Returns
`none` for `.anonymous` or a `.num`-tailed name (a `.num` final component
is not authorable as a source identifier, so it is out of scope). -/
private def finalComponent? : Name → Option String
  | .str _ s => some s
  | _        => none

/-- FILESPEC name-parity test (single source of truth, used by both the root
resolver and the dep-name canonicalizer). A node's on-disk file stem is the
FILESPEC-sanitized form of its PRINCIPAL declaration's name BELOW its
`namespace_context`: the trailing run of decl-name components with dots
collapsed to underscores (a `.` is illegal in a FILESPEC stem). This is the
inverse of the kernel's `challenge_conformance_errors` rule
(`spec.name.replace('.', "_")`).

`declMatchesStem name stem` is true iff SOME trailing component run of `name`,
joined by `_`, equals `stem`. It is namespace-depth agnostic:
  * a top-level math decl `Foo` matches stem `Foo` (length-1 run) — identical
    to the legacy bare-name check;
  * a top-level PV decl `crate.FIELD_MODULUS` matches stem `FIELD_MODULUS`;
  * a deeply-namespaced decl `crate.BiasedFp.Insts.X.eq` matches the flattened
    stem `BiasedFp_Insts_X_eq` via its full below-namespace run;
  * a cross-node authored auxiliary `crate.Owner.realAux` does NOT match a
    different node's stem `OwnerAux` (`realAux`, `Owner_realAux`, … all
    differ), so it is never collapsed onto a present-node id — the
    soundness-load-bearing rejection. -/
private def declMatchesStem (name : Name) (stem : String) : Bool :=
  let comps := name.components.map (·.toString)
  (List.range comps.length).any fun drop =>
    String.intercalate "_" (comps.drop drop) == stem

/-- Recursively collect every AUTHORED declaration-introducing name from a
parsed command's syntax tree. Two sources:

* `Lean.Parser.Command.declId` nodes — the identifier of every top-level
  / namespaced declaration command (`theorem`/`def`/`abbrev`/`opaque`/
  `axiom`/`instance`/`structure`/`inductive`/`class`/…). `declId` is
  `ident >> optional (".{…}")`, so the written name is the node's id when
  it is itself an ident, else its first child (mirrors
  `Lean.Elab.Command.expandDeclIdCore`). We take its FINAL component.
* `Lean.Parser.Term.letId` nodes that sit inside a
  `Lean.Parser.Term.letRecDecl` ancestor — i.e. `where`-bound and
  `let rec`-bound auxiliaries, which Lean lifts to real constants
  (`Owner.eq_1` for `def Owner … where eq_1 := …`). A plain `let x := …`
  inside a proof/term body is a `letDecl` NOT under a `letRecDecl`, creates
  no constant, and is correctly skipped (no false positive). The lifted
  name's final component is the bare binder, which is what we check.

We collect the bare final-component string for each. The
`inLetRec` flag tracks whether we are under a `letRecDecl`. -/
private partial def collectAuthoredFinalComponents
    (stx : Syntax) (inLetRec : Bool) (acc : Array String) : Array String :=
  let kind := stx.getKind
  let nowInLetRec := inLetRec || kind == ``Lean.Parser.Term.letRecDecl
  let acc :=
    if kind == ``Lean.Parser.Command.declId then
      let id : Name := if stx.isIdent then stx.getId else stx[0].getId
      match finalComponent? id with
      | some s => acc.push s
      | none   => acc
    else if kind == ``Lean.Parser.Term.letId && nowInLetRec then
      -- `letId` = `ident >> many letIdBinder`; first child is the binder ident.
      let id : Name := stx[0].getId
      match finalComponent? id with
      | some s => acc.push s
      | none   => acc
    else acc
  stx.getArgs.foldl (fun a c => collectAuthoredFinalComponents c nowInLetRec a) acc

/-! ## MACRO BAN — forbid command-macro / syntax / elaborator-defining commands

The reserved-shaped authored-name scan above catches a declaration whose
written final component is reserved-shaped (`Owner.eq_1`, `Owner._helper`).
But a node could instead author a *command macro* / *elaborator* that, when
its own invocation is elaborated, EMITS a reserved-shaped top-level
declaration whose name never appears literally in source — e.g.

```
macro "declare_eq1" : command => `(theorem Owner.eq_1 : False := …)
declare_eq1
```

`Owner.eq_1` is synthesized at elaboration time, so the syntactic
authored-name scan never sees the string `eq_1` as a `declId`, and the
closure probe then transparent-walks the synthesized constant as an
"internal detail" — the exact review-integrity bypass FIX 2 closes for
the directly-authored case. The synthesizer is always one of the
command-macro / syntax / elaborator-DEFINING commands; an ordinary Tablet
node has no legitimate need for any of them (verified: NO node in the live
designs or mapper Tablets uses any such command, so the ban has zero
compatibility cost). We therefore forbid the whole family in node files.

`notation` / `infix` / `infixl` / `infixr` / `prefix` / `postfix` (kinds
`Lean.Parser.Command.notation` and `Lean.Parser.Command.mixfix`) are
deliberately NOT banned: they introduce term-level notation whose RHS is a
`termParser`, so they cannot expand to a top-level declaration and cannot
synthesize a reserved-shaped constant. A worker may legitimately want
operator notation, so they remain allowed. -/

/-- The command syntax kinds that DEFINE a command macro, term/command
syntax, or elaborator — i.e. the mechanisms that can synthesize a top-level
declaration whose name is absent from source. Verified against
v4.30.0-rc1 by parsing each command and reading `Syntax.getKind` (the
parser `def`s in `Lean/Parser/Syntax.lean` carry these node kinds via
`leading_parser`'s `decl_name%`). `notation`/`mixfix` are intentionally
absent (term-level only; see the section comment). -/
private def bannedCommandKinds : Array Name := #[
  ``Lean.Parser.Command.macro,
  ``Lean.Parser.Command.macro_rules,
  ``Lean.Parser.Command.elab,
  ``Lean.Parser.Command.elab_rules,
  ``Lean.Parser.Command.syntax,
  ``Lean.Parser.Command.syntaxAbbrev,
  ``Lean.Parser.Command.syntaxCat,        -- `declare_syntax_cat`
  ``Lean.Parser.Command.binderPredicate
]

/-- A human-facing keyword for a banned command kind, for the diagnostic. -/
private def bannedCommandKeyword (k : Name) : String :=
  if k == ``Lean.Parser.Command.macro then "macro"
  else if k == ``Lean.Parser.Command.macro_rules then "macro_rules"
  else if k == ``Lean.Parser.Command.elab then "elab"
  else if k == ``Lean.Parser.Command.elab_rules then "elab_rules"
  else if k == ``Lean.Parser.Command.syntax then "syntax"
  else if k == ``Lean.Parser.Command.syntaxAbbrev then "syntax (abbrev)"
  else if k == ``Lean.Parser.Command.syntaxCat then "declare_syntax_cat"
  else if k == ``Lean.Parser.Command.binderPredicate then "binder_predicate"
  else k.toString

/-- Recursively collect the keyword of every banned command kind that
appears anywhere in a parsed command's syntax tree. The walk is RECURSIVE,
not top-level-only: a banned command may be wrapped by a `… in` combinator
(`set_option x in macro …` has top kind `Lean.Parser.Command.in` with the
`macro` nested inside) or by leading attributes, so a top-level-kind check
would miss those forms. The walk is also false-positive-free: an ordinary
declaration's type/body uses `Term`-category quotation kinds, never these
`Command.*` kinds — a `Command.macro`/`Command.elab`/… node appears in a
parsed tree only when the worker actually wrote such a command. -/
private partial def collectBannedCommandKeywords
    (stx : Syntax) (acc : Array String) : Array String :=
  let acc :=
    if bannedCommandKinds.contains stx.getKind then
      acc.push (bannedCommandKeyword stx.getKind)
    else acc
  stx.getArgs.foldl (fun a c => collectBannedCommandKeywords c a) acc

/-- Parse `source` as a Lean module and return every reserved-shaped
authored declaration final-component (deduplicated, sorted). Uses the
frontend parser (`Parser.parseHeader` + `Parser.parseCommand`) against an
`Init`-only environment — no elaboration, no olean loads, so it is cheap
and cannot itself pull in unreviewed content. Parse errors are ignored for
THIS check (the closure walk / `lake build` already gate on a parseable,
elaborating file); we only harvest declaration ids from whatever commands
did parse.

Soundness of `Init`-only despite ignored parse errors: a forged auxiliary
is, by construction, a top-level declaration command whose head is a
builtin keyword (`theorem`/`def`/`abbrev`/`opaque`/`axiom`/`instance`/
`structure`/`inductive`/`class`, with optional `protected`/`private`/
`namespace`), ALL of which live in the `Init` token table. `parseCommand`
(`Lean/Parser/Module.lean`) cannot desync permanently on a preceding
unparseable command: on error it consumes at least one token and the
command-category parser resynchronizes on the next leading command keyword
— i.e. the forge's head. And each declaration parser harvests its name via
`recover declId …` (`Lean/Parser/Command.lean`), so the `declId` is
captured even when the declaration's own type/body uses imported notation
or macros that `Init` cannot parse. So a forge cannot hide behind earlier
imported-notation / custom-command commands. Empirically verified and
locked by the `scan_only_forge_after_{imported_notation,custom_command}`
smoke tests in `kernel/tests/local_closure_smoke.rs`; widening to a
full-import parse is
unnecessary and would add a per-node olean-load cost on the hot path.

The same single parse pass also harvests the MACRO-BAN banned-command
keywords (`collectBannedCommandKeywords`): the macro/syntax/elaborator
commands live in the `Init` token table too, so they parse to their proper
`Command.*` kinds even under the `Init`-only environment. -/
private structure OwnerScanResult where
  /-- Reserved-shaped authored declaration final-components (FIX 2). -/
  reservedNames   : Array String := #[]
  /-- Keywords of banned macro/syntax/elaborator-defining commands (MACRO BAN). -/
  bannedCommands  : Array String := #[]
  /-- `some path` when the node source could not be read (fail closed). -/
  unreadable      : Option String := none
  deriving Inhabited

private def reservedShapedAuthoredNames (source : String) (fileName : String)
    : IO OwnerScanResult := do
  let inputCtx := Parser.mkInputContext source fileName
  -- A minimal environment is enough: the module parser only needs the
  -- command grammar's token table, which `Init` provides.
  let env ← importModules #[{ module := `Init }] {}
  let (_, parserState, messages) ← Parser.parseHeader inputCtx
  let pmctx : Parser.ParserModuleContext := { env := env, options := {} }
  let mut state := parserState
  let mut msgs := messages
  let mut names : Array String := #[]
  let mut banned : Array String := #[]
  repeat
    let (cmd, state', msgs') := Parser.parseCommand inputCtx pmctx state msgs
    state := state'
    msgs := msgs'
    names := collectAuthoredFinalComponents cmd false names
    banned := collectBannedCommandKeywords cmd banned
    if Parser.isTerminalCommand cmd then break
  let offenders := names.filter isReservedShapedComponent
  -- Deduplicate + sort for a deterministic diagnostic.
  let dedup (xs : Array String) : Array String :=
    (xs.foldl (init := (#[] : Array String)) fun acc s =>
      if acc.contains s then acc else acc.push s).qsort (· < ·)
  return { reservedNames := dedup offenders, bannedCommands := dedup banned }

/-- Read a node's own source file (`Tablet/<Node>.lean`, relative to the
probe's cwd = repo root) and return its `OwnerScanResult` (reserved-shaped
authored names + banned macro/elab/syntax commands). The path mirrors
`moduleForNode`'s module→path convention. On a missing / unreadable source
we fail CLOSED by setting `unreadable := some path` so the caller flips
status to `internal_error` — the invariant cannot be verified, so the safe
direction is rejection. -/
private def scanOwnerSourceFile (nodeName : String) : IO OwnerScanResult := do
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
    reservedShapedAuthoredNames source path.toString
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

/-- True iff `name`'s declaring module is in the `Tablet.*` namespace.
Mirrors `lean_semantic_fingerprint.lean:267`'s `isTabletConst`, including
the fail-closed fallback for the seed constant (no module idx ⇒ treat
as Tablet, since the seed is always one of our nodes).

When `getModuleFor?` (i.e. `getModuleIdxFor?`) yields an ambiguous result
(no module → seed-or-local; treat as Tablet), we err on the side of
classifying as Tablet so the boundary-theorem / strict-dep accumulators
get the entry. The Rust wrapper does the NodeId normalization later
and fails closed if the entry is unmappable (plan §4.5). -/
private def isTabletConst (env : Environment) (name : Name) : Bool :=
  match env.getModuleIdxFor? name with
  | some idx =>
    match env.allImportedModuleNames[idx]? with
    | some modName =>
      match modName with
      | .str (.str .anonymous "Tablet") _ => true
      | _ => false
    | none => true   -- index out of range, fail-closed to Tablet
  | none => true     -- no module idx, fail-closed to Tablet

/-- True for Lean's constructor injection theorems `Ctor.inj` / `Ctor.injEq`.

Lean generates these under the constructor namespace, for example
`InputDenotation.finite.injEq` and `InputDenotation.finite.inj`. They are not
authored Tablet nodes and have no kernel lifecycle hook, so they must be
transparent-walked. This is a provenance check, not a suffix-only filter: the
immediate parent must be an environment-recognized constructor. -/
private def isCtorInjectionTheorem (env : Environment) : Name → Bool
  | .str parent "inj" => env.isConstructor parent
  | .str parent "injEq" => env.isConstructor parent
  | _ => false

/-- True for Lean's constructor `sizeOf` specification theorems
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
private def isCtorSizeOfSpecTheorem (env : Environment) (name : Name) : Bool :=
  match name with
  | .str parent "sizeOf_spec" =>
      env.isConstructor parent
      && (match env.getModuleIdxFor? name, env.getModuleIdxFor? parent with
          | some ni, some pi => ni == pi
          | _, _ => false)
  | _ => false

/-- Fix B: final-name-component suffixes of the compiler-generated
recursor/eliminator family attached to an inductive's namespace. Grounded in
the toolchain source (`Lean.AuxRecursor` suffix constants;
`Lean.Elab.MutualInductive.mkAuxConstructions`; `Lean.Meta.IndPredBelow`).
`IndPredBelow` generates `below` (an `inductDecl`) and `brecOn` (a `thmDecl`)
for recursive inductive PREDICATES without ever calling `markAuxRecursor`,
so `Lean.isAuxRecursor` is false for them in EVERY environment. -/
private def recursorFamilySuffixes : List String :=
  ["casesOn", "recOn", "brecOn", "below"]

/-- True iff `s` is a recursor-family suffix, or a numbered variant
`<suffix>_<digits>` (the `Name.appendIndexAfter` shape Lean uses for the
nested/mutual-inductive copies, e.g. `below_1`, `brecOn_2`). Digits-only
after the underscore: `below_lemma` does NOT match (stays fail-closed). -/
private def matchesRecursorFamilySuffix (s : String) : Bool :=
  recursorFamilySuffixes.any fun suffix =>
    s == suffix
    || (s.length > suffix.length + 1
        && s.startsWith (suffix ++ "_")
        && (s.drop (suffix.length + 1)).all Char.isDigit)

/-- Fix B: extension-independent recognition of an inductive's generated
recursor-family members: the final component is a recursor-family suffix AND
the immediate parent name resolves in the env to an INDUCTIVE AND the member
is declared in the SAME module as that inductive (generation is eager, in
the inductive's own module). Provenance-based like `isCtorInjectionTheorem`:
a same-suffix name under a non-inductive parent, or declared in a different
module than its parent inductive, is NOT recognized (fail-closed: it stays a
recorded dep key). -/
private def isRecursorFamilyRealization (env : Environment) (name : Name) : Bool :=
  match name with
  | .str parent s =>
      matchesRecursorFamilySuffix s
      && (match env.find? parent with
          | some (.inductInfo _) => true
          | _ => false)
      && (match env.getModuleIdxFor? name, env.getModuleIdxFor? parent with
          | some ni, some pi => ni == pi
          | _, _ => false)
  | _ => false

/-- True for Tablet-local Lean artifacts that are generated by elaboration
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
private def isTabletGeneratedArtifact (env : Environment) (name : Name) : Bool :=
  name.isInternalDetail
  || Lean.isReservedName env name
  || (env.getProjectionFnInfo? name).isSome
  || (env.getAuxParentProjectionInfo? name).isSome
  || env.isConstructor name
  || isCtorInjectionTheorem env name
  || isCtorSizeOfSpecTheorem env name
  || Lean.isAuxRecursor env name
  || isRecursorFamilyRealization env name
  || Lean.isNoConfusion env name

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

`resolveRoot` is a FALSE-NEGATIVE-only fix. It can make a genuinely-closed
namespaced node resolve, but never admits a wrong / internal-detail /
ambiguous declaration:

* FAST PATH / LEGACY: if `env.find? (nameFromString nodeName)` is `some`,
  return it verbatim — preserves top-level math nodes bit-for-bit.
* NAMESPACED PATH: otherwise scan the environment for declarations that pass
  the FILESPEC name-parity test against `nodeName` (`declMatchesStem`: some
  trailing component run, `.`→`_`, equals the node stem — so both
  `crate.<bare>` and a flattened nested decl `crate.Nested.method` in module
  `Tablet.Nested_method` resolve) AND that are declared in the node's OWN
  module `Tablet.<nodeName>` (reusing the `getModuleIdxFor?` /
  `allImportedModuleNames` module-membership machinery, restricted to the
  node's module so a transitively-imported matching declaration cannot be
  picked) AND that are NOT internal-detail/generated artifacts (reusing
  `isTabletGeneratedArtifact`).
    - exactly ONE such match ⇒ resolve to it;
    - ZERO ⇒ `none` ⇒ `missing_declaration` (unchanged behaviour);
    - MORE THAN ONE ⇒ explicit AMBIGUITY rejection (a distinct status, never
      an arbitrary pick) — preserving the safe over-reject direction. -/

/-- The module `Name` a constant is declared in, or `none` when the
environment has no module index for it (a seed / locally-elaborated decl).
Reuses the exact `getModuleIdxFor?` + `allImportedModuleNames` lookup that
`isTabletConst` uses. -/
private def declaringModule? (env : Environment) (name : Name) : Option Name :=
  match env.getModuleIdxFor? name with
  | some idx => env.allImportedModuleNames[idx]?
  | none     => none

/-- True iff `c` is an auxiliary of the ACTIVE node itself: declared in the
active root's own module AND strictly namespaced under the active root name.
Never true for another node's decl or a genuine cross-node reference.

Fix A (own-node `let rec` aux): a user-named `let rec` binder inside the
active node's own proof is lifted by the elaborator to a real constant
`<activeRoot>.<binderName>` in the node's OWN module. It is an artifact of
the node's own elaboration (design intent: the §4.5 transparent-walk
filter), but `isTabletGeneratedArtifact` is name-shape-based and cannot see
it, so it leaked into the dep records as a dotted boundary key that the
kernel's Patch C-K present-node validation fail-closes on. This predicate
is provenance-based: same declaring module as the active root (each node
compiles to its own module, so no other node's decl can satisfy it) AND
strictly below the active root's namespace. When either module index is
unavailable it returns `false` (fail-closed: the decl stays a recorded
dep). Used by BOTH collectors — the gates must stay mirrored or the dual
collectors disagree ⇒ `internal_error`. -/
private def isOwnNodeAux (env : Environment) (active c : Name) : Bool :=
  active != c
  && active.isPrefixOf c
  && (match declaringModule? env active, declaringModule? env c with
      | some am, some cm => am == cm
      | _, _             => false)

/-! ## Tablet dep-name → NODE ID canonicalization (namespaced-dep fix)

The closure walk accumulates cross-node dependency keys as the dep's full
Lean `Name`. For a math node that name IS the node id (the decl is
top-level, so `toString name == nodeName`). But a PV (Aeneas-extracted) dep
is NAMESPACED — its decl is `<crate>.<depNode>` (e.g.
`ntt_montgomery.FIELD_MODULUS`) while its module is still `Tablet.<depNode>`.
Emitting the full namespaced `Name` then fails the kernel's Patch C-K
present-node validation, which keys `present_nodes` by BARE node ids
(`FIELD_MODULUS`): a namespaced name has no `Tablet.` prefix for the Rust
parser to strip, so it stays `ntt_montgomery.FIELD_MODULUS` and is rejected
as unmappable — blocking `formalization_complete` for every PV node with a
Tablet dependency.

A Tablet dep's node id is its DECLARING MODULE minus the leading `Tablet.`
component (module `Tablet.FIELD_MODULUS` ⇒ node id `FIELD_MODULUS`),
INDEPENDENT of the decl's namespace. `tabletNodeId?` derives exactly that.
For a math/top-level dep the module suffix equals the bare decl name equals
the node id, so the emitted string is byte-identical to the legacy
`toString name` — math runs are unaffected. Only the `name` (node-id) field
is rewritten; the hash field is untouched (hashes identify content, the
name identifies the node). -/

/-- The NODE ID of a Tablet constant THAT IS ITS NODE'S PRINCIPAL
DECLARATION: its declaring module `Name` with the leading `Tablet.`
component stripped, rendered as the node-id string. `none` (caller falls
back to the legacy `toString name`) when:

* the constant has no module index (a seed / locally-elaborated decl), or
  its module is not under `Tablet.`; or
* the decl is NOT the node's principal declaration — detected by the
  FILESPEC name-parity test below.

The principal-declaration test is load-bearing for SOUNDNESS. A node's
on-disk file stem is the FILESPEC-sanitized form of the principal decl's
name BELOW its `namespace_context`: the trailing run of decl-name
components, with dots collapsed to underscores (a dot is illegal in a
FILESPEC stem). Concretely, the principal decl of node
`BiasedFp_Insts_CoreCmpPartialEqBiasedFp_eq` is
`dec2flt_full_integer.BiasedFp.Insts.CoreCmpPartialEqBiasedFp.eq`: stripping
the `dec2flt_full_integer` namespace-context prefix leaves
`BiasedFp.Insts.CoreCmpPartialEqBiasedFp.eq`, whose `.`→`_` sanitization is
exactly the module stem. So the test is: SOME suffix of the decl-name
components, joined by `_`, equals the module stem. This mirrors the kernel's
`challenge_conformance_errors` name-parity rule (`spec.name.replace('.',
"_")`) — the same FILESPEC stem relation, inverted.

This is namespace-depth agnostic. A top-level math dep (`Tablet.Foo`, decl
`Foo`) passes via the length-1 suffix `Foo`, byte-identical to the legacy
bare node id. A top-level PV def (`Tablet.FIELD_MODULUS`, decl
`crate.FIELD_MODULUS`) passes via the length-1 suffix `FIELD_MODULUS`. A
deeply-namespaced instance method passes via its full below-namespace
suffix. But a hand-authored private auxiliary `Owner.realAux` (a `thmInfo`,
NOT a generated member) declared in a DIFFERENT node's module
`Tablet.OwnerAux` has NO component suffix sanitizing to the stem `OwnerAux`
(`realAux`, `Owner_realAux`, … all differ), so it stays a dotted `Name`,
which the kernel's Patch C-K private-auxiliary guard correctly rejects. Only
principal declarations map; we never collapse an auxiliary to a node id. -/
private def tabletNodeId? (env : Environment) (name : Name) : Option String :=
  match (declaringModule? env name).map (·.components.map (·.toString)) with
  | some ("Tablet" :: rest@(_ :: _)) =>
      -- `rest` is the module's node-id components — for a FILESPEC stem this
      -- is a single sanitized component (`["BiasedFp_…_eq"]`); for a legacy
      -- multi-segment math module it may be several (`["A","B"]`). The node
      -- id is the module suffix rendered as a dotted Name.
      let nodeId := toString (rest.foldl Name.str Name.anonymous)
      -- PRINCIPAL-DECLARATION TEST (`declMatchesStem`): the decl name must
      -- have a trailing component run whose `.`→`_` sanitization equals the
      -- module stem. Map iff so, else leave the dotted `Name` for the kernel
      -- to reject (fail-closed). `nodeId.replace "." "_"` is the FILESPEC stem
      -- (a no-op for the single-component PV/math stems; folds the dead
      -- legacy multi-segment `A.B` module shape into `A_B`).
      let stem := nodeId.replace "." "_"
      if declMatchesStem name stem then some nodeId else none
  | _ => none

/-- Canonicalize a Tablet cross-node dependency's `Name` to its NODE ID for
emission. For a Tablet const whose declaring module resolves under `Tablet.`
we emit the node id (`tabletNodeId?`); otherwise (no module index / not a
Tablet module — e.g. the fail-closed-to-Tablet seed branch) we preserve the
legacy `toString name` so behavior there is unchanged. This is the mapping
applied at the THEOREM-dep emission sites (`boundary_theorems` /
`strict_theorem_deps`), where the principal-only `declMatchesStem` gate is a
deliberate soundness guard: a theorem dep on another node's *private
auxiliary* (a hand-authored `thmInfo`, NOT a node's principal declaration)
must stay a dotted `Name` so the kernel's Patch C-K private-auxiliary guard
rejects it (the "2145 incident"). The definition-dep sites use
`depDefKeyName` instead — see below. -/
private def depKeyName (env : Environment) (name : Name) : String :=
  (tabletNodeId? env name).getD (toString name)

/-! ## Tablet DEFINITION-dep name → NODE ID canonicalization

`strict_definition_deps` records `def` / `abbrev` / `inductive`
DEFINITION dependencies, and must map a dep's `Name` to its declaring-module
node id WITHOUT the principal-only `declMatchesStem` gate that `tabletNodeId?`
applies. Two categories of legitimate definition dep are non-principal and so
fall through `tabletNodeId?` to the raw dotted `Name` — which the kernel's
Patch C-K present-node validator then fail-closes:

* Preamble-shared structures (`BiasedFp`, `DecimalSeq`, `Number`) — a
  `structure` declared in module `Tablet.Preamble` (node id `Preamble`, a
  present node) but whose decl name is `<crate>.BiasedFp`, which does not
  sanitize to the stem `Preamble`.
* Aeneas-generated loop helpers (`<crate>.left_shift_loop0`,
  `…_loop0.body`, `parse_decimal_seq_loop0`) — extra defs Aeneas co-emits
  inside a model node's OWN module (e.g. `Tablet.left_shift`; node id
  `left_shift`, a present node), non-principal, and not name-shaped as a
  generated artifact (so `isTabletGeneratedArtifact` does not transparent-walk
  them).

The correct closure attribution for a DEFINITION dep is unconditional: a
definition dependency on ANY declaration living in module `Tablet.X` means
node `X` must be present and Lean-closed for the consumer to close —
regardless of whether the referenced decl is `X`'s principal declaration or a
co-generated helper. So the def-dep mapping uses the bare declaring-module
node id with no name-parity gate. This is sound BECAUSE it only ever maps onto
the declaring module's OWN node id (never onto a different node), and the
referenced decl genuinely lives in that present node's olean.

The theorem-dep gate is intentionally NOT relaxed: there, mapping a private
auxiliary onto its host node id would let a worker ride on another theorem
node's unreviewed private lemma. -/

/-- The NODE ID of any Tablet constant by its DECLARING MODULE alone: the
module `Name` under `Tablet.` with the leading `Tablet.` component stripped,
rendered as the node-id string. `none` when the constant has no module index
(seed / locally-elaborated decl) or its module is not under `Tablet.`. This
is `tabletNodeId?` WITHOUT the principal-declaration (`declMatchesStem`) gate —
used for definition deps only (see the section comment). -/
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

/-- Outcome of resolving a node's root declaration. -/
private inductive RootResolution where
  /-- A unique resolved root `Name` (fast-path or namespaced unique match). -/
  | resolved (name : Name)
  /-- No declaration found ⇒ `missing_declaration` (unchanged). -/
  | missing
  /-- More than one same-final-name non-generated declaration in the node's
  own module ⇒ explicit ambiguity rejection (never an arbitrary pick). The
  array carries the candidates for the diagnostic. -/
  | ambiguous (candidates : Array Name)

/-- Resolve the on-disk root declaration for `nodeName` against `env`. See the
section comment above for the fast-path / namespaced-path / ambiguity policy. -/
private def resolveRoot (env : Environment) (nodeName : String) : RootResolution :=
  let bare := nameFromString nodeName
  -- FAST PATH / LEGACY: a top-level (math) node resolves verbatim.
  if (env.find? bare).isSome then
    .resolved bare
  else
    -- NAMESPACED PATH: collect declarations that pass the FILESPEC name-parity
    -- test (`declMatchesStem`) against the node stem, that live in the node's
    -- OWN module `Tablet.<nodeName>`, and that are not generated/internal-detail
    -- artifacts.
    let ownModule := moduleForNode nodeName
    -- A namespaced root's decl may be `<crate>.<bare>` (final == nodeName) OR,
    -- for a node whose FILESPEC stem flattens a nested below-namespace path
    -- (`crate.Nested.method` in module `Tablet.Nested_method`), a decl whose
    -- final component is the LAST below-namespace segment, NOT the stem.
    -- `declMatchesStem` (the FILESPEC name-parity test, shared with the
    -- dep-name canonicalizer) recognizes BOTH: it matches iff some trailing
    -- component run, `.`→`_`, equals the node stem. The own-module +
    -- non-generated guards still bound the scan, so this stays a
    -- false-negative-only resolver (never an arbitrary or wrong pick).
    let candidates : Array Name :=
      env.constants.fold (init := #[]) fun acc name _ =>
        if declMatchesStem name nodeName
            && declaringModule? env name == some ownModule
            && !isTabletGeneratedArtifact env name then
          acc.push name
        else
          acc
    match candidates with
    | #[]       => .missing
    | #[only]   => .resolved only
    | many      => .ambiguous many

/-! ## Side-effect record + visitor state -/

/-- The set of side effects produced by visiting a single `(c, mode)`
pair. On a memo hit, the record is replayed into the current accumulator.
This is the per-plan-§4.4 alternative to a single-bit `seen` set, which
is **forbidden** because the same constant visited under different modes
produces different side effects. -/
private structure SideEffectRecord where
  axioms              : Array Name              := #[]
  boundaryTheorems    : Array (Name × String)   := #[]
  strictTheoremDeps   : Array (Name × String)   := #[]
  strictDefinitionDeps: Array (Name × String)   := #[]
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
  /-- Errors raised during traversal (e.g. unsafe declarations,
  partial blocks, missing decls below the root). Surface to the JSON
  `errors` array; do NOT abort. -/
  errors               : Array String                          := #[]
  /-- The active root constant (cycle-guard target). -/
  active               : Name                                  := .anonymous
  /-- Per-`(c, mode)` memo of side-effect records. Replayed on hit. -/
  memo                 : Std.HashMap (Name × VisitMode) SideEffectRecord := {}

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
    let errors := s.errors ++ rec.errors
    { s with kernelAxioms, boundaryTheorems, strictTheoremDeps,
             strictDefinitionDeps, errors }

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
active root. Memoized on `(c, mode)` with side-effect record replay. -/
private partial def visitConst (c : Name) (mode : VisitMode) : VisitorM Unit := do
  -- Cycle guard (plan §4.5).
  if c == (← get).active then return

  -- Memo hit: replay the cached record into the current accumulator.
  if let some record := (← get).memo[(c, mode)]? then
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
  modify fun s => { s with memo := s.memo.insert (c, mode) {} }

  match env.find? c with
  | none =>
      -- Decl referenced but absent from env. Surface as an error and stop.
      let msg := s!"missing constant during traversal: {c}"
      record := recordError record msg
      modify fun s => { s with errors := s.errors.push msg }
  | some info =>
      let active := (← get).active
      if isTabletConst env c && (isTabletGeneratedArtifact env c || isOwnNodeAux env active c) then
        -- §4.5 transparent walk: auto-generated artifact (e.g.
        -- `Foo._proof_1_1`, `Foo._sunfold`, `Foo.eq_1`, and reserved
        -- realization names such as `Foo.congr_simp` / `Foo.eq_def`), OR
        -- (Fix A) an auxiliary of the ACTIVE node itself (e.g. a
        -- user-named `let rec` binder lifted to `<activeRoot>.<binder>`
        -- in the node's own module — `isOwnNodeAux`). Both are artifacts
        -- of the node's own elaboration, not cross-node references.
        -- MIRRORED in the secondary (axcheck) collector's gate below —
        -- the two gates must stay identical or the dual collectors
        -- disagree and the wrapper flips status to `internal_error`.
        -- Skip recording the artifact as a dep entry (it's not a real
        -- cross-node reference), but DO walk its type and value so any real
        -- Tablet refs nested inside are still discovered. Mode preserved on
        -- the value walk to honor proof-body boundary cuts. Mirrors the
        -- axiomization-check side's filter below.
        --
        -- Memoization note: the artifact branch records nothing
        -- *directly* (no boundary / strict-dep entry), but the recursive
        -- `visitConst` calls below mutate `s.kernelAxioms` /
        -- `s.boundaryTheorems` / etc. through their own per-`(c,mode)`
        -- memo replay. The empty `record` we end up storing for this
        -- artifact is correct: revisits of the same `(c, mode)` pair are
        -- short-circuited by the sentinel/memo, and the nested visits
        -- they trigger have already populated the live accumulators.
        match info with
        | .thmInfo v | .defnInfo v | .opaqueInfo v =>
            for child in v.type.getUsedConstants do
              visitConst child .strict
            for child in v.value.getUsedConstants do
              visitConst child mode   -- preserve caller's mode for proof-body cuts
        | .inductInfo v =>
            for child in v.type.getUsedConstants do
              visitConst child .strict
            for ctor in v.ctors do
              match env.find? ctor with
              | some (.ctorInfo cv) =>
                  for child in cv.type.getUsedConstants do
                    visitConst child .strict
              | _ => pure ()
        | .ctorInfo v | .recInfo v =>
            for child in v.type.getUsedConstants do
              visitConst child .strict
        | .axiomInfo _ =>
            -- FIX 1 (axiom sub-case): an axiom reached through the
            -- transparent-walk branch must be RECORDED, never dropped.
            -- A reserved-shaped Tablet axiom (e.g. `axiom Owner.eq_1 :
            -- False`) would otherwise vanish from `kernel_axioms`,
            -- letting a consumer derive `False` with no axiom surfaced.
            -- Genuine compiler-generated artifacts are essentially never
            -- axioms, so recording here carries no false-positive cost;
            -- surfacing any axiom is the safe fail-closed behavior.
            -- Mirrors the normal Tablet `.axiomInfo` arm below (record +
            -- walk the type so transitively-used deps in the axiom's type
            -- are captured).
            record := recordAxiom record c
            modify fun s => { s with kernelAxioms := s.kernelAxioms.insert c }
            for child in info.type.getUsedConstants do
              visitConst child .strict
        | .quotInfo _ =>
            pure ()
      else if !isTabletConst env c then
        -- Non-Tablet boundary: hand to Lean.collectAxioms. The transitive
        -- axioms get merged into kernel_axioms. We do NOT recurse into
        -- non-Tablet bodies — the cache + the public collector covers it.
        let axs ← collectExternAxioms c
        for a in axs do
          record := recordAxiom record a
        modify fun s => { s with
          kernelAxioms := axs.foldl (init := s.kernelAxioms) (·.insert ·) }
      else
        -- Tablet const: 9-arm dispatch following plan §4.3 (mirroring
        -- Lean.CollectAxioms.collect's structural shape, with the
        -- `ProofMayAssumeTheorems` boundary cut at thmInfo).
        match info with
        | .thmInfo v =>
            -- Tablet theorem reached at any mode: boundary cut.
            -- "Assume every imported Tablet theorem holds as stated"
            -- (plan §2.2, mirrored by lean_semantic_fingerprint.lean).
            -- v.value is NEVER walked — proof irrelevance for Prop.
            -- Path mode only decides which accumulator records the
            -- entry (boundaryTheorems vs strictTheoremDeps) so that
            -- downstream strict_dep_consumers reverse-index still
            -- distinguishes "reached via proof body" from "reached
            -- via def/type strict path".
            let h := statementHash v.type
            if mode == .proofMayAssumeTheorems then
              record := recordBoundary record c h
              modify fun s => { s with boundaryTheorems := s.boundaryTheorems.insert c h }
            else
              record := recordStrictThm record c h
              modify fun s => { s with strictTheoremDeps := s.strictTheoremDeps.insert c h }
            for child in v.type.getUsedConstants do
              visitConst child .strict
        | .defnInfo v =>
            -- `def` and `abbrev` both elaborate to defnInfo; abbrev is
            -- distinguished by `hints == .abbrev`. Both go through the
            -- same arm per plan §4.3 (`defnInfo / abbrevInfo →`).
            let h := semanticHash v.type [v.value]
            record := recordStrictDef record c h
            modify fun s => { s with strictDefinitionDeps := s.strictDefinitionDeps.insert c h }
            for child in v.type.getUsedConstants do
              visitConst child .strict
            for child in v.value.getUsedConstants do
              visitConst child .strict
        | .axiomInfo _ | .opaqueInfo _ =>
            -- Project axiom or opaque: emit name. Rust wrapper applies
            -- per-node `load_approved_axioms`. We do NOT consult any
            -- approved-axioms list inside this script.
            record := recordAxiom record c
            modify fun s => { s with kernelAxioms := s.kernelAxioms.insert c }
            -- Walk the type so transitively-used boundaries / strict
            -- deps in the axiom's *type* are still captured.
            let typeExpr := info.type
            for child in typeExpr.getUsedConstants do
              visitConst child .strict
        | .inductInfo v =>
            -- Inductive: hash type + ctor names + ctor types; walk type
            -- and each ctor's type (Strict). Mix ctor names AND ctor
            -- types into the hash so adding/removing/retyping a
            -- constructor (semantic change) is detected.
            --
            -- Patch C-K Fix 2 (audit MEDIUM-HIGH): the prior version
            -- mixed only `v.type` and the ctor *names*, NOT the ctor
            -- types. Changing `mk : Nat → Foo` to `mk : Bool → Foo`
            -- preserved the inductive's semantic hash, so a strict
            -- dependency on the inductive would falsely stay valid.
            -- Now we deterministically gather each ctor's `cv.type`
            -- (in `v.ctors` order, which Lean materializes
            -- deterministically) and mix it into the same `hashExprs`
            -- list alongside `v.type`.
            let nameSeed : UInt64 :=
              v.ctors.foldl (fun acc n => mixHash acc (Hashable.hash n)) 0xc70_5_5
            let ctorTypes : List Expr :=
              v.ctors.foldl (fun acc ctor =>
                match env.find? ctor with
                | some (.ctorInfo cv) => acc ++ [cv.type]
                | _ => acc) ([] : List Expr)
            let h := uint64HashStr (mixHash (hashExprs (v.type :: ctorTypes)) nameSeed)
            record := recordStrictDef record c h
            modify fun s => { s with strictDefinitionDeps := s.strictDefinitionDeps.insert c h }
            for child in v.type.getUsedConstants do
              visitConst child .strict
            for ctor in v.ctors do
              match env.find? ctor with
              | some (.ctorInfo cv) =>
                  for child in cv.type.getUsedConstants do
                    visitConst child .strict
              | _ => pure ()
        | .ctorInfo v =>
            -- Constructor: walk type (Strict).
            for child in v.type.getUsedConstants do
              visitConst child .strict
        | .recInfo v =>
            -- Recursor: walk type (Strict).
            for child in v.type.getUsedConstants do
              visitConst child .strict
        | .quotInfo _ =>
            -- Quot built-ins: no further walk. Equivalent to the
            -- `pure ()` branch of plan §4.3.
            pure ()

      -- Detect mutual / unsafe declarations and surface as a non-fatal
      -- error per plan §4.5 ("Mutual blocks: error out cleanly").
      if info.isUnsafe then
        let msg := s!"unsafe declaration in closure: {c}"
        record := recordError record msg
        modify fun s => { s with errors := s.errors.push msg }
      if info.isPartial then
        let msg := s!"partial declaration in closure: {c}"
        record := recordError record msg
        modify fun s => { s with errors := s.errors.push msg }

  -- Replace the memo sentinel with the actual record for this `(c, mode)`.
  modify fun s => { s with memo := s.memo.insert (c, mode) record }

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
  match env.find? rootName with
  | none =>
      modify fun s => { s with
        errors := s.errors.push s!"missing root declaration: {rootName}" }
      return .other
  | some info =>
      let kind := classifyRoot info
      match info with
      | .thmInfo v =>
          for child in v.type.getUsedConstants do
            visitConst child .strict
          for child in v.value.getUsedConstants do
            visitConst child .proofMayAssumeTheorems
      | .defnInfo v =>
          -- Plan §4.2: definition root visits value Strict (no
          -- Prop-valued special case — that was dropped in this revision).
          for child in v.type.getUsedConstants do
            visitConst child .strict
          for child in v.value.getUsedConstants do
            visitConst child .strict
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
per-`(c, mode)` visitor with structured `boundary_theorems` /
`strict_theorem_deps` / `strict_definition_deps`. The mode-aware dispatch
is subtle. To defend against bugs in the primary (or future Lean
elaboration changes that violate its assumptions), we run a *secondary*
collector side-by-side: an `Lean.CollectAxioms.collect`-shaped pass with
the same `isTabletConst` + `Name.isInternalDetail` cuts, but no
per-`(c, mode)` keying. It emits only `{kernel_axioms, boundary_theorems}`
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

private structure AxCheckState where
  /-- Constants already visited (mode-independent: the axiomization
  customization makes every Tablet theorem a boundary regardless of how
  it was reached). -/
  seen             : Std.HashSet Name := {}
  /-- Non-Tablet axioms reached transitively. -/
  kernelAxioms     : Std.HashSet Name := {}
  /-- Tablet theorems reached transitively (axiomized: recorded here,
  not recursed into). -/
  boundaryTheorems : Std.HashSet Name := {}
  /-- Errors raised during traversal. -/
  errors           : Array String     := #[]
  /-- Fix A: the active root constant, mirroring `VisitorState.active`.
  Needed so the axcheck gate can apply the same `isOwnNodeAux` filter as
  the primary collector — omitting it here makes the collectors disagree
  on an own-node `let rec` aux ⇒ spurious `internal_error`. Initialized
  to the resolved root name in `runAxiomizationCheck`. -/
  active           : Name             := .anonymous

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
    if (← get).seen.contains c then return
    modify fun s => { s with seen := s.seen.insert c }
    match env.checked.get.find? c with
    | none =>
        modify fun s => { s with
          errors := s.errors.push s!"missing constant during traversal: {c}" }
    | some info =>
        if isTabletConst env c && (isTabletGeneratedArtifact env c || isOwnNodeAux env (← get).active c) then
          -- §4.5 transparent walk: auto-generated artifact (e.g.
          -- `Foo._proof_1`, `Foo.eq_1`, `Foo._sunfold`, and reserved
          -- realization names such as `Foo.congr_simp` / `Foo.eq_def`), OR
          -- (Fix A) an auxiliary of the ACTIVE node itself
          -- (`isOwnNodeAux`, e.g. a lifted user-named `let rec` binder).
          -- Mirrors the primary collector's transparent-walk gate above so
          -- the dual collectors stay in agreement; do NOT record it as a
          -- Tablet boundary, but DO recurse into its body.
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
              -- FIX 1 (axiom sub-case), mirrored in the secondary
              -- collector so the dual collectors stay in agreement: a
              -- reserved-shaped Tablet axiom reached through the
              -- transparent-walk branch must be recorded into
              -- `kernelAxioms`, not dropped. Same arm as the normal
              -- Tablet `.axiomInfo` case below.
              modify fun s => { s with
                kernelAxioms := s.kernelAxioms.insert c }
              axCheckExpr env v.type
          | .quotInfo _ => pure ()
        else if isTabletConst env c then
          -- Tablet const, not an artifact: 9-arm dispatch with the
          -- thmInfo axiomization cut.
          match info with
          | .thmInfo v =>
              -- Axiomize: record name, walk type only.
              modify fun s => { s with
                boundaryTheorems := s.boundaryTheorems.insert c }
              axCheckExpr env v.type
          | .axiomInfo v =>
              -- Project axiom (declared in a Tablet module): primary
              -- script puts these in `kernel_axioms`; mirror that.
              modify fun s => { s with
                kernelAxioms := s.kernelAxioms.insert c }
              axCheckExpr env v.type
          | .opaqueInfo v =>
              modify fun s => { s with
                kernelAxioms := s.kernelAxioms.insert c }
              axCheckExpr env v.type
          | .defnInfo v =>
              axCheckExpr env v.type
              axCheckExpr env v.value
          | .inductInfo v =>
              axCheckExpr env v.type
              for ctor in v.ctors do axCheckCollect env ctor
          | .ctorInfo v  => axCheckExpr env v.type
          | .recInfo v   => axCheckExpr env v.type
          | .quotInfo _  => pure ()
        else
          -- Non-Tablet const: full recursive walk mirroring
          -- `Lean.CollectAxioms.collect`. This differs in shape from the
          -- primary (which delegates to `Lean.collectAxioms` at the
          -- non-Tablet boundary), but the *transitive axiom set* is the
          -- same, which is what the cross-check compares.
          match info with
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

/-- Top-level axcheck traversal: dispatch on the root's kind, walk
type and value (for theorems/defs/abbrevs). Mirrors the primary
script's `visitRoot` (plan §4.2). Root itself is NOT recorded. -/
private def axCheckRoot (env : Environment) (rootName : Name) : AxCheckM Unit := do
  match env.checked.get.find? rootName with
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
`keyer` selects the mapping: theorem-dep sites pass `depKeyName` (the
principal-only gate, a soundness guard); the definition-dep site passes
`depDefKeyName` (declaring-module node id, no gate — see `tabletDefDepNodeId?`).
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

/-- Compute set differences between primary and axcheck for diagnostic
output. Returns sorted name lists. -/
private def setDiff (a b : Std.HashSet Name) : List String :=
  stableSort (a.toList.filter (fun n => !b.contains n) |>.map toString)

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
    (skipped : Bool) : Json :=
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
      ("axcheck_only_boundaries",  Json.arr #[])
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
    let primaryReachedTheoremNames : Std.HashSet Name :=
      let init := primary.boundaryTheorems.fold
        (init := ({} : Std.HashSet Name)) (fun acc n _ => acc.insert n)
      primary.strictTheoremDeps.fold (init := init) (fun acc n _ => acc.insert n)
    let axiomsAgree :=
      primary.kernelAxioms.toList.all axcheck.kernelAxioms.contains
        && axcheck.kernelAxioms.toList.all primary.kernelAxioms.contains
    let boundariesAgree :=
      primaryReachedTheoremNames.toList.all axcheck.boundaryTheorems.contains
        && axcheck.boundaryTheorems.toList.all primaryReachedTheoremNames.contains
    let agreed := axiomsAgree && boundariesAgree
    let kernelAxList := stableSort (axcheck.kernelAxioms.toList.map toString)
    let boundaryList := stableSort (axcheck.boundaryTheorems.toList.map toString)
    let primaryOnlyAx     := setDiff primary.kernelAxioms axcheck.kernelAxioms
    let axcheckOnlyAx     := setDiff axcheck.kernelAxioms primary.kernelAxioms
    let primaryOnlyBnd    := setDiff primaryReachedTheoremNames axcheck.boundaryTheorems
    let axcheckOnlyBnd    := setDiff axcheck.boundaryTheorems primaryReachedTheoremNames
    Json.mkObj [
      ("kernel_axioms",            Json.arr (kernelAxList.toArray.map Json.str)),
      ("boundary_theorems",        Json.arr (boundaryList.toArray.map Json.str)),
      ("agreed",                   Json.bool agreed),
      ("skipped",                  Json.bool false),
      ("primary_only_axioms",      Json.arr (primaryOnlyAx.toArray.map Json.str)),
      ("axcheck_only_axioms",      Json.arr (axcheckOnlyAx.toArray.map Json.str)),
      ("primary_only_boundaries",  Json.arr (primaryOnlyBnd.toArray.map Json.str)),
      ("axcheck_only_boundaries",  Json.arr (axcheckOnlyBnd.toArray.map Json.str))
    ]

private def emitJson
    (env      : Environment)
    (nodeName : String)
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
  -- check. Theorem deps use `depKeyName` (principal-only gate); definition
  -- deps use `depDefKeyName` (declaring-module node id). Byte-identical to
  -- the legacy output for top-level math deps.
  let boundaryArr      := pairArrayJson env "statement_hash" depKeyName
    (s.boundaryTheorems.toList)
  let strictThmArr     := pairArrayJson env "value_hash" depKeyName
    (s.strictTheoremDeps.toList)
  -- Definition deps use `depDefKeyName` (declaring-module node id, no
  -- principal gate) so Preamble-shared structs and Aeneas co-generated loop
  -- helpers map to their host present-node id instead of fail-closing.
  let strictDefArr     := pairArrayJson env "semantic_hash" depDefKeyName
    (s.strictDefinitionDeps.toList)
  let errorsArr        : Array Json := s.errors.map Json.str
  let payload : Json := Json.mkObj [
    ("node",                  Json.str nodeName),
    ("status",                Json.str status),
    ("root_kind",             Json.str rootKind),
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
private def runClosure (env : Environment) (rootName : Name)
    : IO (VisitorState × RootKind) := do
  let coreCtx : Core.Context := {
    fileName := "<lean_local_closure>",
    fileMap  := default
  }
  let coreState : Core.State := { env := env }
  let initialVisitor : VisitorState := { active := rootName }
  let visitor : VisitorM RootKind := visitRoot rootName
  let action : CoreM (RootKind × VisitorState) := do
    StateRefT'.run visitor initialVisitor
  let ((kind, finalSt), _) ← action.toIO coreCtx coreState
  return (finalSt, kind)

/-- Run the axcheck collector over `rootName` against `env`, returning
the final axcheck state. -/
private def runAxiomizationCheck (env : Environment) (rootName : Name)
    : IO AxCheckState := do
  -- Fix A: seed `active` with the resolved root so the axcheck gate's
  -- `isOwnNodeAux` filter mirrors the primary collector (which seeds
  -- `VisitorState.active` the same way in `runClosure`).
  let (_, finalSt) ← (axCheckRoot env rootName).run { active := rootName }
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

/-- FIX 2 + MACRO-BAN owner-file scan, packaged for `runAndEmit`. Reads the
node's own source and returns `some <diagnostic>` if the node must be
rejected, or `none` if the node is clean. Three rejection causes, checked
in order:

* unreadable source ⇒ fail closed (the invariant cannot be verified);
* a banned macro/syntax/elaborator-defining command (MACRO BAN) — checked
  BEFORE the reserved-name list because such a command can synthesize a
  reserved-shaped declaration the name scan cannot see, so it is the more
  fundamental violation to report;
* an authored reserved-shaped auxiliary declaration (FIX 2).

Any unexpected IO failure during the scan is itself a fail-closed
rejection. -/
private def ownerFileScanRejection (nodeName : String) : IO (Option String) := do
  let scan : IO (Option String) := do
    let result ← scanOwnerSourceFile nodeName
    match result.unreadable with
    | some path =>
        return some s!"owner-file authoring scan could not read node source {path}; \
          failing closed (cannot verify the macro-command / reserved-shaped-auxiliary \
          invariant)"
    | none =>
        if !result.bannedCommands.isEmpty then
          return some s!"node defines macro/syntax/elaborator command(s) \
            {result.bannedCommands.toList}: an ordinary Tablet node file may not \
            author a `macro`/`macro_rules`/`elab`/`elab_rules`/`syntax`/\
            `declare_syntax_cat`/`binder_predicate` command. Such a command can \
            synthesize a top-level declaration whose name never appears literally in \
            source (e.g. a macro emitting `theorem {nodeName}.eq_1`), bypassing the \
            local-closure / authored-name invariant. Remove the command (term-level \
            `notation`/`infix`/`prefix`/`postfix` are allowed)."
        else if !result.reservedNames.isEmpty then
          return some s!"node authors reserved-shaped private auxiliary declaration(s) \
            {result.reservedNames.toList}: a declaration whose final name component \
            matches a compiler-internal shape (leading `_`, or \
            `eq_`/`match_`/`proof_`/`omega_` + digits) is an unregistered cross-node \
            dependency hidden from the local-closure check. Move the auxiliary into \
            its own registered node, or rename it to a non-reserved shape."
        else
          return none
  match (← scan.toBaseIO) with
  | .ok r    => return r
  | .error e => return some s!"owner-file scan failed: {e}"

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
private def runAndEmit (nodeName : String) (env : Environment)
    (axCheckEnabled : Bool) : IO Unit := do
  -- Resolve the on-disk root declaration. Top-level (math) nodes take the
  -- fast path (node name == decl name); namespaced (PV) nodes are resolved by
  -- a unique same-final-name match in the node's own module. ZERO matches ⇒
  -- `missing_declaration` (unchanged); MORE THAN ONE ⇒ an explicit ambiguity
  -- rejection, never an arbitrary pick. The resolved name then flows through
  -- the owner-scan, closure walk, axcheck and `visitRoot` root-KIND check
  -- exactly as the bare name used to — none of those guards are weakened.
  match resolveRoot env nodeName with
  | .missing =>
      IO.println (emitFailureJson nodeName "missing_declaration"
        #[s!"declaration {nameFromString nodeName} not found in module \
            {moduleForNode nodeName} (no top-level decl, and no unique \
            namespaced declaration with final component {nodeName})"])
  | .ambiguous candidates =>
      -- Safe over-reject: refuse to guess among multiple same-final-name
      -- declarations in the node's own module. A distinct status so the Rust
      -- wrapper / operator can tell this apart from a genuine missing decl.
      IO.println (emitFailureJson nodeName "ambiguous_declaration"
        #[s!"node {nodeName} has multiple candidate root declarations with final \
            component {nodeName} in module {moduleForNode nodeName}: \
            {candidates.toList}; refusing to pick one (resolve the namespace \
            ambiguity in the node source)"])
  | .resolved rootName =>
      -- FIX 2: owner-file authoring invariant. Scan THIS node's own source
      -- for authored declarations whose final name component is
      -- reserved-shaped (the `isInternalDetail` string shapes). Such a
      -- declaration is an unregistered private auxiliary masquerading as a
      -- generated artifact; reject the node at its own acceptance so every
      -- accepted owner is clean and consumers are transitively safe. Fail
      -- closed: a found offender OR an unreadable source flips status to
      -- `internal_error`. This runs before the closure walk.
      if let some rejection ← ownerFileScanRejection nodeName then
        IO.println (emitFailureJson nodeName "internal_error" #[rejection])
        return
      try
        let (finalSt, rootKind) ← runClosure env rootName
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
              let axCheckSt ← runAxiomizationCheck env rootName
              pure (axiomizationCheckJson finalSt axCheckSt (skipped := false), none)
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
        IO.println (emitJson env nodeName "ok" rootKind.toString finalSt axCheckJson)
      catch e =>
        IO.println (emitFailureJson nodeName "internal_error"
          #[s!"traversal failed: {e.toString}"])

/-- FIX 2 coverage: emit the minimal envelope for a `--scan-only` run. The
scan-only mode runs ONLY the owner-file authored-name scan (no closure
walk, no axcheck), so the dep/axiom arrays are always empty and
`axiomization_check` reports `skipped: true` (a trivial pass for the Rust
wrapper). `status` is `ok` when the node is clean, `internal_error` with
the reserved-shape diagnostic in `errors[]` when it authors a
reserved-shaped auxiliary (or its source is unreadable ⇒ fail closed).
The shape matches `emitFailureJson` so `parse_local_closure_response`
ingests it unchanged. -/
private def emitScanOnlyJson (nodeName : String) (rejection : Option String) : String :=
  let (status, errors) :=
    match rejection with
    | some msg => ("internal_error", #[msg])
    | none     => ("ok", (#[] : Array String))
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

/-- FIX 2 coverage: run ONLY the owner-file authored-name scan over the
node's own source and emit a scan-only envelope. This is the universal,
all-node-kind authoring gate: it reuses the exact same `where`-aware
`ownerFileScanRejection` the full probe runs, but decoupled from the
closure-record machinery (which is legitimately proof-node-only). The
scan parses the node's source against an `Init`-only environment (it does
NOT `importModules` the node's own module), so it is cheap and works even
when the node's oleans are not built — generated internals are never
source declaration commands, so a source parse never sees them. -/
private def runScanOnly (nodeName : String) : IO Unit := do
  let rejection ← ownerFileScanRejection nodeName
  IO.println (emitScanOnlyJson nodeName rejection)

/-- Parse the CLI args looking for `--no-axcheck` and `--scan-only`.
Returns the node name plus the `axCheckEnabled` and `scanOnly` flags.
Plan §4.6.1: `--no-axcheck` is an additive opt-out so the default remains
"run both collectors". FIX 2 coverage: `--scan-only` is an additive mode
selector so the default remains "run the full closure probe". -/
private def parseArgs (args : List String) : Option (String × Bool × Bool) :=
  match args with
  | [] => none
  | nodeName :: rest =>
      let hasNoAxcheck := rest.any (· == "--no-axcheck")
      let hasScanOnly := rest.any (· == "--scan-only")
      some (nodeName, !hasNoAxcheck, hasScanOnly)

def main (args : List String) : IO UInt32 := do
  match parseArgs args with
  | none =>
      IO.eprintln "ERR\t<global>\tno node name provided"
      return 2
  | some (nodeName, axCheckEnabledByArgs, scanOnly) =>
      -- The owner-file scan parses against an `Init`-only environment, so
      -- the search path must be initialized for BOTH modes (scan-only and
      -- the full probe) before any `importModules` call.
      initSearchPath (← findSysroot)
      -- FIX 2 coverage: `--scan-only` runs ONLY the owner-file authoring
      -- scan and returns. No node-olean import, no closure walk, no
      -- axcheck — so this mode is the cheap, all-node-kind authoring gate
      -- that the Rust kernel runs on every changed/new node file
      -- (including Definition-kind nodes the full closure probe skips).
      if scanOnly then
        runScanOnly nodeName
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
        -- `runAndEmit` resolves the root declaration from the loaded `env`
        -- (namespaced-node fix): the namespace is not passed on argv, so we
        -- resolve it from the already-imported environment rather than the
        -- bare node name.
        runAndEmit nodeName env axCheckEnabled
        return 0
      catch e =>
        IO.println (emitFailureJson nodeName "elaboration_error"
          #[s!"importModules failed: {e.toString}"])
        return 0

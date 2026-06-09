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
suffix filters here unless Lean also prevents users from authoring them. -/
private def isTabletGeneratedArtifact (env : Environment) (name : Name) : Bool :=
  name.isInternalDetail || Lean.isReservedName env name

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
      if isTabletConst env c && isTabletGeneratedArtifact env c then
        -- §4.5 transparent walk: auto-generated artifact (e.g.
        -- `Foo._proof_1_1`, `Foo._sunfold`, `Foo.eq_1`, and reserved
        -- realization names such as `Foo.congr_simp` / `Foo.eq_def`).
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
        | .axiomInfo _ | .quotInfo _ =>
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
        if isTabletConst env c && isTabletGeneratedArtifact env c then
          -- §4.5 transparent walk: auto-generated artifact (e.g.
          -- `Foo._proof_1`, `Foo.eq_1`, `Foo._sunfold`, and reserved
          -- realization names such as `Foo.congr_simp` / `Foo.eq_def`);
          -- do NOT record it as a Tablet boundary, but DO recurse into its
          -- body.
          match info with
          | .thmInfo v   => axCheckExpr env v.type; axCheckExpr env v.value
          | .defnInfo v  => axCheckExpr env v.type; axCheckExpr env v.value
          | .opaqueInfo v => axCheckExpr env v.type; axCheckExpr env v.value
          | .inductInfo v =>
              axCheckExpr env v.type
              for ctor in v.ctors do axCheckCollect env ctor
          | .ctorInfo v  => axCheckExpr env v.type
          | .recInfo v   => axCheckExpr env v.type
          | .axiomInfo _ | .quotInfo _ => pure ()
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
sorted by name for determinism. -/
private def pairArrayJson (hashField : String) (pairs : List (Name × String)) : Json :=
  let sorted := pairs.toArray.qsort (fun a b => toString a.1 < toString b.1)
  let items : Array Json := sorted.map fun (n, h) =>
    Json.mkObj [
      ("name", Json.str (toString n)),
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
    (nodeName : String)
    (status   : String)
    (rootKind : String)
    (s        : VisitorState)
    (axCheck  : Json) : String :=
  let kernelAxiomsList := stableSort (s.kernelAxioms.toList.map toString)
  let kernelAxiomsArr  : Array Json := kernelAxiomsList.toArray.map Json.str
  let boundaryArr      := pairArrayJson "statement_hash"
    (s.boundaryTheorems.toList)
  let strictThmArr     := pairArrayJson "value_hash"
    (s.strictTheoremDeps.toList)
  let strictDefArr     := pairArrayJson "semantic_hash"
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
  let (_, finalSt) ← (axCheckRoot env rootName).run {}
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
private def runAndEmit (nodeName : String) (env : Environment) (rootName : Name)
    (axCheckEnabled : Bool) : IO Unit := do
  match env.find? rootName with
  | none =>
      IO.println (emitFailureJson nodeName "missing_declaration"
        #[s!"declaration {rootName} not found in module {moduleForNode nodeName}"])
  | some _ =>
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
        IO.println (emitJson nodeName "ok" rootKind.toString finalSt axCheckJson)
      catch e =>
        IO.println (emitFailureJson nodeName "internal_error"
          #[s!"traversal failed: {e.toString}"])

/-- Parse the CLI args looking for `--no-axcheck`. Returns the node name
and the `axCheckEnabled` flag. Plan §4.6.1: disable flag is an additive
opt-out so the default remains "run both collectors". -/
private def parseArgs (args : List String) : Option (String × Bool) :=
  match args with
  | [] => none
  | nodeName :: rest =>
      let hasNoAxcheck := rest.any (· == "--no-axcheck")
      some (nodeName, !hasNoAxcheck)

def main (args : List String) : IO UInt32 := do
  match parseArgs args with
  | none =>
      IO.eprintln "ERR\t<global>\tno node name provided"
      return 2
  | some (nodeName, axCheckEnabledByArgs) =>
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
      initSearchPath (← findSysroot)
      let rootName := nameFromString nodeName
      try
        let env ← importModules #[{ module := moduleForNode nodeName }] {}
        runAndEmit nodeName env rootName axCheckEnabled
        return 0
      catch e =>
        IO.println (emitFailureJson nodeName "elaboration_error"
          #[s!"importModules failed: {e.toString}"])
        return 0

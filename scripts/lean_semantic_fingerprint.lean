import Lean

open Lean

/-!
# Tablet semantic fingerprint

Given a Tablet node name (e.g. `"Main2"`), import its module and emit a
deterministic textual fingerprint of the *closure* of that node's elaborated
declaration in the Lean environment. Two runs on semantically-equivalent
inputs must produce byte-identical output; any meaning-affecting change must
produce a different output.

## Closure policy (what enters the fingerprint)

Starting from the seed declaration, we walk:

* A **theorem**: only its `type`. The proof term (`value`) is *not* walked
  — proof changes do not change a theorem's meaning. This is what makes
  "edits to lemma B used only in A's proof" leave A's fingerprint
  unchanged. Proof-only deps therefore never enter A's closure.
* A **definition** / **opaque** (in strict mode this would also include
  opaque values; we don't): both `type` and `value`. A def's value *is*
  part of its meaning, so if `D : Nat := 3` becomes `D : Nat := 4`, every
  fingerprint that transitively uses `D` must change.
* An **axiom** / **constructor** / **recursor** / **quotient**: just `type`.
* An **inductive**: `type` plus the names of its constructors (so adding /
  removing a constructor is detected).

For every const we encounter during this walk we apply a *Mathlib boundary*:
if the const's declaring module is not under `Tablet.*`, we record only its
fully-qualified name (`extern|<name>`) and stop. We rely on the cache key
in `sync.py` (toolchain sha + lake-manifest sha) to invalidate when the
external semantics shift; trying to recurse into Mathlib's transitive
defns is both wasteful and pathological for nodes referencing
`Real.exp` / `Real.log` etc.

## Why we hash, not textually serialize

Earlier versions of this script textually serialized every expression. That
turned out to be exponential on Tablet defs whose value uses nested
`Classical.choose` / `Classical.choose_spec`: the elaborated value embeds
the same large existential-statement type at multiple positions, but the
in-memory `Expr` shares those subterms (Lean's `Expr.hash` is computed at
construction and exploits sharing). A naive recursive serializer revisits
each shared subterm at every reference, producing strings of size
`O(2^depth)` and hanging on real nodes (`HighRegionBound` ran for >5min
with no progress).

The new walk (`walkExprImpl`) uses pointer-keyed memoization
(`ptrAddrUnsafe`): each unique `Expr` pointer is hashed once and the result
is reused on every subsequent reference. Cost is `O(unique-pointers)`
rather than `O(walk-fanout)`, which is the same asymptotic as Lean's own
`Expr.hash`.

## Why we do not just call `Expr.hash` directly

`Expr.hash` is computed at `Expr` construction time and is structural over
the *full* AST, including:

* `Expr.mdata` (source-position annotations and other syntactic metadata
  that re-elaboration can shuffle without changing the kernel term);
* binder names (`λ x => x` and `λ y => y` are alpha-equivalent terms with
  different `Expr.hash` values).

Either of those would produce false-positive fingerprint changes on
re-elaboration with no semantic delta, wasting downstream LLM verifier
cycles. So `walkExprImpl` does its own structural hash with two fixed
canonicalizations baked in:

* `mdata` is transparently skipped (`.mdata _ body => walkExprImpl body st`);
* binder *names* in `lam` / `forallE` / `letE` are not mixed into the
  hash. Binder *info* (default / implicit / strictImplicit / instImplicit)
  *is* included, since flipping `(x : T)` ↔ `{x : T}` changes elaboration.

This matches the precision policy of the previous text serializer
(which dropped binder names via `_` placeholders and ignored `mdata`),
without paying the textual cost.

## Const collection runs in the same walk

Earlier the closure was discovered by a separate `collectConsts` pass over
each expression. That had the same exponential blow-up on shared subterms.
`walkExprImpl` accumulates the const set as part of the same memoized
traversal, so const collection is also `O(unique-pointers)`. Each cache
slot stores only the hash (`UInt64`); the const set lives in the threaded
state and is updated when we visit a `.const` node for the first time.

## Output format

For each Tablet const reached, one line is emitted:

```
const|<name>|<kind>|lvls=<comma-separated level params>|typehash=<UInt64>|...kind-specific fields...
```

Where `<kind>` is one of `thm | axiom | opaque | def | quot | induct | ctor | rec`,
and `def` additionally carries `valuehash=<UInt64>`. For each Mathlib const
reached on the boundary, a single line `extern|<name>` is emitted.

All lines are sorted lexicographically and joined with `||`, with a
leading `root|<name>` line, to produce one deterministic payload per
seed. The payload is stable across processes and machines as long as the
toolchain, manifest, and elaborated AST are identical.

## Cache key plumbing

`trellis/checker/sync.py` keys its sidecar by:

* `SEMANTIC_PAYLOAD_CACHE_VERSION` (bump on output-format changes);
* sha256 of *this script*;
* sha256 of the `lean-toolchain` pin;
* sha256 of the lake manifest;
* sha256 of the node's `.lean` and `.olean` files plus those of every
  transitive Tablet dep.

So any of: format change, script change, toolchain bump, mathlib rev
bump, edit to a Tablet source file, or rebuilt olean for any closure
member, invalidates the sidecar and forces a fresh fingerprint compute.
The fingerprint then either matches the previous payload (no semantic
change → cached LLM verdict reused) or differs (semantic change → LLM
re-runs).
-/

private def nameFromString (s : String) : Name :=
  s.splitOn "." |>.foldl Name.str Name.anonymous

/-- Distinct salt per `BinderInfo` so that flipping `(x : T) → ...` to
`{x : T} → ...` (or instance-implicit, etc.) is detected by the hash even
though the binder name itself is not. -/
private def binderInfoTag : BinderInfo → UInt64
  | .default        => 0x10
  | .implicit       => 0x11
  | .strictImplicit => 0x12
  | .instImplicit   => 0x13

/-- Hash a `Literal` by case + payload hash. (`Hashable` for `Literal`
exists upstream, but writing it out makes the format intentional and
stable.) -/
private def literalHash : Literal → UInt64
  | .natVal n => mixHash 0xaa (Hashable.hash n)
  | .strVal s => mixHash 0xab (Hashable.hash s)

/-- Pointer-memoized walk over an `Expr`. Returns

    `(structural-hash, (ptr-cache, accumulated const set))`

with the cache and const-set threaded through subsequent calls so we
visit each unique `Expr` pointer at most once across the whole walk
(including across multiple top-level expressions, e.g. a def's `type` and
`value` that share subterms).

The hash is mdata- and binder-name-insensitive; everything else
(constructor tag, structure, universes, binder info, const names,
literals) is mixed in.

Marked `unsafe` because `ptrAddrUnsafe` is unsafe; called only from
`fingerprintExprImpl` below, which is wrapped via `@[implemented_by]` so
the rest of the script stays in safe code. -/
private unsafe def walkExprImpl
    (e : Expr)
    (st : Std.HashMap USize UInt64 × Std.HashSet Name)
    : UInt64 × (Std.HashMap USize UInt64 × Std.HashSet Name) :=
  let addr := ptrAddrUnsafe e
  match st.1[addr]? with
  | some h => (h, st)
  | none =>
    let (h, st') : UInt64 × (Std.HashMap USize UInt64 × Std.HashSet Name) :=
      match e with
      | .bvar idx =>
          (mixHash 0x01 idx.toUInt64, st)
      | .fvar fvarId =>
          (mixHash 0x02 (Hashable.hash fvarId.name), st)
      | .mvar _ =>
          (0x03, st)
      | .sort lvl =>
          (mixHash 0x04 lvl.hash, st)
      | .const name lvls =>
          let consts := st.2.insert name
          let lvlH : UInt64 :=
            lvls.foldl (fun acc l => mixHash acc l.hash) 0
          (mixHash 0x05 (mixHash (Hashable.hash name) lvlH), (st.1, consts))
      | .app f a =>
          let (hF, st1) := walkExprImpl f st
          let (hA, st2) := walkExprImpl a st1
          (mixHash 0x06 (mixHash hF hA), st2)
      | .lam _ ty body bi =>
          let (hTy,   st1) := walkExprImpl ty   st
          let (hBody, st2) := walkExprImpl body st1
          (mixHash 0x07 (mixHash hTy (mixHash hBody (binderInfoTag bi))), st2)
      | .forallE _ ty body bi =>
          let (hTy,   st1) := walkExprImpl ty   st
          let (hBody, st2) := walkExprImpl body st1
          (mixHash 0x08 (mixHash hTy (mixHash hBody (binderInfoTag bi))), st2)
      | .letE _ ty val body nondep =>
          let (hTy,   st1) := walkExprImpl ty   st
          let (hVal,  st2) := walkExprImpl val  st1
          let (hBody, st3) := walkExprImpl body st2
          let nondepBit : UInt64 := if nondep then 1 else 0
          (mixHash 0x09 (mixHash (mixHash hTy hVal) (mixHash hBody nondepBit)), st3)
      | .lit lit =>
          (mixHash 0x0a (literalHash lit), st)
      | .mdata _ body =>
          -- mdata stripped: source-position drift must not change the hash
          walkExprImpl body st
      | .proj typeName idx struct =>
          let (hStruct, st1) := walkExprImpl struct st
          (mixHash 0x0b
            (mixHash (Hashable.hash typeName) (mixHash idx.toUInt64 hStruct)),
           st1)
    (h, (st'.1.insert addr h, st'.2))

/-- Top-level entry that runs `walkExprImpl` over a list of expressions
sharing one cache + const-set, then returns `(combined hash, sorted unique
const list)`.

The combined hash is the sequential mix of each expression's hash
(under a fixed seed), so callers passing `[type, value]` get a single
def-level digest that is sensitive to either component changing. The
returned const list is sorted by `toString` ordering for determinism. -/
private unsafe def fingerprintExprsImpl (exprs : List Expr)
    : UInt64 × List Name :=
  let init : UInt64 × (Std.HashMap USize UInt64 × Std.HashSet Name) :=
    (0xfeed_face_cafe_beef, ((∅ : Std.HashMap USize UInt64),
                             (∅ : Std.HashSet Name)))
  let (h, (_, consts)) :=
    exprs.foldl (fun (acc, st) e =>
        let (hE, st') := walkExprImpl e st
        (mixHash acc hE, st'))
      init
  let names := consts.toList
  let sorted := (names.toArray.qsort (fun a b => toString a < toString b)).toList
  (h, sorted)

/-- Safe-callable wrapper. The `@[implemented_by]` annotation tells the
compiler to use `fingerprintExprsImpl` at runtime; the body here is a
typechecking placeholder that is never actually executed. This lets the
rest of the script (e.g. `visitConst`) stay in pure / `Except` code while
the unsafe pointer-memoization happens behind the boundary. -/
@[implemented_by fingerprintExprsImpl]
private def fingerprintExprs (_exprs : List Expr) : UInt64 × List Name :=
  (0, [])

private structure FingerprintState where
  seen  : Std.HashSet Name := {}
  lines : Array String := #[]

private def levelParamsString (params : List Name) : String :=
  String.intercalate "," (params.map toString)

/-- Merge two pre-sorted-and-unique name lists into one sorted-and-unique
list. Used to fold in extra refs (e.g. inductive constructors) on top of
the const list returned by `fingerprintExprs`. -/
private def mergeUniqueSortedNames (a b : List Name) : List Name :=
  let combined := (a ++ b).toArray.qsort (fun a b => toString a < toString b)
  let step (acc : Std.HashSet Name × List Name) (name : Name) :=
    if acc.1.contains name then acc
    else (acc.1.insert name, name :: acc.2)
  let (_, revOut) := combined.toList.foldl step ({}, [])
  revOut.reverse

/-- True iff `name`'s declaring module is in the `Tablet.*` namespace.

Tablet nodes are bare-named consts (e.g. `def BaseGraphProfile`) but live
in modules under `Tablet/`, so we identify them by the env-recorded
module rather than by the const name's namespace. -/
private def isTabletConst (env : Environment) (name : Name) : Bool :=
  match env.getModuleIdxFor? name with
  | some idx =>
    match env.allImportedModuleNames[idx]? with
    | some modName =>
      match modName with
      | .str (.str .anonymous "Tablet") _ => true
      | _ => false
    | none => false
  | none =>
    -- Decl exists in env but no module idx (e.g. the seed itself before
    -- import resolution); treat as Tablet since the seed is always one
    -- of our nodes.
    true

/-- Visit one const. If we have already seen it, no-op. If it lives
outside `Tablet.*`, emit an `extern|<name>` boundary line and stop. Else,
emit a kind-specific `const|<name>|<kind>|...` line and recurse into the
sub-consts the kind dictates we should walk (see the closure-policy
section in the file header). -/
private partial def visitConst (env : Environment) (name : Name) (st : FingerprintState) : Except String FingerprintState := do
  if st.seen.contains name then
    return st
  let st := { st with seen := st.seen.insert name }
  let some info := env.find? name
    | return { st with lines := st.lines.push s!"missing|{name}" }

  if !isTabletConst env name then
    return { st with lines := st.lines.push s!"extern|{name}" }

  let (line, refs) :=
    match info with
    | .thmInfo v =>
        let (typeHash, typeRefs) := fingerprintExprs [v.type]
        let line := s!"thm|lvls={levelParamsString v.levelParams}|typehash={typeHash}"
        (line, typeRefs)
    | .axiomInfo v =>
        let (typeHash, typeRefs) := fingerprintExprs [v.type]
        let line := s!"axiom|lvls={levelParamsString v.levelParams}|typehash={typeHash}"
        (line, typeRefs)
    | .opaqueInfo v =>
        let (typeHash, typeRefs) := fingerprintExprs [v.type]
        let line := s!"opaque|lvls={levelParamsString v.levelParams}|typehash={typeHash}"
        (line, typeRefs)
    | .defnInfo v =>
        -- type and value share one cache so any subterm that appears in
        -- both (very common after elaboration) is hashed once.
        let (defHash, defRefs) := fingerprintExprs [v.type, v.value]
        let line := s!"def|lvls={levelParamsString v.levelParams}|hash={defHash}"
        (line, defRefs)
    | .quotInfo v =>
        let (typeHash, typeRefs) := fingerprintExprs [v.type]
        let line := s!"quot|lvls={levelParamsString v.levelParams}|typehash={typeHash}"
        (line, typeRefs)
    | .inductInfo v =>
        let (typeHash, typeRefs) := fingerprintExprs [v.type]
        let ctors := String.intercalate "," (v.ctors.map toString)
        let allRefs := mergeUniqueSortedNames typeRefs v.ctors
        let line := s!"induct|lvls={levelParamsString v.levelParams}|typehash={typeHash}|ctors={ctors}|params={v.numParams}|indices={v.numIndices}"
        (line, allRefs)
    | .ctorInfo v =>
        let (typeHash, typeRefs) := fingerprintExprs [v.type]
        let line := s!"ctor|lvls={levelParamsString v.levelParams}|typehash={typeHash}|induct={v.induct}|cidx={v.cidx}|params={v.numParams}|fields={v.numFields}"
        (line, typeRefs)
    | .recInfo v =>
        let (typeHash, typeRefs) := fingerprintExprs [v.type]
        let line := s!"rec|lvls={levelParamsString v.levelParams}|typehash={typeHash}"
        (line, typeRefs)

  let st := { st with lines := st.lines.push s!"const|{name}|{line}" }
  refs.foldlM (init := st) fun acc child =>
    if child == name then
      return acc
    else
      visitConst env child acc

private def fingerprintPayloadFor (env : Environment) (declName : Name) : Except String String := do
  let st ← visitConst env declName {}
  let sortedLines := (st.lines.qsort (fun a b => a < b)).toList
  let payload := String.intercalate "||" (s!"root|{declName}" :: sortedLines)
  return payload

private def moduleForNode (nodeName : String) : Name :=
  nameFromString s!"Tablet.{nodeName}"

def main (args : List String) : IO UInt32 := do
  initSearchPath (← findSysroot)
  let nodeNames := args.toArray
  if nodeNames.isEmpty then
    IO.eprintln "ERR\t<global>\tno node names provided"
    return 1
  for nodeName in nodeNames do
    let declName := nameFromString nodeName
    try
      let env ← importModules #[{ module := moduleForNode nodeName }] {}
      match fingerprintPayloadFor env declName with
      | .ok payload =>
          IO.println s!"FP\t{nodeName}\t{payload}"
      | .error err =>
          IO.println s!"ERR\t{nodeName}\t{err}"
    catch e =>
      IO.println s!"ERR\t{nodeName}\t{e.toString}"
  return 0

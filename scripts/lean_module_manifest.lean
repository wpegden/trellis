import Lean

open Lean

/-!
Read certificate-v2 manifests directly from the exact artifact bundle replayed
by `leanchecker`.  `ModuleData.constants` is the authority: it is serialized
from the module-local stage-2 constant map and is not reconstructed from the
lossy, merged `Environment.const2ModIdx` table.

For a split Lean module, artifact parts are ordered exported, server, private.
Each level includes the preceding level.  The last (most-private) part is the
ownership manifest; the per-part manifests are visibility commitments.  For a
legacy/non-module artifact the exported part is also the ownership manifest.

`extraConstNames` is emitted only as structural telemetry.  It is IR/codegen
attribution and never enters a logical declaration manifest.
-/

private def nameFromString (s : String) : Name :=
  s.splitOn "." |>.foldl Name.str Name.anonymous

private def moduleForNode (node : String) : Name :=
  nameFromString s!"Tablet.{node}"

/-- This match is intentionally exhaustive.  A new `ConstantInfo` constructor
must make the pinned adapter fail to compile instead of being silently skipped. -/
private def declarationKind : ConstantInfo → String
  | .thmInfo _    => "theorem"
  | .axiomInfo _  => "axiom"
  | .opaqueInfo _ => "opaque"
  | .defnInfo _   => "definition"
  | .inductInfo _ => "inductive"
  | .ctorInfo _   => "constructor"
  | .recInfo _    => "recursor"
  | .quotInfo _   => "quotient"

private structure ArtifactPart where
  level : String
  path  : System.FilePath
  data  : ModuleData

private def validateManifest (moduleName : Name) (level : String)
    (data : ModuleData) : IO Unit := do
  let derivedNames := data.constants.map (·.name)
  unless data.constNames == derivedNames do
    throw <| IO.userError s!"unsupported_toolchain: {moduleName} {level}: \
      ModuleData.constNames != constants.map ConstantInfo.name"
  let mut seen : Std.HashSet Name := {}
  for info in data.constants do
    if seen.contains info.name then
      throw <| IO.userError s!"unsupported_toolchain: {moduleName} {level}: \
        duplicate logical constant {info.name} in one ModuleData manifest"
    seen := seen.insert info.name
    -- Force the exhaustive adapter check for every entry while reading.
    let _ := declarationKind info

private def readArtifactParts (moduleName : Name) (validateDeclarations : Bool := true) : IO (Array ArtifactPart) := do
  let exportedPath ← findOLean moduleName
  unless (← exportedPath.pathExists) do
    throw <| IO.userError s!"unsupported_artifact: missing exported artifact {exportedPath}"
  let (exported, _) ← readModuleData exportedPath
  let serverPath := OLeanLevel.server.adjustFileName exportedPath
  let privatePath := OLeanLevel.private.adjustFileName exportedPath
  let serverExists ← serverPath.pathExists
  let privateExists ← privatePath.pathExists
  if !exported.isModule then
    if serverExists || privateExists then
      throw <| IO.userError s!"unsupported_artifact: legacy artifact {exportedPath} \
        unexpectedly has split-module parts"
    if validateDeclarations then validateManifest moduleName "exported" exported
    return #[{ level := "exported", path := exportedPath, data := exported }]
  if privateExists && !serverExists then
    throw <| IO.userError s!"unsupported_artifact: private part {privatePath} is unbound \
      because server part {serverPath} is absent"
  let mut paths := #[exportedPath]
  let mut levels := #["exported"]
  if serverExists then
    paths := paths.push serverPath
    levels := levels.push "server"
    if privateExists then
      paths := paths.push privatePath
      levels := levels.push "private"
  let loaded ← readModuleDataParts paths
  if loaded.size != paths.size then
    throw <| IO.userError s!"unsupported_artifact: artifact-part reader returned \
      {loaded.size} parts for {paths.size} paths"
  let mut result := #[]
  for h : i in [:loaded.size] do
    let data := loaded[i].1
    unless data.isModule do
      throw <| IO.userError s!"unsupported_artifact: split part {paths[i]!} \
        is not marked as module-system data"
    if validateDeclarations then validateManifest moduleName levels[i]! data
    result := result.push { level := levels[i]!, path := paths[i]!, data }
  return result

private def manifestJson (entries : Array ConstantInfo) : Array Json :=
  let sorted := entries.qsort (fun a b => toString a.name < toString b.name)
  sorted.map fun info => Json.mkObj [
    ("name", Json.str (toString info.name)),
    ("kind", Json.str (declarationKind info))
  ]

private def partJson (part : ArtifactPart) : Json :=
  Json.mkObj [
    ("level", Json.str part.level),
    ("path", Json.str part.path.toString),
    ("declarations", Json.arr (manifestJson part.data.constants)),
    ("extra_const_names", Json.arr <|
      part.data.extraConstNames.map (Json.str <| toString ·))
  ]

private def directImportsJson (imports : Array Import) : IO (Array Json) :=
  imports.mapM fun importSpec => do
    let oleanPath ← findOLean importSpec.module
    pure <| Json.mkObj [
      ("module", Json.str importSpec.module.toString),
      ("olean", Json.str oleanPath.toString),
      ("import_all", Json.bool importSpec.importAll),
      ("is_exported", Json.bool importSpec.isExported),
      ("is_meta", Json.bool importSpec.isMeta)
    ]

private def emitEnvironmentRow (emitted : IO.Ref Nat) (row : Json) : IO Unit := do
  let line := row.compress
  let total := (← emitted.get) + line.utf8ByteSize + 1
  if total > 32 * 1024 * 1024 then
    throw <| IO.userError "portable replay inventory exceeds 32 MiB"
  emitted.set total
  IO.println line

/-- Bounded, read-only inventory for portable replay. Emit no declarations and
retain no ModuleData after visiting a module. Package outputs are mutable in
the compiler sandbox, so traverse their actual imported graph too. Only the
pinned toolchain sysroot is an existing read-only trust boundary. -/
private def replayEnvironment (sysroot : System.FilePath) (nodes : List String) : IO Unit := do
  let mut pending := nodes.toArray.map moduleForNode
  let mut seen : Std.HashSet Name := {}
  let sysrootPrefix := (sysroot / "lib" / "lean").toString ++ "/"
  let emitted ← IO.mkRef 0
  emitEnvironmentRow emitted (Json.mkObj [
    ("environment_header", Json.bool true),
    ("sysroot", Json.str sysroot.toString),
    ("path", Json.str ((← IO.getEnv "PATH").getD "")),
    ("search_path", Json.arr <| (← searchPathRef.get).toArray.map (Json.str ∘ System.FilePath.toString))
  ])
  while !pending.isEmpty do
    let moduleName := pending.back!
    pending := pending.pop
    if seen.contains moduleName then continue
    seen := seen.insert moduleName
    if seen.size > 32768 then
      throw <| IO.userError "portable replay import inventory exceeds 32768 modules"
    let path ← findOLean moduleName
    if path.toString.startsWith sysrootPrefix then
      emitEnvironmentRow emitted (Json.mkObj [
        ("module", Json.str moduleName.toString),
        ("sysroot_import", Json.str path.toString)
      ])
    else
      -- This auxiliary inventory needs imports and complete byte bindings,
      -- not another traversal of every external library's constant table.
      -- The ordinary owner manifest path below retains its full validation.
      let parts ← readArtifactParts moduleName false
      let mut imports : Array Json := #[]
      for part in parts do
        if part.data.imports.size > 32768 then
          throw <| IO.userError "portable replay module import count exceeds 32768"
        imports := imports ++ (← directImportsJson part.data.imports)
        for entry in part.data.imports do
          pending := pending.push entry.module
      emitEnvironmentRow emitted (Json.mkObj [
        ("module", Json.str moduleName.toString),
        ("parts", Json.arr <| parts.map fun part => Json.mkObj [
          ("level", Json.str part.level), ("path", Json.str part.path.toString)]),
        ("imports", Json.arr imports)
      ])

def main (args : List String) : IO UInt32 := do
  let sysroot ← findSysroot
  initSearchPath sysroot
  if args.isEmpty then
    IO.eprintln "no Tablet node names provided"
    return (2 : UInt32)
  try
    if args.head? == some "--replay-environment" then
      replayEnvironment sysroot args.tail!
      return (0 : UInt32)
    for node in args do
      let moduleName := moduleForNode node
      let parts ← readArtifactParts moduleName
      if h : parts.size = 0 then
        throw <| IO.userError s!"unsupported_artifact: {moduleName} has no artifact parts"
      else
        let ownership := parts[parts.size - 1]
        let directImports ← directImportsJson ownership.data.imports
        IO.println (Json.mkObj [
          ("adapter", Json.str "lean-4.33-module-data-v2"),
          ("node", Json.str node),
          ("artifact_format", Json.str <|
            if ownership.data.isModule then "split-module-data" else "legacy-module-data"),
          -- Compatibility name for readers while v2 rolls out.  This is the
          -- authoritative ownership manifest, not merged attribution.
          ("declarations", Json.arr (manifestJson ownership.data.constants)),
          ("ownership_manifest", Json.arr (manifestJson ownership.data.constants)),
          ("artifact_parts", Json.arr (parts.map partJson)),
          ("direct_imports", Json.arr directImports),
          ("sysroot", Json.str sysroot.toString)
        ]).compress
    return (0 : UInt32)
  catch error =>
    IO.eprintln s!"module manifest capture failed: {error.toString}"
    return (1 : UInt32)

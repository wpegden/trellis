-- Library root for the local_closure_smoke fixture.
-- Lake expects this file to exist alongside the `Tablet/` directory
-- when `lean_lib «Tablet»` is declared. It re-exports each fixture
-- node so `import Tablet` pulls them all in; individual tests use
-- `import Tablet.X` for one-node imports.

import Tablet.Preamble
import Tablet.Helper
import Tablet.Closed
import Tablet.UsesHelper
import Tablet.ActiveSorry
import Tablet.InductiveNat
import Tablet.UsesInductive
import Tablet.ReservedArtifactDef
import Tablet.UsesReservedArtifact
import Tablet.StructFields
import Tablet.UsesStructField
import Tablet.EnumColor
import Tablet.UsesEnumCtor
import Tablet.ClassMethod
import Tablet.UsesClassMethod
import Tablet.DiamondChild
import Tablet.UsesDiamondParent
import Tablet.Owner
import Tablet.OwnerAux
import Tablet.UsesOwnerAux
import Tablet.OwnerToCtorIdxAux
import Tablet.UsesOwnerToCtorIdx
import Tablet.InstanceValue
import Tablet.UsesInstanceValue
import Tablet.FieldTyped
import Tablet.UsesFieldTyped
-- FIX 1 (reserved-shaped axiom transparent-walk) regression fixtures.
import Tablet.AxiomForge
import Tablet.UsesAxiomForge
-- FIX 2 (owner-file reserved-shaped authored-name) forge fixtures, one per
-- authoring surface, each rejected at its OWN acceptance.
import Tablet.ForgeBareEq
import Tablet.ForgeProtectedEq
import Tablet.ForgePrivateEq
import Tablet.ForgeUnderscore
import Tablet.ForgeNamespace
import Tablet.ForgeWhere
-- FIX 2 coverage: Definition-kind authoring surfaces (the residual the full
-- closure probe never scans). Each rejected at its OWN acceptance by the
-- universal `--scan-only` owner-file gate, regardless of node kind.
import Tablet.DefForgeLetRec
import Tablet.StructForgeAux
import Tablet.ClassForgeAux
import Tablet.UsesDefForgeWhere
-- FIX 2 no-false-positive fixtures (genuine generated internals; legit aux).
import Tablet.RecDef
import Tablet.UsesRecDef
import Tablet.LegitAux
-- FIX 2 parse-robustness fixtures (codex "additional risk" about the
-- scan-only `Init`-only parse): a forged reserved-shaped auxiliary AFTER a
-- declaration that uses imported notation, and after a custom command macro.
-- The libraries declaring the notation/command are imported first.
import Tablet.NotationLib
import Tablet.ForgeAfterNotation
import Tablet.CommandLib
import Tablet.ForgeAfterCommand
-- FIX 3 (forbid macro/syntax/elaborator-defining commands in node files):
-- a macro-generated reserved-shaped declaration (the residual) plus a
-- consumer, plain `macro`/`elab` commands (banned by family), and a
-- legit `notation`/`infix` node that must still pass (term-level, allowed).
import Tablet.MacroEqForge
import Tablet.UsesMacroEqForge
import Tablet.PlainMacro
import Tablet.PlainElab
import Tablet.LegitNotation
-- NAMESPACED-ROOT FIX regression: a PV-shape node whose root theorem is
-- declared under a `namespace` opened in the free region above the marker.
-- The probe must resolve the namespaced on-disk decl by the unique
-- same-final-name match in the node's own module (not the bare node name).
import Tablet.NamespacedRoot
-- NAMESPACED-DEP FIX regression: a NAMESPACED consumer node that depends on
-- OTHER NAMESPACED nodes, exercising the kernel's Patch C-K present-node
-- validation end to end. The probe must emit each cross-node dep's BARE node
-- id (its `Tablet.`-stripped module suffix), NOT the namespaced on-disk
-- `Name`, so `strict_definition_deps` / `strict_theorem_deps` /
-- `boundary_theorems` keys map to ratified present_nodes.
import Tablet.NamespacedThm
import Tablet.NamespacedStrictThm
import Tablet.NamespacedDef
import Tablet.NamespacedConsumer
-- NAMESPACED-FORGE soundness fixtures (dep-fix principal-declaration guard):
-- a namespaced node that authors a namespaced private auxiliary, plus a
-- consumer in another module that depends on it. The aux's dep key must stay
-- a dotted `Name` (NOT collapsed to the bare present-node id), so the kernel
-- C-K guard still rejects it fail-closed.
import Tablet.NamespacedForgeAux
import Tablet.UsesNamespacedForgeAux
-- DEEPLY-NAMESPACED-DEP regression (the dec2flt-shape `declMatchesStem` fix):
-- a node whose FILESPEC stem FLATTENS a multi-segment below-namespace path
-- (decl `crate_ns.Nested.method` in module `Tablet.Nested_method`), plus a
-- consumer. Both the root resolver and the dep-name canonicalizer must
-- recognize the principal via the FILESPEC name-parity test, not a bare
-- final-component match.
import Tablet.Nested_method
import Tablet.UsesNestedMethod
-- DEEPLY-NAMESPACED-FORGE soundness regression: the same flattened-stem shape
-- but the cross-node dep is a hand-authored namespaced auxiliary
-- (`crate_ns.Nested.Forge.realAux`) whose runs never flatten to the stem, so
-- it must stay a dotted `Name` and be rejected fail-closed.
import Tablet.Nested_Forge
import Tablet.UsesNestedForge
-- AMBIGUOUS-ROOT fixture (root-fix `ambiguous_declaration` coverage): two
-- same-final-name non-generated decls in one node's own module ⇒ the resolver
-- must over-reject with `ambiguous_declaration`, never an arbitrary pick.
import Tablet.AmbiguousRoot
-- DEFINITION-DEP MODULE-ATTRIBUTION regression (the dec2flt `strict_definition_deps`
-- fix): definition deps must map to their DECLARING-MODULE node id with NO
-- principal `declMatchesStem` gate. Two categories: (a) a `structure` declared
-- in the shared `Tablet.Preamble` module (node id `Preamble`) referenced by a
-- consumer's def, and (b) a non-principal Aeneas-shape co-generated `_loop`
-- helper living inside a model node's own module (node id `LoopHost`). Pre-fix
-- both emitted raw dotted names that Patch C-K fail-closed; post-fix they emit
-- the bare host-node ids `Preamble` / `LoopHost`.
import Tablet.LoopHost
import Tablet.UsesPreambleStruct
import Tablet.UsesLoopHelper

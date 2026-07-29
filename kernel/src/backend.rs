//! Proof-assistant backend seam (Phase I, behavior-preserving).
//!
//! Today there is exactly one backend — Lean — so `BackendId` is a closed
//! single-variant enum and `SourceModel` is a single-variant wrapper over a
//! zero-sized `LeanSourceModel`. The module exists to give the kernel's
//! pure (no-I/O) text→facts operations a backend dimension *without changing
//! any Lean behavior*: every method here delegates 1:1 to the existing
//! production function it wraps.
//!
//! Dispatch is a hand-written `match self`, matching the kernel idiom
//! (`git grep "Box<dyn"` over `kernel/src/*.rs` = zero hits; all trait usage
//! is generic-bound static dispatch). Monomorphic call sites.
//!
//! ## Why the constants are *aliased*, not duplicated
//!
//! `approved_axioms_floor` POINTS AT [`crate::model::CANONICAL_APPROVED_AXIOMS`]
//! (the single source of truth for the kernel-axiom floor — see that const's
//! doc). `forbidden_keywords` and `allowed_import_prefixes` are the canonical
//! home for the Lean lexical policy; `runtime_cli_observations.rs` aliases
//! *these* (mirroring how `DEFAULT_APPROVED_AXIOMS` already aliases
//! `CANONICAL_APPROVED_AXIOMS`). No value is duplicated, so no drift is
//! possible; `tests::descriptor_constants_match_legacy_literals` pins the
//! relationship.

use crate::model::{NodeId, NodeKind};
use serde::{Deserialize, Serialize};

/// The Lean lexical policy: declaration-construct keywords an ordinary
/// Tablet node file may never contain. **Copied verbatim** from the
/// historical `runtime_cli_observations.rs::FORBIDDEN_KEYWORDS` (16
/// entries). The order is load-bearing for any caller that reports the
/// first hit, so it is preserved exactly; `runtime_cli_observations.rs`
/// now aliases this constant.
pub const LEAN_FORBIDDEN_KEYWORDS: &[&str] = &[
    "sorryAx",
    "sorry",
    "axiom",
    "constant",
    "unsafe",
    "opaque",
    "partial",
    "native_decide",
    "implementedBy",
    "implemented_by",
    "extern",
    "elab",
    "macro",
    "syntax",
    "run_cmd",
    "#eval",
];

/// Legal import-root prefixes for a Lean tablet node. Canonical home;
/// `runtime_cli_observations.rs` aliases this.
pub const LEAN_ALLOWED_IMPORT_PREFIXES: &[&str] = &["Mathlib"];

/// The Isabelle/HOL lexical policy: outer-syntax COMMAND keywords an ordinary
/// Tablet node `.thy` may never contain. These are the command-defining / ML /
/// oracle / axiom surfaces — the analogue of the Lean macro/`axiom` ban — that
/// (a) can synthesize a fact/name never appearing literally in source, (b)
/// extend the keyword table (defeating the static-table outer-token scan), or
/// (c) introduce out-of-band axioms/oracles into the trust basis. Grounded in
/// `isabelle_port_research/03-file-format-hashing.md` (Part D — the `keywords`
/// / ML ban) and `04-soundness-oracle-auditing.md` (§2.3/§5.4 — the oracle /
/// `axiomatization` surface).
///
/// `sorry` is DELIBERATELY ABSENT: it is the OPEN-proof marker (the analogue of
/// Lean `sorry`), handled by [`SourceModel::is_node_open`], not a forbidden
/// command — listing it here would reject every open node.
/// `tests::isabelle_hol_descriptor_constants` pins both `keywords`'s presence
/// and `sorry`'s absence.
pub const ISABELLE_HOL_FORBIDDEN_KEYWORDS: &[&str] = &[
    "oops",
    "axiomatization",
    "ML",
    "ML_file",
    "ML_command",
    "ML_val",
    "setup",
    "local_setup",
    "method_setup",
    "attribute_setup",
    "parse_translation",
    "parse_ast_translation",
    "print_translation",
    "typed_print_translation",
    "oracle",
    "syntax",
    "nonterminal",
    "keywords",
    "Thm.add_oracle",
    // S4 — codegen-trusting proof METHODS (`apply (eval)` / `by normalization`
    // / `by code_simp`). They reduce the goal through the trusted code
    // generator (the Feb-2025 NBE prove-False bug), so a `False`-ish result can
    // surface with NO named oracle in the cert. These are method tokens, not
    // outer commands, but `validate_node_shape`'s forbidden scan classifies
    // every `Ident` token, so listing them here rejects them in method position
    // too (R3). `code_simp` is the simp-with-code-equations variant.
    "eval",
    "normalization",
    "code_simp",
];

/// Legal import-root prefixes for an Isabelle/HOL tablet node. The precise
/// session roots (`HOL-Analysis`, AFP entries, …) are finalized with the
/// scaffold/session pin (checker increment); `HOL` and `Main` are the
/// universally-present HOL roots.
pub const ISABELLE_HOL_ALLOWED_IMPORT_PREFIXES: &[&str] = &["HOL", "Main"];

/// The Isabelle/HOL **foundational trusted base** — the object-logic kernel
/// axioms `HOL` (and the nat construction / Hilbert choice) asserts on top of
/// Pure. Dump-confirmed against the installed Isabelle2025-2 HOL heap
/// (`Theory.all_axioms_of`; recorded in `isabelle_install_notes.md` Appendix B).
/// Each name is a real `axiom` in the live `HOL`/`Hilbert_Choice`/`Nat`
/// theories (verified PRESENT), so this is an honest, install-grounded set —
/// the analogue of Lean's [`crate::model::CANONICAL_APPROVED_AXIOMS`].
///
/// **Soundness-model note.** For Isabelle the PRIMARY mechanical soundness gate
/// is the ORACLE gate `oracles ⊆ approved_oracles_floor` (= ∅: no
/// `Pure.skip_proof`/`sorry`, no `smt`/external-solver oracle) — read from the
/// kernel's own `Thm_Deps.all_oracles` derivation record
/// (`04-soundness-oracle-auditing.md` §2.2/§4.2). This axiom floor is the
/// FOUNDATIONAL trusted base — a secondary/sanity bound on the trusted basis,
/// not the cheat detector.
///
/// **Scope (deliberately the foundational base, not the full closure).** The
/// broader `thm_deps` closure of a real proof also reaches Pure inference
/// axioms and *definitional* axioms (`definition`/`fun`/`typedef` `*_def` /
/// `type_definition_*`; the full `Main` axiom inventory is ~3451 — see
/// Appendix B). Whether the accept-gate bounds `kernel_axioms` against the bare
/// foundational base, against an allow-prefixed definitional superset, or only
/// runs the oracle gate is a **B2c-gate** decision and is intentionally NOT
/// baked into this descriptor constant.
///
/// **Dump corrections vs the source-read seed** (`isabelle_install_notes.md`
/// Appendix B): `HOL.iff` is a *derived theorem*, not an axiom (omitted; the
/// axiomatic propositional surface is `HOL.True_or_False`). `Nat.Zero_Rep` /
/// `Nat.Suc_Rep` are *constants*, not axioms; the foundational nat axioms are
/// `Nat.Suc_Rep_inject` + `Nat.Suc_Rep_not_Zero_Rep`.
pub const ISABELLE_HOL_APPROVED_AXIOMS_FLOOR: &[&str] = &[
    // HOL object-logic primitive axioms (theory `HOL.HOL`).
    "HOL.refl",
    "HOL.subst",
    "HOL.ext",
    "HOL.the_eq_trivial",
    "HOL.impI",
    "HOL.mp",
    "HOL.True_or_False",
    "HOL.eq_reflection",
    "HOL.fun_arity",
    "HOL.itself_arity",
    // Hilbert choice (theory `HOL.Hilbert_Choice`).
    "Hilbert_Choice.someI",
    // Foundational nat construction (theory `HOL.Nat`).
    "Nat.Suc_Rep_inject",
    "Nat.Suc_Rep_not_Zero_Rep",
];

/// The Isabelle/HOL approved **oracle** floor — EMPTY by policy and the PRIMARY
/// mechanical soundness gate's allow-set. No oracle is ever approvable:
/// `Pure.skip_proof` (the `sorry` / `\<proof>` cheat oracle) NEVER, and no
/// external-solver oracle (`smt`/`z3`/`cvc4`/…) either. The dump confirms a
/// clean proof yields an EMPTY `Thm_Deps.all_oracles` set, and — critically
/// (R3) — that type-class/arity reasoning does NOT surface as a named oracle
/// (`isabelle_install_notes.md` Appendix B), so the empty floor does not block
/// legitimate HOL proofs. Consumed by the B2c-gate accept clause (not yet).
pub const ISABELLE_HOL_APPROVED_ORACLES_FLOOR: &[&str] = &[];

/// The Lean approved **oracle** floor — EMPTY. Lean's mechanical soundness
/// model is axiom-closure (`sorryAx` ∈ closure ⇒ reject), not oracle-based, so
/// Lean has no oracle concept; the field exists only so the cross-backend
/// `approved_oracles_floor` is total. Nothing on the Lean path reads it, and
/// the B2c-gate's oracle clause is vacuous for Lean (empty `oracles_used` ⊆
/// empty floor), keeping Lean acceptance byte-identical.
pub const LEAN_APPROVED_ORACLES_FLOOR: &[&str] = &[];

/// Closed set of proof-assistant backends. One source of truth for the
/// backend axis. Two variants today (Lean, Isabelle/HOL); Isabelle/ZF is
/// deferred to a later phase.
///
/// Serializes `snake_case`, so `Lean` ⟷ `"lean"` and `IsabelleHol` ⟷
/// `"isabelle_hol"` (the documented config default and the step-7/12 checker
/// `target` wire strings). The wire strings are load-bearing and pinned by
/// `tests::backend_id_serializes_as_lean` /
/// `tests::backend_id_isabelle_hol_serializes_as_isabelle_hol`.
///
/// `Default = Lean` so the `#[serde(default)]` `ProtocolState.tablet_target`
/// field fills as `Lean` for on-disk checkpoints predating the field.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BackendId {
    #[default]
    Lean,
    IsabelleHol,
}

/// Static, data-only description a backend advertises. No I/O. Built once
/// as a `'static` via [`lean_descriptor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendDescriptor {
    /// The backend this descriptor describes.
    pub id: BackendId,
    /// File extension of a node-source file (`"lean"`), without the dot.
    pub node_source_ext: &'static str,
    /// Repo-relative path of the umbrella import file (`"Tablet.lean"`).
    pub umbrella_relpath: &'static str,
    /// The approved kernel-axiom floor. POINTS AT the canonical constant;
    /// per-node `APPROVED_AXIOMS.json` may widen this set, never narrow it.
    pub approved_axioms_floor: &'static [&'static str],
    /// The approved **oracle** floor (the Isabelle soundness model's PRIMARY
    /// gate allow-set). EMPTY for both backends: Lean has no oracle concept
    /// (axiom-closure model), and Isabelle policy is "no oracle ever approvable"
    /// (`Pure.skip_proof`/`smt` rejected). Additive; consumed by the B2c-gate
    /// accept clause (`oracles_used ⊆ approved_oracles_floor`), not yet by
    /// anything — so it is byte-inert today, vacuous for Lean.
    pub approved_oracles_floor: &'static [&'static str],
    /// Banned declaration-construct keywords (the Lean lexical policy).
    pub forbidden_keywords: &'static [&'static str],
    /// Legal import-root prefixes.
    pub allowed_import_prefixes: &'static [&'static str],
}

/// The single Lean backend descriptor. `const fn` so it is usable in
/// const contexts and carries no runtime cost.
pub const fn lean_descriptor() -> &'static BackendDescriptor {
    &BackendDescriptor {
        id: BackendId::Lean,
        node_source_ext: "lean",
        umbrella_relpath: "Tablet.lean",
        approved_axioms_floor: crate::model::CANONICAL_APPROVED_AXIOMS,
        approved_oracles_floor: LEAN_APPROVED_ORACLES_FLOOR,
        forbidden_keywords: LEAN_FORBIDDEN_KEYWORDS,
        allowed_import_prefixes: LEAN_ALLOWED_IMPORT_PREFIXES,
    }
}

/// The Isabelle/HOL backend descriptor. `const fn`, mirroring
/// [`lean_descriptor`]. The `umbrella_relpath` is the placeholder `"ROOT"`
/// (the Isabelle session `ROOT` file) — finalized in the checker/scaffold
/// increment; never read on the Lean path. `approved_axioms_floor` is the
/// dump-confirmed foundational HOL base (see
/// [`ISABELLE_HOL_APPROVED_AXIOMS_FLOOR`]); `approved_oracles_floor` is EMPTY
/// (the PRIMARY soundness gate's allow-set — no oracle approvable).
pub const fn isabelle_hol_descriptor() -> &'static BackendDescriptor {
    &BackendDescriptor {
        id: BackendId::IsabelleHol,
        node_source_ext: "thy",
        // ROOT: placeholder; finalized in the checker/scaffold increment.
        umbrella_relpath: "ROOT",
        approved_axioms_floor: ISABELLE_HOL_APPROVED_AXIOMS_FLOOR,
        approved_oracles_floor: ISABELLE_HOL_APPROVED_ORACLES_FLOOR,
        forbidden_keywords: ISABELLE_HOL_FORBIDDEN_KEYWORDS,
        allowed_import_prefixes: ISABELLE_HOL_ALLOWED_IMPORT_PREFIXES,
    }
}

/// Zero-sized Lean realization of the pure source-model face. Its methods
/// delegate 1:1 to the existing production functions in `filespec_split`,
/// `filespec`, and `worker_normalization`; introducing it changes no Lean
/// behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeanSourceModel;

/// Zero-sized Isabelle/HOL realization of the pure source-model face. Its
/// methods delegate to [`crate::isabelle_filespec`] (the `.thy` outer-syntax
/// splitter); `classify_declaration` delegates to the SAME backend-agnostic
/// `.tex` classifier as the Lean arm. In increment 1 NONE of these methods is
/// ever called on the live Lean path (every `IsabelleHol` arm is statically
/// unreachable there); they are exercised only by the inline `.thy` unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsabelleSourceModel;

/// The pure (no-I/O) text→facts face, dispatched per backend. Methods
/// `match self` and delegate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceModel {
    Lean(LeanSourceModel),
    IsabelleHol(IsabelleSourceModel),
}

/// The Lean source model. Zero-sized; instantiate it directly at a call
/// site (`backend::lean_source_model()`), do not thread it as a parameter
/// (threading the active model through public signatures is a later phase).
pub const fn lean_source_model() -> SourceModel {
    SourceModel::Lean(LeanSourceModel)
}

/// The Isabelle/HOL source model. Zero-sized; instantiate it directly at a
/// call site (`backend::isabelle_source_model()`).
pub const fn isabelle_source_model() -> SourceModel {
    SourceModel::IsabelleHol(IsabelleSourceModel)
}

impl SourceModel {
    /// The static descriptor for this source model's backend.
    pub const fn descriptor(&self) -> &'static BackendDescriptor {
        match self {
            SourceModel::Lean(_) => lean_descriptor(),
            SourceModel::IsabelleHol(_) => isabelle_hol_descriptor(),
        }
    }

    /// The backend id for this source model.
    pub const fn id(&self) -> BackendId {
        match self {
            SourceModel::Lean(_) => BackendId::Lean,
            SourceModel::IsabelleHol(_) => BackendId::IsabelleHol,
        }
    }

    /// Split one node's source into its FILESPEC regions. Lean: the
    /// `-- BODY` marker scan ([`crate::filespec_split::split`]). 1:1 wrap;
    /// `node` is forwarded unchanged (the Lean splitter ignores it, per its
    /// own doc, but the parameter is preserved for backend symmetry).
    pub fn split(
        &self,
        source: &str,
        node: &str,
    ) -> Result<crate::filespec_split::FilespecSplit, String> {
        match self {
            SourceModel::Lean(_) => crate::filespec_split::split(source, node),
            SourceModel::IsabelleHol(_) => crate::isabelle_filespec::split(source, node),
        }
    }

    /// The Tier-1 signature hash: the SHA-256 over the normalized
    /// declaration-signature region. Lean:
    /// [`crate::filespec_split::declaration_hash_strict`]. 1:1 wrap,
    /// including the `repo_path` parameter (unused by the Lean text scan but
    /// kept in the signature it wraps).
    pub fn signature_hash(
        &self,
        repo_path: &std::path::Path,
        source: &str,
        node: &str,
    ) -> Result<String, String> {
        match self {
            SourceModel::Lean(_) => {
                crate::filespec_split::declaration_hash_strict(repo_path, source, node)
            }
            SourceModel::IsabelleHol(_) => {
                crate::isabelle_filespec::signature_hash(repo_path, source, node)
            }
        }
    }

    /// Validate a node-file's declaration shape, returning human-readable
    /// errors (empty = valid). Lean:
    /// [`crate::filespec::validate_lean_node_shape`]. 1:1 wrap.
    pub fn validate_node_shape(&self, source: &str, node: &str) -> Vec<String> {
        match self {
            SourceModel::Lean(_) => crate::filespec::validate_lean_node_shape(source, node),
            SourceModel::IsabelleHol(_) => {
                crate::isabelle_filespec::validate_node_shape(source, node)
            }
        }
    }

    /// Classify a node's kind ({Preamble, Definition, Proof}) from its name
    /// + `.tex` content. Lean: delegates to
    /// [`crate::worker_normalization::classify_node_kind_from_tex`], which
    /// keys off the **naive (B)** `tex_statement_environment` scan — NOT the
    /// nesting-aware filespec (A) one. This divergence is intentional and
    /// pinned by the step-3 A-vs-B test.
    pub fn classify_declaration(&self, node: &str, tex_content: &str) -> NodeKind {
        match self {
            // The `.tex` NodeKind is backend-agnostic (it keys off the LaTeX
            // statement environment, not the proof-assistant source), so BOTH
            // arms delegate to the SAME classifier.
            SourceModel::Lean(_) | SourceModel::IsabelleHol(_) => {
                crate::worker_normalization::classify_node_kind_from_tex(node, tex_content)
            }
        }
    }

    /// Classify the node's principal SOURCE declaration as `"definition"` /
    /// `"theorem_like"` / `""` (not found), keyed off the proof-assistant
    /// source — NOT the `.tex` (that is [`Self::classify_declaration`]). Parallel
    /// to `classify_declaration`; consumed by the node-eval declaration-kind ↔
    /// tex-environment consistency checks. Lean: the declaration-head scan
    /// (`filespec::declaration_heads` ⇒ `is_definition_kind` ? "definition" :
    /// "theorem_like"), byte-identical to `runtime_cli_observations::declaration_kind`.
    /// Isabelle: the principal-command keyword class
    /// ([`crate::isabelle_filespec::declaration_kind`]).
    pub fn declaration_kind(&self, source: &str, node: &str) -> String {
        match self {
            SourceModel::Lean(_) => {
                for head in crate::filespec::declaration_heads(source) {
                    if head.name == node {
                        if crate::filespec::is_definition_kind(&head.kind) {
                            return "definition".to_string();
                        }
                        return "theorem_like".to_string();
                    }
                }
                String::new()
            }
            SourceModel::IsabelleHol(_) => {
                crate::isabelle_filespec::declaration_kind(source, node)
            }
        }
    }

    /// Cheap textual "is this node still open?" — scans the source for a
    /// live `sorry` (no prover). Lean: the WHOLE
    /// [`crate::worker_normalization::has_sorry`], including comment/string
    /// masking and the `macro_rules`-sorry-rewrite unmasking. Feeds
    /// `open_nodes_from_repo` → the contract's `openNodes` → phase advance.
    pub fn is_node_open(&self, source: &str) -> bool {
        match self {
            SourceModel::Lean(_) => crate::worker_normalization::has_sorry(source),
            SourceModel::IsabelleHol(_) => crate::isabelle_filespec::is_node_open(source),
        }
    }

    /// Intra-tablet import edges (`import Tablet.X`) parsed from source
    /// text. Lean: [`crate::worker_normalization::extract_tablet_imports`].
    /// 1:1 wrap (the `dep != node` self-filter stays at the caller).
    pub fn tablet_imports(&self, source: &str) -> Vec<NodeId> {
        match self {
            SourceModel::Lean(_) => crate::worker_normalization::extract_tablet_imports(source),
            SourceModel::IsabelleHol(_) => crate::isabelle_filespec::tablet_imports(source),
        }
    }
}

// NOTE: the source-model text→facts methods (`split`, `signature_hash`,
// `validate_node_shape`, `classify_declaration`, `is_node_open`,
// `tablet_imports`) are added in Phase I steps 2–4, each wrapping its
// production function 1:1.

// ---------------------------------------------------------------------------
// CheckerDriver — the effectful-op (check.py CLI subcommand) seam (Phase III)
// ---------------------------------------------------------------------------
//
// The kernel never speaks the checker socket protocol; for every effectful
// op it shells out `python3 <repo>/.trellis/scripts/check.py <SUBCOMMAND>
// <args>` via the two `run_repo_command_json` transport functions
// (`runtime_cli_observations.rs` for the OOM-attributing bin path,
// `tablet_support.rs` for the stdin-pipe path). The Rust-visible op
// vocabulary is therefore the *CLI-subcommand strings*, not the socket ops.
//
// `CheckerDriver` owns OP IDENTITY ONLY — the subcommand wire literal and the
// conditional arg-vec construction. It deliberately does NOT own the spawn:
// the two transports differ (cgroup-OOM attribution vs stdin EPIPE handling)
// and the raw `Command` machinery is untestable without a live checker.
// Owning it would force unifying the transports (a behavior change) and hide
// untestable code behind the abstraction. So the wrapper fns keep their body
// + transport call verbatim; they merely source the subcommand literal from
// `driver.<op>()` and the arg-vec from `driver.<args>()`. Wire-unchanged,
// Lean byte-identical.
//
// Single variant today (`Lean`); the seam is forward-compatible. A Rust-side
// `target` is inert with one variant — to mean anything it must cross
// `check.py` into the Python dispatch (pytest-gated, deferred), so no
// `target` parameter is added to any subcommand or to the transports.
//
// ## Why the op strings are single-sourced, not duplicated
//
// `LEAN_OP_*` are the canonical home for the check.py subcommand wire
// literals; the `<op>()` methods return them and the wrapper fns alias these
// methods (mirroring the Phase I "alias not duplicate" doctrine for
// `forbidden_keywords` / the axiom floor). Both the dispatched subcommand and
// the `external_command_from_value` / error-message subcommand strings source
// from the SAME `driver.<op>()`, so the dispatched string and its diagnostic
// can never desync. `tests::checker_driver_op_strings_match_legacy_literals`
// pins all seven verbatim.

/// `lean-compile-node`: compile one Tablet node, returning an
/// `ExternalCommandObservation`. Args `[node, repo]`.
pub const LEAN_OP_COMPILE_NODE: &str = "lean-compile-node";
/// `print-axioms`: print the kernel-axiom dependency set of one node.
/// Args `[node, repo]`.
pub const LEAN_OP_PRINT_AXIOMS: &str = "print-axioms";
/// `local-closure-axioms`: the local-closure probe (full walk, or
/// `--scan-only` for the owner-file authored-name scan). Arg `[node]`
/// (+ `--no-axcheck` when the cross-check is disabled, or `--scan-only`).
pub const LEAN_OP_LOCAL_CLOSURE_AXIOMS: &str = "local-closure-axioms";
/// `lean-semantic-payloads`: elaborated per-node semantic payloads.
/// Args `[repo, (--node N)*]`.
pub const LEAN_OP_LEAN_SEMANTIC_PAYLOADS: &str = "lean-semantic-payloads";
/// `materialize-tablet-oleans`: build `.olean`s for the requested nodes
/// (+ their transitive Tablet imports). Args `[repo, (--node N)*]`.
pub const LEAN_OP_MATERIALIZE_TABLET_OLEANS: &str = "materialize-tablet-oleans";
/// `prepare-compiled-support`: build the shared compiled support artefacts.
/// Args `[repo]`.
pub const LEAN_OP_PREPARE_COMPILED_SUPPORT: &str = "prepare-compiled-support";
/// `sync-tablet-support`: render + sync the tablet support files (INDEX /
/// README / header). Args `[repo, --render-json, -]` + the render payload
/// piped over stdin.
pub const LEAN_OP_SYNC_TABLET_SUPPORT: &str = "sync-tablet-support";

/// The `--no-axcheck` flag: appended to `local-closure-axioms` when the
/// bridge's `local_closure_axcheck_enabled` flag is `false` (skip the
/// secondary collector). Canonical home; the call-site keeps the
/// `axcheck_enabled` *decision*.
pub const LEAN_FLAG_NO_AXCHECK: &str = "--no-axcheck";
/// The `--scan-only` flag: runs `local-closure-axioms` in owner-file scan
/// mode (parse-only, no closure walk, no dep/axiom keys).
pub const LEAN_FLAG_SCAN_ONLY: &str = "--scan-only";
/// The `--node` flag: prefixes each per-node argument in the
/// `(--node N)*` arg lists (`lean-semantic-payloads`,
/// `materialize-tablet-oleans`).
pub const LEAN_FLAG_NODE: &str = "--node";

// The Isabelle check.py subcommand wire literals. `CheckerDriver` owns OP
// IDENTITY ONLY — these strings are the op vocabulary, NOT an invocation.
// FINALIZED in B2b: each literal matches a real `cli.py` subcommand parser
// (B2a) one-for-one — the names below are exactly the five
// `subparsers.add_parser(...)` strings the Python CLI dispatches to the
// AF_UNIX checker server (`client_isabelle_*`). The B2b per-node routing
// (`checker_driver_for(effective_node_target(node))`) sends an IsabelleHol
// node's ops through these; the seam stays byte-testable
// (`tests::isabelle_checker_driver_op_strings`).

/// `isabelle-check-node` — check one Tablet node (the `lean-compile-node`
/// analogue). Args `[node]` (server derives the repo from its socket root).
pub const ISABELLE_OP_CHECK_NODE: &str = "isabelle-check-node";
/// `isabelle-thm-oracles` — the transitive oracle set of one node theorem
/// (`Thm_Deps.all_oracles`; the `print-axioms` analogue). Args `[node]`.
pub const ISABELLE_OP_THM_ORACLES: &str = "isabelle-thm-oracles";
/// `isabelle-thm-deps` — the proof-dependency / axiom set of one node theorem
/// (the `local-closure-axioms` analogue). Args `[node]` (+ `--scan-only`,
/// which the Isabelle CLI parser accepts and ignores: the cert is
/// whole-theorem, so there is no parse-only mode — the second collector is
/// the `thm_oracles`/`thm_deps` pair, not an axiomatization walk).
pub const ISABELLE_OP_THM_DEPS: &str = "isabelle-thm-deps";
/// `isabelle-build-session` — build the per-tablet session heap (the
/// `prepare-compiled-support` / `materialize-tablet-oleans` analogue). The
/// CLI parser takes an OPTIONAL `node` (`nargs="?"`); the kernel passes
/// `[node]` to keep the existing arg-vec shape.
pub const ISABELLE_OP_BUILD_SESSION: &str = "isabelle-build-session";
/// `isabelle-sync-session` — render + write the session scaffold (the
/// `sync-tablet-support` analogue). Socket-only: the server derives the
/// session dir from its socket runtime root and renders the scaffold itself,
/// consuming NO stdin and NO `--render-json` (unlike the Lean
/// `sync-tablet-support`). The CLI parser takes an OPTIONAL `node`.
pub const ISABELLE_OP_SYNC_SESSION: &str = "isabelle-sync-session";

/// Zero-sized Lean realization of the effectful-op (check.py subcommand)
/// face. Reports the exact Lean wire subcommand for each op; it does NOT
/// own the spawn (see the module note above).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LeanCheckerDriver;

/// Zero-sized Isabelle/HOL realization of the effectful-op face. Reports the
/// `ISABELLE_OP_*` wire subcommands (B2a-finalized, matching the `cli.py`
/// parsers); like its Lean sibling it owns OP IDENTITY ONLY and never spawns.
/// Reachable only when a node's `effective_node_target` is `IsabelleHol`
/// (B2b routing); on the all-Lean live path the IsabelleHol arms are
/// statically unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsabelleCheckerDriver;

/// The effectful-op face, dispatched per backend. Methods `match self` and
/// return the wire subcommand / build the arg-vec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckerDriver {
    Lean(LeanCheckerDriver),
    IsabelleHol(IsabelleCheckerDriver),
}

/// The Lean checker driver. Zero-sized; instantiate it directly at a call
/// site (`backend::lean_checker_driver()`), do not thread it as a parameter
/// (threading the active driver through public signatures is a later phase).
pub const fn lean_checker_driver() -> CheckerDriver {
    CheckerDriver::Lean(LeanCheckerDriver)
}

/// The Isabelle/HOL checker driver. Zero-sized; instantiate it directly at a
/// call site (`backend::isabelle_checker_driver()`).
pub const fn isabelle_checker_driver() -> CheckerDriver {
    CheckerDriver::IsabelleHol(IsabelleCheckerDriver)
}

impl CheckerDriver {
    /// The backend id for this driver.
    pub const fn id(&self) -> BackendId {
        match self {
            CheckerDriver::Lean(_) => BackendId::Lean,
            CheckerDriver::IsabelleHol(_) => BackendId::IsabelleHol,
        }
    }

    /// `lean-compile-node` (compile one node). Isabelle:
    /// `isabelle-check-node`.
    pub const fn compile_node_op(&self) -> &'static str {
        match self {
            CheckerDriver::Lean(_) => LEAN_OP_COMPILE_NODE,
            CheckerDriver::IsabelleHol(_) => ISABELLE_OP_CHECK_NODE,
        }
    }

    /// `print-axioms` (kernel-axiom dependency set of one node). Isabelle:
    /// `isabelle-thm-oracles` (`Thm_Deps.all_oracles`).
    pub const fn print_axioms_op(&self) -> &'static str {
        match self {
            CheckerDriver::Lean(_) => LEAN_OP_PRINT_AXIOMS,
            CheckerDriver::IsabelleHol(_) => ISABELLE_OP_THM_ORACLES,
        }
    }

    /// `local-closure-axioms` (the local-closure probe; full or scan-only).
    /// Isabelle: `isabelle-thm-deps` (the proof-dependency / axiom set).
    pub const fn local_closure_op(&self) -> &'static str {
        match self {
            CheckerDriver::Lean(_) => LEAN_OP_LOCAL_CLOSURE_AXIOMS,
            CheckerDriver::IsabelleHol(_) => ISABELLE_OP_THM_DEPS,
        }
    }

    /// `lean-semantic-payloads` (elaborated per-node semantic payloads).
    /// Isabelle: `isabelle-thm-deps` (the dependency-graph data the Tier-2
    /// fingerprint will read).
    pub const fn lean_semantic_payloads_op(&self) -> &'static str {
        match self {
            CheckerDriver::Lean(_) => LEAN_OP_LEAN_SEMANTIC_PAYLOADS,
            CheckerDriver::IsabelleHol(_) => ISABELLE_OP_THM_DEPS,
        }
    }

    /// `materialize-tablet-oleans` (build `.olean`s for the requested nodes).
    /// Isabelle: `isabelle-build-session`.
    pub const fn materialize_oleans_op(&self) -> &'static str {
        match self {
            CheckerDriver::Lean(_) => LEAN_OP_MATERIALIZE_TABLET_OLEANS,
            CheckerDriver::IsabelleHol(_) => ISABELLE_OP_BUILD_SESSION,
        }
    }

    /// `prepare-compiled-support` (build shared compiled support artefacts).
    /// Isabelle: `isabelle-build-session`.
    pub const fn prepare_compiled_support_op(&self) -> &'static str {
        match self {
            CheckerDriver::Lean(_) => LEAN_OP_PREPARE_COMPILED_SUPPORT,
            CheckerDriver::IsabelleHol(_) => ISABELLE_OP_BUILD_SESSION,
        }
    }

    /// `sync-tablet-support` (render + sync the tablet support files).
    /// Isabelle: `isabelle-sync-session`.
    pub const fn sync_tablet_support_op(&self) -> &'static str {
        match self {
            CheckerDriver::Lean(_) => LEAN_OP_SYNC_TABLET_SUPPORT,
            CheckerDriver::IsabelleHol(_) => ISABELLE_OP_SYNC_SESSION,
        }
    }

    /// Build the `local-closure-axioms` *full-probe* arg-vec for `node`,
    /// reproducing the historical construction verbatim: `[node]`, plus
    /// `--no-axcheck` IFF `axcheck_enabled` is `false` (the bridge
    /// kill-switch skips the secondary collector). The `axcheck_enabled`
    /// decision stays at the call-site; this only reproduces the append.
    pub fn local_closure_args(&self, node: &str, axcheck_enabled: bool) -> Vec<String> {
        match self {
            CheckerDriver::Lean(_) => {
                let mut args: Vec<String> = vec![node.to_string()];
                if !axcheck_enabled {
                    args.push(LEAN_FLAG_NO_AXCHECK.to_string());
                }
                args
            }
            // Isabelle full-probe arg-vec is `[node]` (no `--no-axcheck`):
            // the Isabelle second collector is the `thm_oracles`/`thm_deps`
            // pair, NOT an axiomatization walk, so there is no `--no-axcheck`
            // kill-switch to append — `axcheck_enabled` is intentionally
            // unused. The server derives the repo from its socket root.
            CheckerDriver::IsabelleHol(_) => vec![node.to_string()],
        }
    }

    /// Build the `local-closure-axioms` *owner-scan* arg-vec for `node`:
    /// `[node, --scan-only]` (parse-only mode; `--no-axcheck` is irrelevant
    /// and omitted, matching the historical call-site).
    pub fn local_closure_scan_args(&self, node: &str) -> Vec<String> {
        match self {
            CheckerDriver::Lean(_) => {
                vec![node.to_string(), LEAN_FLAG_SCAN_ONLY.to_string()]
            }
            // Isabelle reuses the `[node, --scan-only]` owner-scan shape: the
            // `cli.py` `isabelle-thm-deps` parser accepts `--scan-only` and
            // ignores it (the cert is whole-theorem, so there is no parse-only
            // mode). The flag literal is shared with the Lean arm.
            CheckerDriver::IsabelleHol(_) => {
                vec![node.to_string(), LEAN_FLAG_SCAN_ONLY.to_string()]
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Target-keyed selectors (Phase IV step 11)
// ---------------------------------------------------------------------------
//
// `ProtocolState.tablet_target: BackendId` is the single source of truth for
// which backend a tablet targets. These selectors map a `BackendId` to the
// per-backend face it selects. Single exhaustive `Lean` arm today, delegating
// to the existing `lean_*()` builders, so they are byte-identical to a direct
// `lean_*()` call. `const fn` so the engine accept-gate const site
// (`engine.rs` `ENGINE_CANONICAL_APPROVED_AXIOMS`) can route through
// `descriptor_for(BackendId::Lean).approved_axioms_floor` and stay
// const-evaluable. Pass the `BackendId::Lean` literal at the leaf; per-node
// threading of the active target is a later phase.

/// The static descriptor for `t`'s backend. `Lean => lean_descriptor()`,
/// `IsabelleHol => isabelle_hol_descriptor()`.
pub const fn descriptor_for(t: BackendId) -> &'static BackendDescriptor {
    match t {
        BackendId::Lean => lean_descriptor(),
        BackendId::IsabelleHol => isabelle_hol_descriptor(),
    }
}

/// The pure (no-I/O) source-model face for `t`'s backend.
/// `Lean => lean_source_model()`, `IsabelleHol => isabelle_source_model()`.
pub const fn source_model_for(t: BackendId) -> SourceModel {
    match t {
        BackendId::Lean => lean_source_model(),
        BackendId::IsabelleHol => isabelle_source_model(),
    }
}

/// The effectful-op (check.py subcommand) face for `t`'s backend.
/// `Lean => lean_checker_driver()`,
/// `IsabelleHol => isabelle_checker_driver()`.
pub const fn checker_driver_for(t: BackendId) -> CheckerDriver {
    match t {
        BackendId::Lean => lean_checker_driver(),
        BackendId::IsabelleHol => isabelle_checker_driver(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The descriptor's constant *pointers* must resolve to the historical
    /// literal values, as sets and (for forbidden keywords, where order is
    /// load-bearing) as ordered sequences. This is the step-1 guard that the
    /// seam introduced no value drift.
    #[test]
    fn descriptor_constants_match_legacy_literals() {
        let d = lean_descriptor();

        assert_eq!(d.id, BackendId::Lean);
        assert_eq!(d.node_source_ext, "lean");
        assert_eq!(d.umbrella_relpath, "Tablet.lean");

        // Axiom floor: points at the canonical four. Compare as a set
        // (the floor's membership is what gates acceptance).
        let floor: std::collections::BTreeSet<&str> =
            d.approved_axioms_floor.iter().copied().collect();
        let canonical: std::collections::BTreeSet<&str> =
            crate::model::CANONICAL_APPROVED_AXIOMS.iter().copied().collect();
        assert_eq!(floor, canonical);
        assert_eq!(
            floor,
            ["Classical.choice", "Quot.sound", "funext", "propext"]
                .into_iter()
                .collect::<std::collections::BTreeSet<&str>>()
        );

        // Lean has no oracle concept (axiom-closure soundness model); the new
        // `approved_oracles_floor` field is EMPTY for Lean — pinned so the
        // B2c-gate oracle clause stays vacuous (Lean acceptance byte-identical).
        assert!(
            d.approved_oracles_floor.is_empty(),
            "Lean approved_oracles_floor must be empty, got {:?}",
            d.approved_oracles_floor
        );

        // Forbidden keywords: order is load-bearing (callers report the
        // first hit), so pin the exact 16-entry sequence verbatim.
        assert_eq!(
            d.forbidden_keywords,
            &[
                "sorryAx",
                "sorry",
                "axiom",
                "constant",
                "unsafe",
                "opaque",
                "partial",
                "native_decide",
                "implementedBy",
                "implemented_by",
                "extern",
                "elab",
                "macro",
                "syntax",
                "run_cmd",
                "#eval",
            ]
        );
        assert_eq!(d.forbidden_keywords.len(), 16);

        // Allowed import prefixes.
        assert_eq!(d.allowed_import_prefixes, &["Mathlib"]);
    }

    #[test]
    fn lean_source_model_is_zero_sized_and_reports_lean() {
        // The per-backend realization is zero-sized. (The `SourceModel` enum
        // itself is now 1 byte — a discriminant — since a second variant
        // `IsabelleHol` was added in Phase B1; the ZST is what carries no data.)
        assert_eq!(std::mem::size_of::<LeanSourceModel>(), 0);
        let m = lean_source_model();
        assert_eq!(m.id(), BackendId::Lean);
        assert_eq!(m.descriptor().id, BackendId::Lean);
    }

    #[test]
    fn checker_driver_is_zero_sized_and_reports_lean_ops() {
        // The per-backend realization is zero-sized (the enum is 1 byte after
        // the `IsabelleHol` variant was added — see the source-model test).
        assert_eq!(std::mem::size_of::<LeanCheckerDriver>(), 0);
        let d = lean_checker_driver();
        assert_eq!(d, CheckerDriver::Lean(LeanCheckerDriver));
        assert_eq!(d.id(), BackendId::Lean);
        // Every op resolves to a non-empty Lean wire subcommand.
        for op in [
            d.compile_node_op(),
            d.print_axioms_op(),
            d.local_closure_op(),
            d.lean_semantic_payloads_op(),
            d.materialize_oleans_op(),
            d.prepare_compiled_support_op(),
            d.sync_tablet_support_op(),
        ] {
            assert!(!op.is_empty());
        }
    }

    /// Pin every one of the seven check.py CLI subcommands the driver owns to
    /// its EXACT historical wire literal, verbatim — the Phase III byte-for-
    /// byte guard (mirrors `descriptor_constants_match_legacy_literals`). A
    /// change here would silently move the dispatched subcommand string and
    /// break the canned-response stub tests; the literal is the contract.
    #[test]
    fn checker_driver_op_strings_match_legacy_literals() {
        let d = lean_checker_driver();
        assert_eq!(d.compile_node_op(), "lean-compile-node");
        assert_eq!(d.print_axioms_op(), "print-axioms");
        assert_eq!(d.local_closure_op(), "local-closure-axioms");
        assert_eq!(d.lean_semantic_payloads_op(), "lean-semantic-payloads");
        assert_eq!(d.materialize_oleans_op(), "materialize-tablet-oleans");
        assert_eq!(d.prepare_compiled_support_op(), "prepare-compiled-support");
        assert_eq!(d.sync_tablet_support_op(), "sync-tablet-support");

        // The `LEAN_OP_*` consts are the single source the methods return.
        assert_eq!(d.compile_node_op(), LEAN_OP_COMPILE_NODE);
        assert_eq!(d.print_axioms_op(), LEAN_OP_PRINT_AXIOMS);
        assert_eq!(d.local_closure_op(), LEAN_OP_LOCAL_CLOSURE_AXIOMS);
        assert_eq!(d.lean_semantic_payloads_op(), LEAN_OP_LEAN_SEMANTIC_PAYLOADS);
        assert_eq!(d.materialize_oleans_op(), LEAN_OP_MATERIALIZE_TABLET_OLEANS);
        assert_eq!(d.prepare_compiled_support_op(), LEAN_OP_PREPARE_COMPILED_SUPPORT);
        assert_eq!(d.sync_tablet_support_op(), LEAN_OP_SYNC_TABLET_SUPPORT);

        // The flag literals.
        assert_eq!(LEAN_FLAG_NO_AXCHECK, "--no-axcheck");
        assert_eq!(LEAN_FLAG_SCAN_ONLY, "--scan-only");
        assert_eq!(LEAN_FLAG_NODE, "--node");
    }

    /// Pin the local-closure arg-vec construction to the historical call-site
    /// shape: full-probe `[node]` (axcheck on), `[node, --no-axcheck]`
    /// (axcheck off), and owner-scan `[node, --scan-only]`. Guards against
    /// arg-vec drift on the `--no-axcheck` / `--scan-only` conditionals.
    #[test]
    fn local_closure_args_reproduces_legacy_construction() {
        let d = lean_checker_driver();
        // Full probe, axcheck enabled (the default): just `[node]`.
        assert_eq!(d.local_closure_args("Foo", true), vec!["Foo".to_string()]);
        // Full probe, axcheck disabled: `[node, --no-axcheck]`.
        assert_eq!(
            d.local_closure_args("Foo", false),
            vec!["Foo".to_string(), "--no-axcheck".to_string()]
        );
        // Owner scan: `[node, --scan-only]`.
        assert_eq!(
            d.local_closure_scan_args("Foo"),
            vec!["Foo".to_string(), "--scan-only".to_string()]
        );
    }

    /// The target-keyed selectors must return the same faces a direct
    /// `lean_*()` call returns for `BackendId::Lean`. This is the step-11
    /// guard that routing the 18 seam sites through `*_for(Lean)` is
    /// byte-identical to the historical direct call.
    #[test]
    fn selectors_return_lean_impls_for_lean() {
        assert_eq!(descriptor_for(BackendId::Lean), lean_descriptor());
        assert_eq!(source_model_for(BackendId::Lean), lean_source_model());
        assert_eq!(checker_driver_for(BackendId::Lean), lean_checker_driver());
        // The const site routes through the descriptor selector; the floor
        // pointer must be the same canonical `&'static` it aliased before.
        assert_eq!(
            descriptor_for(BackendId::Lean).approved_axioms_floor,
            lean_descriptor().approved_axioms_floor
        );
        // B2c-floor behavior-preservation: routing through the selector, Lean's
        // axiom floor is UNCHANGED (the canonical four) and the new oracle floor
        // is EMPTY — the additive field is byte-inert for Lean.
        assert_eq!(
            descriptor_for(BackendId::Lean).approved_axioms_floor,
            crate::model::CANONICAL_APPROVED_AXIOMS
        );
        assert!(descriptor_for(BackendId::Lean)
            .approved_oracles_floor
            .is_empty());
    }

    /// R4 C4: the `SourceModel::declaration_kind` seam classifies the principal
    /// SOURCE declaration. The Lean arm (declaration-head ⇒ `is_definition_kind`)
    /// and the Isabelle arm (principal-command keyword class) must agree on the
    /// "definition" / "theorem_like" / "" verdicts for the corresponding source
    /// shapes — the parallel to `classify_declaration`.
    #[test]
    fn source_model_declaration_kind_lean_and_isabelle_parity() {
        let lean = lean_source_model();
        // Lean theorem ⇒ theorem_like; Lean def ⇒ definition; absent ⇒ "".
        assert_eq!(
            lean.declaration_kind("theorem Foo : True := by trivial\n", "Foo"),
            "theorem_like"
        );
        assert_eq!(
            lean.declaration_kind("def Foo : Nat := 0\n", "Foo"),
            "definition"
        );
        assert_eq!(lean.declaration_kind("theorem Bar : True := by trivial\n", "Foo"), "");

        let iso = isabelle_source_model();
        // Isabelle theorem-goal ⇒ theorem_like; thy_decl ⇒ definition; absent ⇒ "".
        assert_eq!(
            iso.declaration_kind(
                "theory Tablet_Foo\n  imports Main\nbegin\ntheorem Foo: \"P\"\n  by simp\nend\n",
                "Foo"
            ),
            "theorem_like"
        );
        assert_eq!(
            iso.declaration_kind(
                "theory Tablet_Foo\n  imports Main\nbegin\ndefinition Foo :: \"nat\" where \"Foo = 0\"\nend\n",
                "Foo"
            ),
            "definition"
        );
        assert_eq!(
            iso.declaration_kind(
                "theory Tablet_Foo\n  imports Main\nbegin\ntheorem Bar: \"P\"\n  by simp\nend\n",
                "Foo"
            ),
            ""
        );
    }

    /// The `BackendId` wire string is load-bearing (the step-7/12 checker
    /// `target` field keys off it). `Lean` MUST serialize as the lowercase
    /// `"lean"` (rename_all snake_case); default serde would emit `"Lean"`.
    /// Pin the exact string and the round-trip.
    #[test]
    fn backend_id_serializes_as_lean() {
        assert_eq!(serde_json::to_string(&BackendId::Lean).unwrap(), "\"lean\"");
        let parsed: BackendId = serde_json::from_str("\"lean\"").unwrap();
        assert_eq!(parsed, BackendId::Lean);
    }

    /// A default `ProtocolState` targets Lean. Every state constructor starts
    /// from `Default` (or seeds the config value, which is `Lean` absent the
    /// key), so the resume/back-compat guarantee is that the field is `Lean`.
    #[test]
    fn default_protocol_state_target_is_lean() {
        assert_eq!(
            crate::model::ProtocolState::default().tablet_target,
            BackendId::Lean
        );
    }

    // ---- Isabelle/HOL backend (Phase B1 increment 1, commit 1a) ----

    /// The new `IsabelleHol` variant MUST serialize as the snake_case
    /// `"isabelle_hol"` (the checker `target` wire string; `target_from_config`
    /// keys off it). Re-assert Lean⇒`"lean"` and `Default == Lean` here so the
    /// behavior-preservation invariants are pinned alongside the new variant.
    #[test]
    fn backend_id_isabelle_hol_serializes_as_isabelle_hol() {
        assert_eq!(
            serde_json::to_string(&BackendId::IsabelleHol).unwrap(),
            "\"isabelle_hol\""
        );
        let parsed: BackendId = serde_json::from_str("\"isabelle_hol\"").unwrap();
        assert_eq!(parsed, BackendId::IsabelleHol);

        // Behavior-preservation: Lean's wire string and the Default are
        // unchanged by adding the variant.
        assert_eq!(serde_json::to_string(&BackendId::Lean).unwrap(), "\"lean\"");
        assert_eq!(BackendId::default(), BackendId::Lean);
    }

    /// Pin the Isabelle/HOL descriptor constants: `thy` extension; the
    /// dump-confirmed foundational HOL axiom floor (NON-empty, containing
    /// `Hilbert_Choice.someI`/`HOL.ext`/`HOL.refl`); the EMPTY oracle floor (the
    /// PRIMARY soundness gate's allow-set); the forbidden-keyword policy
    /// CONTAINS the command-defining / ML / oracle / axiom surfaces
    /// (`oops`/`axiomatization`/`ML`/`setup`/`oracle`/`keywords`) and DOES NOT
    /// contain `sorry` (the open marker); the `HOL`/`Main` import prefixes.
    #[test]
    fn isabelle_hol_descriptor_constants() {
        let d = isabelle_hol_descriptor();

        assert_eq!(d.id, BackendId::IsabelleHol);
        assert_eq!(d.node_source_ext, "thy");
        assert_eq!(d.umbrella_relpath, "ROOT");

        // The axiom floor is now the dump-confirmed foundational HOL base —
        // NON-empty and containing the choice / extensionality / reflexivity
        // primitives (full set in `ISABELLE_HOL_APPROVED_AXIOMS_FLOOR`).
        let floor: std::collections::BTreeSet<&str> =
            d.approved_axioms_floor.iter().copied().collect();
        assert!(!floor.is_empty(), "HOL axiom floor is the foundational base");
        for ax in [
            "Hilbert_Choice.someI",
            "HOL.ext",
            "HOL.refl",
            "HOL.True_or_False",
            "Nat.Suc_Rep_inject",
        ] {
            assert!(
                floor.contains(ax),
                "HOL foundational floor must contain `{ax}`, got {:?}",
                d.approved_axioms_floor
            );
        }
        // Dump corrections: the non-axiom `HOL.iff` and the *constant* names
        // `Nat.Zero_Rep`/`Nat.Suc_Rep` are deliberately absent (Appendix B).
        assert!(!floor.contains("HOL.iff"));
        assert!(!floor.contains("Nat.Zero_Rep"));
        assert!(!floor.contains("Nat.Suc_Rep"));

        // The oracle floor is EMPTY by policy (no oracle ever approvable; the
        // PRIMARY mechanical soundness gate's allow-set).
        assert!(
            d.approved_oracles_floor.is_empty(),
            "HOL oracle floor must be empty by policy, got {:?}",
            d.approved_oracles_floor
        );

        // The banned COMMANDS must include the command-defining / ML / oracle /
        // axiom surfaces.
        let forbidden: std::collections::BTreeSet<&str> =
            d.forbidden_keywords.iter().copied().collect();
        for kw in [
            "oops",
            "axiomatization",
            "ML",
            "ML_file",
            "setup",
            "local_setup",
            "method_setup",
            "attribute_setup",
            "oracle",
            "syntax",
            "keywords",
            "Thm.add_oracle",
            // S4 codegen-trusting methods (B2c-gate Slice 3).
            "eval",
            "normalization",
            "code_simp",
        ] {
            assert!(
                forbidden.contains(kw),
                "Isabelle forbidden keywords must contain the banned command `{kw}`, got {:?}",
                d.forbidden_keywords
            );
        }

        // CRITICAL: `sorry` is the OPEN-proof marker (handled by is_node_open),
        // NOT a forbidden command — it must be ABSENT, or every open node would
        // be rejected. (`sorryAx`, a Lean-only name, is likewise absent.)
        assert!(
            !forbidden.contains("sorry"),
            "`sorry` is the open marker and must NOT be a forbidden command"
        );
        assert!(!forbidden.contains("sorryAx"));

        // Import prefixes: the HOL roots.
        assert_eq!(d.allowed_import_prefixes, &["HOL", "Main"]);
    }

    /// The target-keyed selectors return the Isabelle/HOL faces for
    /// `BackendId::IsabelleHol` (the seam's whole point: the variant routes to
    /// its own impls). Each per-backend realization is a ZST (`size_of == 0`);
    /// the wrapping enums are now 1 byte (a discriminant) since they carry two
    /// variants — no per-backend data is stored.
    #[test]
    fn selectors_return_isabelle_impls_for_isabelle_hol() {
        assert_eq!(
            descriptor_for(BackendId::IsabelleHol),
            isabelle_hol_descriptor()
        );
        assert_eq!(
            source_model_for(BackendId::IsabelleHol),
            isabelle_source_model()
        );
        assert_eq!(
            checker_driver_for(BackendId::IsabelleHol),
            isabelle_checker_driver()
        );

        // The source model / checker driver report the IsabelleHol id and the
        // Isabelle descriptor.
        let m = isabelle_source_model();
        assert_eq!(m.id(), BackendId::IsabelleHol);
        assert_eq!(m.descriptor().id, BackendId::IsabelleHol);
        assert_eq!(
            isabelle_checker_driver().id(),
            BackendId::IsabelleHol
        );

        // Each per-backend realization is a ZST; the enums hold no per-backend
        // data (1 byte for the discriminant now that there are two variants).
        assert_eq!(std::mem::size_of::<IsabelleSourceModel>(), 0);
        assert_eq!(std::mem::size_of::<IsabelleCheckerDriver>(), 0);
        assert_eq!(std::mem::size_of::<LeanSourceModel>(), 0);
        assert_eq!(std::mem::size_of::<LeanCheckerDriver>(), 0);
    }

    /// Pin the Isabelle checker-driver op strings (B2a-finalized — each
    /// matches a real `cli.py` subparser). This is the byte-for-byte op-IDENTITY
    /// guard (mirrors `checker_driver_op_strings_match_legacy_literals`): a
    /// change here would silently move the dispatched IsabelleHol subcommand and
    /// desync it from the Python CLI. Each op resolves to a non-empty wire
    /// literal.
    #[test]
    fn isabelle_checker_driver_op_strings() {
        let d = isabelle_checker_driver();
        assert_eq!(d, CheckerDriver::IsabelleHol(IsabelleCheckerDriver));

        assert_eq!(d.compile_node_op(), "isabelle-check-node");
        assert_eq!(d.print_axioms_op(), "isabelle-thm-oracles");
        assert_eq!(d.local_closure_op(), "isabelle-thm-deps");
        assert_eq!(d.materialize_oleans_op(), "isabelle-build-session");
        assert_eq!(d.prepare_compiled_support_op(), "isabelle-build-session");
        assert_eq!(d.sync_tablet_support_op(), "isabelle-sync-session");

        // The `ISABELLE_OP_*` consts are the single source the methods return.
        assert_eq!(d.compile_node_op(), ISABELLE_OP_CHECK_NODE);
        assert_eq!(d.print_axioms_op(), ISABELLE_OP_THM_ORACLES);
        assert_eq!(d.local_closure_op(), ISABELLE_OP_THM_DEPS);
        assert_eq!(d.lean_semantic_payloads_op(), ISABELLE_OP_THM_DEPS);
        assert_eq!(d.materialize_oleans_op(), ISABELLE_OP_BUILD_SESSION);
        assert_eq!(d.prepare_compiled_support_op(), ISABELLE_OP_BUILD_SESSION);
        assert_eq!(d.sync_tablet_support_op(), ISABELLE_OP_SYNC_SESSION);

        // Every op is non-empty.
        for op in [
            d.compile_node_op(),
            d.print_axioms_op(),
            d.local_closure_op(),
            d.lean_semantic_payloads_op(),
            d.materialize_oleans_op(),
            d.prepare_compiled_support_op(),
            d.sync_tablet_support_op(),
        ] {
            assert!(!op.is_empty());
        }

        // The arg-vec shapes: full-probe `[node]` (no `--no-axcheck` — the
        // Isabelle second collector is the `thm_oracles`/`thm_deps` pair, not an
        // axiomatization walk, so `axcheck_enabled` is inert), owner-scan
        // `[node, --scan-only]` (the CLI accepts+ignores `--scan-only`).
        assert_eq!(d.local_closure_args("Foo", true), vec!["Foo".to_string()]);
        assert_eq!(d.local_closure_args("Foo", false), vec!["Foo".to_string()]);
        assert_eq!(
            d.local_closure_scan_args("Foo"),
            vec!["Foo".to_string(), "--scan-only".to_string()]
        );
    }
}

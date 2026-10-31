# Trust-base v2.3 schemas (authoritative for the build)

> W9 / v2.4 amendment (prose GOAL campaigns): `seed-authored-definition-
> manifest.schema.json` gained the OPTIONAL pair `pv_goal_prose_sha256` /
> `pv_goal_prose_utf8` — the prose GOAL referent (digest + verbatim bytes)
> as a member of the authoritative seed record. Coherence (the digest must
> hash the bytes; the pair is all-or-nothing) is enforced by
> `validate_seed_manifest_semantics`; mode-B/math seeds omit the pair and
> stay byte-identical. This is the seed half of the v2.4 format generation
> (plan schema `trellis-campaign-trust-plan/v4`, the `ExecutableDeferred`
> launch-half evaluation contract); v2.3-generation artifacts keep loading
> unchanged.

Everything the trust base pins at compile time lives in
`src/trust_base/schemas/`, inside the crate's manifest root:

- `v2.3/*.schema.json` — the record schemas loaded by
  `SchemaRegistry::v1()` (`schema.rs`, via the `schema_source!` macro plus two
  longhand `include_str!` sites). Stage 3 of the PV-framework rebuild (plan
  doc 32) pruned the registry to 8 schemas: the journal family
  (journal-event, journal-event-payload, journal-event-bundle,
  journal-event-policy, journal-commit-receipt, the actor-authentication
  pair, the package-authorization sidecar) and reflection-validation-result
  were deleted with the Q1/Q7 machinery, along with the
  `JOURNAL_EVENT_POLICY` / `*_HASH_FIXTURES` JSON files their tests
  consumed (the surviving fixture assertions are constructed inline).

These are `include_str!`ed with paths **relative to the including source
file**. That is deliberate: the schema bytes are part of the trust
base's TCB, so the pin has to be a build-time guarantee, and it has to hold for
a plain `git clone` of this repository with nothing else on disk.

## Rules

1. Never reference these through `env!("CARGO_MANIFEST_DIR")` and never with a
   `/../` segment. An include that escapes the manifest root makes the crate
   unbuildable from a fresh checkout and breaks `cargo package` / `cargo vendor`
   / out-of-tree builds — and it can be satisfied silently by whatever happens
   to be sitting next to the repo, which is exactly how a whole `master` push
   shipped 37 compile errors that no local build reproduced.
2. These files are copies of the design dossier's
   `schemas-v2.3/` files as amended by the PV-framework rebuild (plan doc
   32). **This copy is authoritative for the build.** The dossier
   (`trust-base-redesign-dossier/`, gitignored, not part of the repository)
   is the design record. If a schema is revised there, the revision only
   takes effect once it is copied here and the affected tests are updated.
3. `DOCUMENT_STATUS.v1.json` from the dossier is deliberately NOT vendored: no
   kernel source references it.
4. `src/trust_base/schemas/` holds `*.schema.json` and nothing else. This
   document lives at `kernel/TRUST_BASE_SCHEMAS.md` rather than beside the
   schemas because the runtime-distribution closure
   (`trust_cli.rs`, `collect_runtime_distribution_sources`) sweeps all of
   `kernel/src` into the reproducible-build receipt the runtime is rebuilt
   from. That closure admits exactly the extensions that decide the binary —
   `rs` and `json` — and hard-errors on anything else, so a stray file can
   neither enter the receipt nor silently escape it. Prose is not a build
   input and does not belong in the receipt; keep it out here.

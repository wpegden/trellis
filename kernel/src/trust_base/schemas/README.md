# Trust-base v2.3 schemas and hash fixtures (authoritative for the build)

Everything the trust base pins at compile time lives here, inside the crate's
manifest root:

- `v2.3/*.schema.json` — the 36 record schemas loaded by
  `SchemaRegistry::v1()` (`schema.rs`, via the `schema_source!` macro plus two
  longhand `include_str!` sites).
- `JOURNAL_EVENT_POLICY.v1.json` — the embedded journal policy record
  (`records.rs`, `JournalPolicy::embedded_v1`).
- `JOURNAL_HASH_FIXTURES.v1.json`, `REGISTRATION_HASH_DAG_FIXTURES.v1.json` —
  the cross-implementation hash vectors asserted by the `*_HASH_FIXTURES`
  tests in `auth.rs`, `basis.rs`, `canonical.rs`, `closure.rs`, `journal.rs`,
  `package.rs`, `pipeline.rs`, `records.rs`, `schema.rs`, `seed.rs` and
  `source_validation.rs`.

These are `include_str!`ed with paths **relative to the including source
file**. That is deliberate: the schema and fixture bytes are part of the trust
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
   `schemas-v2.3/` and `*.v1.json`. **This copy is authoritative for the
   build.** The dossier (`trust-base-redesign-dossier/`, gitignored, not part
   of the repository) is the design record. If a schema is revised there, the
   revision only takes effect once it is copied here and the affected fixture
   tests are regenerated.
3. `DOCUMENT_STATUS.v1.json` from the dossier is deliberately NOT vendored: no
   kernel source references it.

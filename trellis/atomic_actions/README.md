Atomic actions are the only Python-side checker operations still used by the
active trellis runtime.

They are intentionally narrow:

- `lean-compile-node`
  Runs `lake env lean Tablet/<node>.lean` and returns raw exit status/stdout/stderr.
- `prepare-compiled-support`
  Runs the narrow deterministic Lean bootstrap needed for clean bundles:
  `lake exe cache get` followed by `lake build Tablet.Preamble`.
- `lean-build-tablet`
  Runs `lake build Tablet` and returns raw exit status/stdout/stderr.
- `print-axioms`
  Runs `#print axioms <node>` through a temporary repo-local probe file and returns raw exit status/stdout/stderr.
- `sync-tablet-support`
  Writes repo-local support artifacts (`Tablet/INDEX.md`, `Tablet/README.md`, `Tablet/header.tex`) from an explicit kernel-authored render payload.

Everything else is kernel-owned:

- reading Lean/TeX files
- parsing declarations/imports/TeX environments
- scope checking
- axiom allowlist interpretation
- build-output classification
- artifact validation
- worker/reviewer acceptance

These Python commands must only gather facts for Rust. They must not decide
validity or synthesize acceptance errors.

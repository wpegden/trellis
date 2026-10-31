# PV trust, results, and source reproductions

Human ratification, machine proof, automated review, and source execution answer different questions. None substitutes for the others.

| Evidence | What it establishes | What it does not establish |
| --- | --- | --- |
| Modeling human review | The Lean/NL goal pairing, supporting meanings, model-preservation claims, scope and limitations actually presented in its instrument | Goal truth, later semantic changes, or anything omitted from that instrument |
| Correspondence/substantiveness/soundness review | The recorded automated judgment on its versioned subject | Human ratification or Lean proof |
| Lean closure certificate | A checked local proposition under recorded axioms and boundary dependencies | Transitive closure before dependencies close; real-source correspondence |
| Assumption gate | The explicitly presented additional premises | Unpresented changes to goal meaning |
| Conditional ratification | The exact sealed restriction, companion statement and obligations presented | Replacement of independent original proof/disproof; source-bug classification |
| Rust execution | Observed behavior of the pinned crate, test, compiler, configuration and resource-limited execution | General correctness, model preservation, Lean disproof, or bug eligibility |

## Approved meaning

New runs freeze the existing approved-target snapshot at modeling approval. Argument workers use the same native Lean/NL correspondence guard as proof formalization. Approved goal meanings and their semantic dependencies cannot drift while proof sketches and helpers develop. Imported support without individual correspondence baselines remains covered through the approved roots' semantic closures and existing source protection.

Automatic advance to proof formalization retains this snapshot; it does not ratify newly added helpers. A later actual human instrument can extend protection to newly reviewed meanings, such as conditional dependencies, while retaining original protections. Semantic changes require the explicit revision and renewed human-review route; protected reapproval remains available in proof formalization. Proof bodies and explanatory arguments can improve through normal verification without pretending that the human has read every later version.

Historical replay stays unchanged unless explicitly activated at an event-count boundary. Migration must preserve approvals and certificates. For a run already in proof formalization, retain the existing protected snapshot and check original goal pairings and supporting meanings against historical approval. Do not fabricate a newer receipt or apply new review instructions retroactively.

## Disclosure

Expert packages contain `TRUST_SUMMARY.md` and `trust-summary.json`. They separate original direction and conditional companions, index assumptions, model bindings and preservation arguments, and disclose local certificates, axioms, boundary theorems and revalidation flags. This projection does not reimplement completion: the runtime owns freshness, transitive closure and final completion.

A legacy approval boolean does not establish the exact review text. Interpret historical approval using its actual instrument and artifact version; disclose missing historical presentation evidence. Never infer source-bug classification from disproof. One defect can cause several goals to fail; share evidence and distinguish goal counts from causes.

## Durable source witnesses

The existing worker Rust-witness lane remains the route for authenticated correspondence review. `python3 -m scripts.pv_reproduce` optionally replays catalogued corroboration for legacy runs without creating protocol authority. Its catalog is `reference/rust-witnesses/catalog.json`, schema `trellis-pv-reproducer-catalog/v1`.

An executable case records ID, primary target, related goals, `small`/`large` resource class, explanation, witness SHA-256, canonical pinned-crate tree identity, GOAL SHA-256, exact `rustc -vV`, Cargo.lock SHA-256, address-space bound and optional hashed evidence files. Source uses the existing `reference/rust-witnesses/target-<sha256(target_id)>/witness.rs` path. `symbolic` and `historical` entries make no executable claim.

```
python3 -m scripts.pv_reproduce --repo RUN --case CASE_ID \
  --toolchain-root PINNED_TOOLCHAIN --runtime-root OUTPUT
```

Large cases require `--allow-large`. Generate long inputs in the test. Assert the specific failure and include controls so build errors or unrelated panics cannot count as success. Multiple goals may reference one cause and execution.

The sealed offline runner has two-CPU affinity and bounded time, memory, files and output. Preserve requests and results, including failures. If the historical crate lacks Cargo.lock, v2 accepts a hashed supplemental lock, installs it only in the private copy, and labels it as outside historical approval. It cannot replace a pinned lock. The v1 authenticated request retains its strict schema.

`ModelingApproval.tla` is a focused abstraction of the modeling/argument/revision boundary omitted from the larger supervisor model. Its smoke checks approved meaning preservation and absence of implicit re-ratification; it does not prove the whole implementation.

# SEAM-REPAIR DEVIATIONS (trust protocol v1)

A deviation in this run is an audited seam repair: an explicit, authorized change to the pinned crate/model surface that restores the extracted model's fidelity to real Rust at one of three seams.

- **Seam (i)** — a Rust language/compiler guarantee the extraction dropped. Cite the guarantee itself, from the closed list: slice/allocation size ≤ `isize::MAX`, references non-null, valid enum discriminants, `str` is UTF-8, layout/niche invariants, and existence/coverage of valid values on the same basis.
- **Seam (ii)** — an upstream computation step the adaptation removed or replaced. Cite the exact upstream source, and re-attach the deleted step to an executable check: the differential harness and its receipt are mandatory.
- **Seam (iii)** — a denotation cut where spec-side meaning stands in for deleted computation. Cite the denotation through the deleted computation: the argument that the spec-side value equals what the removed code computes.

Every repair declares its adapted paths with before/after digests, the affected Decide targets, and its assessment-tool identity effect: a change touching the pinned crate or the evidence tool rebuilds the tool (it statically links the crate), so the repair re-pins the rebuilt tool digest or explicitly re-attests a byte-identical rebuild.

A repair must be valid on its own and stay within its declared seam. Authorize only when the citation meets its seam's standard and the declared paths and affected targets match the TeX file's argument. Every authorized repair lands in the adaptation ledger and is reviewed at the human gate.

Seam repairs are *unusual*, not normal operation. The pinned surface is the ground truth; repair only a demonstrated divergence from real Rust.
